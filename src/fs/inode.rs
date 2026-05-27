use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub const FUSE_ROOT_INO: u64 = 1;

/// Identity of a GitHub repository, carried through every per-repo inode so
/// FUSE callbacks can build API URLs without consulting a separate index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub repo_id: u64,
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub enum InodeKind {
    /// The mount point itself; lists owners (users/orgs the token can see
    /// repos under).
    Root,
    /// An owner (user or organization) directory directly under the mount
    /// root. Lists the repos the token can see for that owner. Always a
    /// virtual directory — owner dirs never become writable passthroughs;
    /// the passthrough carve-out applies one level down, per repo.
    Owner { login: String },
    /// A repository directory. Surfaces the content of a single branch — by
    /// default the repo's GitHub-default branch, overridable per repo via
    /// `ghfs branch`. An empty `branch` means no branch could be resolved
    /// (e.g. a freshly-created empty repo with no default); the dir is
    /// surfaced as empty rather than erroring.
    ///
    /// Serves children virtually from the GitHub tree API when no worktree
    /// exists, or passes through to
    /// `<clone_root>/<owner>/<repo>/<fs_name>/` when a worktree has been
    /// materialized. The inode itself is unchanged across that transition —
    /// the FS routes ops dynamically per call, so a cwd inside the repo
    /// dir survives `ghfs promote`.
    Repo { repo: RepoRef, branch: String },
    /// A directory inside a repo. `repo_tree_sha` is the repo's *root*
    /// recursive tree SHA for the virtual case; ignored when the parent
    /// repo has a materialized worktree (passthrough reads disk directly).
    /// Passthrough-allocated dirs store an empty string as a sentinel.
    Dir {
        repo: RepoRef,
        branch: String,
        repo_tree_sha: String,
        path: String,
    },
    /// A regular file. `blob_sha`/`size`/`executable` describe the virtual
    /// view from the GitHub tree; when the parent repo has a worktree,
    /// the file is opened/stat'd directly off disk and these fields are
    /// ignored. Passthrough-allocated files use an empty `blob_sha`.
    File {
        repo: RepoRef,
        branch: String,
        path: String,
        blob_sha: String,
        size: u64,
        executable: bool,
    },
    Symlink {
        repo: RepoRef,
        branch: String,
        path: String,
        blob_sha: String,
        size: u64,
    },
    /// Git submodule (mode 160000). v0.1 surfaces it as an empty directory so
    /// `cd` works and `ls` is empty rather than erroring.
    Submodule {
        repo: RepoRef,
        branch: String,
        path: String,
    },
}

/// Per-session inode allocator. Maps (parent_ino, name) -> stable ino so
/// repeated lookups return the same inode within a mount.
///
/// `by_ino_link` is the reverse of `by_path`: it answers "what is this
/// inode's current parent + name?" and is what makes ino identity survive
/// rename. Passthrough disk-path resolution walks up this map instead of
/// trusting the (potentially stale) `path` field baked into an `InodeKind`
/// at allocation time. See `Ghfs::branch_relative_path`.
pub struct InodeTable {
    next_ino: AtomicU64,
    by_ino: RwLock<HashMap<u64, Arc<InodeKind>>>,
    by_path: RwLock<HashMap<(u64, String), u64>>,
    by_ino_link: RwLock<HashMap<u64, (u64, String)>>,
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeTable {
    pub fn new() -> Self {
        let mut by_ino = HashMap::new();
        by_ino.insert(FUSE_ROOT_INO, Arc::new(InodeKind::Root));
        Self {
            next_ino: AtomicU64::new(FUSE_ROOT_INO + 1),
            by_ino: RwLock::new(by_ino),
            by_path: RwLock::new(HashMap::new()),
            by_ino_link: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, ino: u64) -> Option<Arc<InodeKind>> {
        self.by_ino
            .read()
            .expect("InodeTable.by_ino poisoned")
            .get(&ino)
            .cloned()
    }

    /// Current `(parent_ino, name)` for `ino`. The root has no link.
    /// Source of truth for "where does this inode live now," updated by
    /// `relocate` on rename so disk-path resolution stays correct even
    /// when the kernel holds a stale reference to an inode whose name
    /// has since changed.
    pub fn parent_link(&self, ino: u64) -> Option<(u64, String)> {
        self.by_ino_link
            .read()
            .expect("InodeTable.by_ino_link poisoned")
            .get(&ino)
            .cloned()
    }

    /// Drop the `(parent, name) → ino` mapping. Used by write ops that
    /// delete the underlying disk entry (unlink/rmdir) so a subsequent
    /// create reuses the slot with a fresh ino — kernel page cache is
    /// keyed by inode, and a stale ino would let post-unlink reads see
    /// ghost data.
    ///
    /// The old ino is **not** removed from `by_ino` or `by_ino_link`:
    /// any in-flight FUSE op or open file handle still references it.
    /// We just stop returning it for future name resolutions.
    pub fn evict(&self, parent: u64, name: &str) -> Option<u64> {
        let key = (parent, name.to_string());
        self.by_path
            .write()
            .expect("InodeTable.by_path poisoned")
            .remove(&key)
    }

    /// Move the `(old_parent, old_name) → ino` mapping to
    /// `(new_parent, new_name) → ino`, keeping the same ino. This is
    /// what `rename(2)` looks like from the FS's perspective: the
    /// inode's identity is preserved (matching POSIX semantics — fds
    /// and cached dentries remain valid against the renamed file), only
    /// its reverse link is rewritten. Any inode previously occupying
    /// the destination name is unlinked from `by_path` (its ino survives
    /// in `by_ino` so in-flight ops can finish, but it's no longer
    /// name-addressable).
    ///
    /// Returns the relocated ino, or `None` if no entry existed at the
    /// source.
    pub fn relocate(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Option<u64> {
        let mut by_path = self.by_path.write().expect("InodeTable.by_path poisoned");
        let ino = by_path.remove(&(old_parent, old_name.to_string()))?;
        // If the destination name was already mapped (rename-overwrite),
        // drop that mapping so the renamed ino owns the name now.
        by_path.remove(&(new_parent, new_name.to_string()));
        by_path.insert((new_parent, new_name.to_string()), ino);
        drop(by_path);

        self.by_ino_link
            .write()
            .expect("InodeTable.by_ino_link poisoned")
            .insert(ino, (new_parent, new_name.to_string()));
        Some(ino)
    }

    /// Return the inode for `(parent, name)`, allocating one (via `make`) if
    /// this is the first time we've seen this pair. Uses double-checked
    /// locking: a read-lock fast path, write-lock fallback that re-checks
    /// under the lock so concurrent lookups don't allocate twice.
    pub fn lookup_or_create<F>(&self, parent: u64, name: &str, make: F) -> (u64, Arc<InodeKind>)
    where
        F: FnOnce() -> InodeKind,
    {
        let key = (parent, name.to_string());

        // Read-locked fast path. Release by_path before consulting by_ino.
        let existing = self
            .by_path
            .read()
            .expect("InodeTable.by_path poisoned")
            .get(&key)
            .copied();
        if let Some(ino) = existing
            && let Some(kind) = self.get(ino)
        {
            return (ino, kind);
        }

        // Write-locked slow path. Re-check under the write lock so two callers
        // racing on the same (parent, name) don't both allocate.
        let mut by_path = self.by_path.write().expect("InodeTable.by_path poisoned");
        if let Some(&ino) = by_path.get(&key)
            && let Some(kind) = self.get(ino)
        {
            return (ino, kind);
        }

        let ino = self.next_ino.fetch_add(1, Ordering::SeqCst);
        let kind = Arc::new(make());
        self.by_ino
            .write()
            .expect("InodeTable.by_ino poisoned")
            .insert(ino, kind.clone());
        by_path.insert(key, ino);
        drop(by_path);
        self.by_ino_link
            .write()
            .expect("InodeTable.by_ino_link poisoned")
            .insert(ino, (parent, name.to_string()));
        (ino, kind)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_ino.read().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rref() -> RepoRef {
        RepoRef {
            repo_id: 1,
            owner: "u".into(),
            name: "r".into(),
        }
    }

    #[test]
    fn root_is_preallocated() {
        let t = InodeTable::new();
        let root = t.get(FUSE_ROOT_INO).expect("root must exist");
        assert!(matches!(*root, InodeKind::Root));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn lookup_or_create_allocates_monotonically() {
        let t = InodeTable::new();
        let (a, _) = t.lookup_or_create(FUSE_ROOT_INO, "a", || InodeKind::Repo {
            repo: rref(),
            branch: "main".into(),
        });
        let (b, _) = t.lookup_or_create(FUSE_ROOT_INO, "b", || InodeKind::Repo {
            repo: rref(),
            branch: "main".into(),
        });
        assert!(a >= 2);
        assert_eq!(b, a + 1);
    }

    #[test]
    fn lookup_or_create_is_idempotent_per_parent_name() {
        let t = InodeTable::new();
        let (ino1, _) = t.lookup_or_create(FUSE_ROOT_INO, "repo", || InodeKind::Repo {
            repo: rref(),
            branch: "main".into(),
        });
        // Calling again with the same (parent, name) returns the same ino;
        // crucially, `make` is NOT consulted again (its result is discarded).
        let (ino2, _) = t.lookup_or_create(FUSE_ROOT_INO, "repo", || {
            panic!("factory must not be called on a cache hit");
        });
        assert_eq!(ino1, ino2);
    }

    #[test]
    fn same_name_under_different_parents_gets_different_inos() {
        let t = InodeTable::new();
        let (a, _) = t.lookup_or_create(FUSE_ROOT_INO, "child", || InodeKind::Repo {
            repo: rref(),
            branch: "main".into(),
        });
        let (b, _) = t.lookup_or_create(99, "child", || InodeKind::Repo {
            repo: rref(),
            branch: "main".into(),
        });
        assert_ne!(a, b);
    }

    #[test]
    fn get_returns_kind_after_creation() {
        let t = InodeTable::new();
        let (ino, kind) = t.lookup_or_create(FUSE_ROOT_INO, "f", || InodeKind::File {
            repo: rref(),
            branch: "main".into(),
            path: "f".into(),
            blob_sha: "deadbeef".into(),
            size: 12,
            executable: false,
        });
        let fetched = t.get(ino).unwrap();
        match (&*kind, &*fetched) {
            (InodeKind::File { blob_sha: a, .. }, InodeKind::File { blob_sha: b, .. }) => {
                assert_eq!(a, b);
            }
            _ => panic!("expected matching File variants"),
        }
    }

    #[test]
    fn get_returns_none_for_unknown_ino() {
        let t = InodeTable::new();
        assert!(t.get(9_999_999).is_none());
    }

    #[test]
    fn parent_link_records_alloc_site_and_relocate_updates_it() {
        // This locks in the rename-survives invariant: an inode is
        // reachable through its current (parent, name) via by_path, and
        // the reverse (parent_link) reflects the same. After relocate,
        // both flip together — same ino, new name.
        let t = InodeTable::new();
        let (ino, _) = t.lookup_or_create(FUSE_ROOT_INO, "old", || InodeKind::File {
            repo: rref(),
            branch: "main".into(),
            path: "old".into(),
            blob_sha: String::new(),
            size: 0,
            executable: false,
        });
        assert_eq!(t.parent_link(ino), Some((FUSE_ROOT_INO, "old".into())));

        let relocated = t.relocate(FUSE_ROOT_INO, "old", FUSE_ROOT_INO, "new");
        assert_eq!(relocated, Some(ino), "rename must preserve the ino");
        assert_eq!(t.parent_link(ino), Some((FUSE_ROOT_INO, "new".into())));

        let (ino_again, _) = t.lookup_or_create(FUSE_ROOT_INO, "new", || {
            panic!(
                "factory must not be called: relocated ino must already be cached at the new name"
            );
        });
        assert_eq!(ino_again, ino);
    }

    #[test]
    fn relocate_overwrites_destination() {
        // rename(2) atomically replaces an existing destination; mirror
        // that in by_path so the renamed ino owns the new name and the
        // displaced ino loses its name-addressability.
        let t = InodeTable::new();
        let (src, _) = t.lookup_or_create(FUSE_ROOT_INO, "src", || InodeKind::File {
            repo: rref(),
            branch: "main".into(),
            path: "src".into(),
            blob_sha: String::new(),
            size: 0,
            executable: false,
        });
        let (dst, _) = t.lookup_or_create(FUSE_ROOT_INO, "dst", || InodeKind::File {
            repo: rref(),
            branch: "main".into(),
            path: "dst".into(),
            blob_sha: String::new(),
            size: 0,
            executable: false,
        });
        assert_ne!(src, dst);

        assert_eq!(
            t.relocate(FUSE_ROOT_INO, "src", FUSE_ROOT_INO, "dst"),
            Some(src)
        );

        let (ino_at_dst, _) = t.lookup_or_create(FUSE_ROOT_INO, "dst", || {
            panic!("dst must now resolve to the renamed ino");
        });
        assert_eq!(ino_at_dst, src);

        // The displaced inode still exists in by_ino (in-flight ops are
        // safe) but is no longer reachable by name.
        assert!(t.get(dst).is_some());
    }

    #[test]
    fn relocate_returns_none_when_source_missing() {
        let t = InodeTable::new();
        assert_eq!(
            t.relocate(FUSE_ROOT_INO, "ghost", FUSE_ROOT_INO, "elsewhere"),
            None
        );
    }
}
