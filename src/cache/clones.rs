//! On-demand non-bare clones of GitHub repositories via libgit2.
//!
//! Acts as a parallel data source to the REST/GraphQL path: when a clone for
//! a repo exists, callers may prefer it for tree listings and blob reads
//! (offline-capable, no per-blob HTTP round-trip). When a clone is missing or
//! any libgit2 step fails, the caller falls back to the GitHub API path.
//!
//! Layout on disk: `<cache_dir>/clones/<owner>/<repo>/` (non-bare). One clone
//! per repo, with every branch fetched into `refs/heads/*`, `origin`
//! configured, and upstream tracking set. Whichever branch is requested first
//! is checked out; the user can `git checkout` to switch.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use git2::{BranchType, Cred, FetchOptions, ObjectType, Oid, RemoteCallbacks, Repository};

use crate::cache::CacheError;
use crate::config::{CloneUrlProtocol, token::Token};
use crate::github::{Tree, TreeEntry, TreeEntryKind};

const GITHUB_HTTPS_BASE: &str = "https://github.com";
const GITHUB_SSH_HOST: &str = "github.com";

type RepoKey = (String, String);
type RepoLocks = Mutex<HashMap<RepoKey, Arc<Mutex<()>>>>;

/// Progress events emitted by [`CloneStore::ensure_clone`] during a fresh
/// clone. Callers must throttle their own rendering — `Fetching` fires many
/// times per second while libgit2 streams pack data.
#[derive(Debug, Clone, Copy)]
pub enum CloneProgress {
    Fetching {
        received_objects: usize,
        total_objects: usize,
        indexed_objects: usize,
        received_bytes: usize,
    },
    CheckingOut {
        completed: usize,
        total: usize,
    },
    /// The clone is on disk and the requested branch is checked out.
    Done,
}

/// Callback shape accepted by [`CloneStore::ensure_clone`]. Use
/// `&mut no_progress()` (or `&mut |_| {}`) when you don't care.
pub type ProgressFn<'a> = &'a mut dyn FnMut(CloneProgress);

/// Convenience: a no-op progress sink for tests / callers that just want
/// the side effect of cloning.
pub fn no_progress() -> impl FnMut(CloneProgress) {
    |_| {}
}

pub struct CloneStore {
    root: PathBuf,
    token: Token,
    url_protocol: CloneUrlProtocol,
    /// One Mutex per (owner, repo) — serializes the initial clone of the
    /// same repo. Read-only ops (`find_blob`, `find_commit`, tree walks)
    /// bypass this lock entirely.
    locks: RepoLocks,
}

impl CloneStore {
    pub fn open<P: AsRef<Path>>(root: P, token: Token) -> Result<Self, CacheError> {
        Self::open_with_url_protocol(root, token, CloneUrlProtocol::default())
    }

    pub fn open_with_url_protocol<P: AsRef<Path>>(
        root: P,
        token: Token,
        url_protocol: CloneUrlProtocol,
    ) -> Result<Self, CacheError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            token,
            url_protocol,
            locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the non-bare clone for `<owner>/<repo>` (may not exist yet).
    /// This is the working-tree root; the git dir lives at `<path>/.git`.
    pub fn repo_path(&self, owner: &str, repo: &str) -> PathBuf {
        self.root.join(owner).join(repo)
    }

    /// True iff a clone for this repo exists on disk. Checks for `.git/HEAD`
    /// rather than just the directory so a half-initialized clone (mkdir
    /// without subsequent init) doesn't masquerade as ready.
    pub fn has_clone(&self, owner: &str, repo: &str) -> bool {
        self.repo_path(owner, repo)
            .join(".git")
            .join("HEAD")
            .exists()
    }

    /// Resolve the tip commit for `branch` in the local clone. Pure object-db
    /// read (no network); errors if the branch ref isn't present locally.
    pub fn branch_tip(&self, owner: &str, repo: &str, branch: &str) -> Result<Oid, CacheError> {
        let repo_handle = Repository::open(self.repo_path(owner, repo))?;
        let reference = repo_handle.find_reference(&format!("refs/heads/{branch}"))?;
        reference
            .target()
            .ok_or_else(|| CacheError::Git(git2::Error::from_str("branch ref has no target oid")))
    }

    fn lock_for(&self, owner: &str, repo: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().expect("CloneStore.locks poisoned");
        locks
            .entry((owner.to_string(), repo.to_string()))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Ensure a non-bare clone of `<owner>/<repo>` exists on disk with all
    /// branches fetched and `initial_branch` checked out in the working tree.
    /// Returns the path to the clone (the working-tree root).
    ///
    /// **First call:** `git init` + fetch all branches from
    /// `remote_base/<owner>/<repo>.git`, configure `origin`, then check out
    /// `initial_branch`.
    /// `fetch_depth = Some(n)` makes the fetch shallow (`--depth n`); `None`
    /// fetches full history.
    ///
    /// **Subsequent calls:** no-op — the working tree is the user's now.
    /// Switching branches inside the clone is their responsibility (`git
    /// checkout`); ghfs will not clobber dirty working-tree state.
    ///
    /// The materialization fetch uses HTTP basic with username
    /// `x-access-token`, which works for both classic and fine-grained GitHub
    /// PATs. The persisted `origin` URL follows the configured URL protocol.
    /// Concurrent calls for the same repo serialize on the per-`(owner, repo)`
    /// lock.
    pub fn ensure_clone(
        &self,
        owner: &str,
        repo: &str,
        initial_branch: &str,
        remote_base: &str,
        fetch_depth: Option<u32>,
        progress: ProgressFn<'_>,
    ) -> Result<PathBuf, CacheError> {
        // Fast path: clone already on disk. Skip even the lock — the steady-
        // state case is "exists" and we want it to be essentially free.
        let target = self.repo_path(owner, repo);
        if self.has_clone(owner, repo) {
            return Ok(target);
        }

        let lock = self.lock_for(owner, repo);
        let _guard = lock.lock().expect("CloneStore per-repo lock poisoned");
        if self.has_clone(owner, repo) {
            // Cloned by another thread while we waited for the lock.
            return Ok(target);
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let repo_handle = Repository::init(&target)?;
        let fetch_url = format!("{remote_base}/{owner}/{repo}.git");
        let origin_url = persisted_origin_url(self.url_protocol, owner, repo, &fetch_url);
        let token = self.token.clone();
        // RefCell so both libgit2 callbacks (transfer_progress here,
        // checkout progress below) can mutably borrow the same FnMut without
        // a Mutex. libgit2 invokes them serially on the same thread, so
        // contention is impossible — but the borrow checker can't see that.
        let progress = RefCell::new(progress);

        {
            let mut cbs = RemoteCallbacks::new();
            cbs.credentials(move |_url, _username, _allowed| {
                Cred::userpass_plaintext("x-access-token", token.expose())
            });
            cbs.transfer_progress(|p| {
                if let Ok(mut cb) = progress.try_borrow_mut() {
                    (*cb)(CloneProgress::Fetching {
                        received_objects: p.received_objects(),
                        total_objects: p.total_objects(),
                        indexed_objects: p.indexed_objects(),
                        received_bytes: p.received_bytes(),
                    });
                }
                true
            });
            let mut fo = FetchOptions::new();
            fo.remote_callbacks(cbs);
            if let Some(d) = fetch_depth {
                // i32 cast is safe in practice — depth > i32::MAX would be
                // pathological and libgit2 itself takes i32.
                fo.depth(d as i32);
            }

            // Fetch over the authenticated URL, then leave a normal `origin`
            // behind for the user's future git commands. The extra
            // remote-tracking refspec is what lets plain `git push` and
            // `git pull` infer an upstream branch.
            let mut remote = repo_handle.remote("origin", &fetch_url)?;
            let all_refs = "+refs/heads/*:refs/heads/*";
            let tracking_refs = "+refs/heads/*:refs/remotes/origin/*";
            remote.fetch(&[all_refs, tracking_refs], Some(&mut fo), None)?;
        }

        if origin_url != fetch_url {
            repo_handle.remote_set_url("origin", &origin_url)?;
        }
        configure_branch_upstreams(&repo_handle)?;

        // Set HEAD to the requested branch and materialize its tree in the
        // working directory. `set_head` + `checkout_head` is the libgit2
        // equivalent of `git checkout <branch>` against an empty index.
        let head_ref = format!("refs/heads/{initial_branch}");
        repo_handle.set_head(&head_ref)?;
        let mut co = git2::build::CheckoutBuilder::new();
        co.force();
        co.progress(|_path, completed, total| {
            if let Ok(mut cb) = progress.try_borrow_mut() {
                (*cb)(CloneProgress::CheckingOut { completed, total });
            }
        });
        repo_handle.checkout_head(Some(&mut co))?;

        (*progress.borrow_mut())(CloneProgress::Done);

        Ok(target)
    }

    /// Return the root tree SHA for the commit at `commit_oid`. Pure object-db
    /// read — no network.
    pub fn commit_tree_sha(
        &self,
        owner: &str,
        repo: &str,
        commit_oid: Oid,
    ) -> Result<String, CacheError> {
        let repo_handle = Repository::open(self.repo_path(owner, repo))?;
        let commit = repo_handle.find_commit(commit_oid)?;
        Ok(commit.tree_id().to_string())
    }

    /// Walk the entire tree at `commit_oid` and produce a flat list of entries
    /// in the exact shape `GithubClient::get_tree(.., recursive=true)` returns.
    ///
    /// This lets the FS layer feed the result into the existing
    /// `MetaCache::put_tree` + `TreeIndex` machinery without conditional
    /// branches deeper in the read path.
    pub fn build_recursive_tree(
        &self,
        owner: &str,
        repo: &str,
        commit_oid: Oid,
    ) -> Result<Tree, CacheError> {
        let repo_handle = Repository::open(self.repo_path(owner, repo))?;
        let commit = repo_handle.find_commit(commit_oid)?;
        self.build_tree_from(&repo_handle, &commit.tree()?)
    }

    /// Same as [`build_recursive_tree`] but starts from a tree OID directly
    /// (useful when callers already have a tree SHA, not a commit SHA).
    pub fn build_recursive_tree_from_tree(
        &self,
        owner: &str,
        repo: &str,
        tree_oid: Oid,
    ) -> Result<Tree, CacheError> {
        let repo_handle = Repository::open(self.repo_path(owner, repo))?;
        let tree = repo_handle.find_tree(tree_oid)?;
        self.build_tree_from(&repo_handle, &tree)
    }

    fn build_tree_from(
        &self,
        repo_handle: &Repository,
        tree: &git2::Tree,
    ) -> Result<Tree, CacheError> {
        let root_sha = tree.id().to_string();
        let mut entries: Vec<TreeEntry> = Vec::new();
        walk_tree(repo_handle, tree, "", &mut entries)?;
        Ok(Tree {
            sha: root_sha,
            // The URL field on Tree is informational; the FUSE path doesn't
            // dereference it. Leave empty when synthesised from libgit2.
            url: String::new(),
            truncated: false,
            tree: entries,
        })
    }

    /// Read the raw bytes of blob `sha` from the local object DB.
    pub fn read_blob(&self, owner: &str, repo: &str, sha: &str) -> Result<Vec<u8>, CacheError> {
        let repo_handle = Repository::open(self.repo_path(owner, repo))?;
        let oid = Oid::from_str(sha)?;
        let blob = repo_handle.find_blob(oid)?;
        Ok(blob.content().to_vec())
    }
}

fn walk_tree(
    repo: &Repository,
    tree: &git2::Tree,
    prefix: &str,
    out: &mut Vec<TreeEntry>,
) -> Result<(), CacheError> {
    for entry in tree.iter() {
        let Some(name) = entry.name() else { continue };
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let mode = format!("{:06o}", entry.filemode());
        let sha = entry.id().to_string();
        match entry.kind() {
            Some(ObjectType::Tree) => {
                out.push(TreeEntry {
                    path: path.clone(),
                    mode,
                    kind: TreeEntryKind::Tree,
                    sha,
                    size: None,
                });
                let object = entry.to_object(repo)?;
                let subtree = object.peel_to_tree()?;
                walk_tree(repo, &subtree, &path, out)?;
            }
            Some(ObjectType::Blob) => {
                let size = repo.find_blob(entry.id()).ok().map(|b| b.size() as u64);
                out.push(TreeEntry {
                    path,
                    mode,
                    kind: TreeEntryKind::Blob,
                    sha,
                    size,
                });
            }
            Some(ObjectType::Commit) => {
                // Submodule / gitlink — sha is the embedded commit id.
                out.push(TreeEntry {
                    path,
                    mode,
                    kind: TreeEntryKind::Commit,
                    sha,
                    size: None,
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn persisted_origin_url(
    protocol: CloneUrlProtocol,
    owner: &str,
    repo: &str,
    fetch_url: &str,
) -> String {
    match protocol {
        CloneUrlProtocol::Https => fetch_url.to_owned(),
        CloneUrlProtocol::Ssh => format!("git@{GITHUB_SSH_HOST}:{owner}/{repo}.git"),
    }
}

fn configure_branch_upstreams(repo: &Repository) -> Result<(), git2::Error> {
    for branch in repo.branches(Some(BranchType::Local))? {
        let (mut branch, _) = branch?;
        let Some(name) = branch.name()?.map(str::to_owned) else {
            continue;
        };

        let upstream = format!("origin/{name}");
        if repo.find_branch(&upstream, BranchType::Remote).is_ok() {
            branch.set_upstream(Some(&upstream))?;
        }
    }

    Ok(())
}

/// Default remote base used when cloning. Exposed so tests can point at a
/// local bare-repo `file://` URL.
pub fn default_remote_base() -> String {
    GITHUB_HTTPS_BASE.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Initialise a bare remote with one commit on `main`:
    ///   - `hello.txt` -> "hello world"
    ///   - `src/main.rs` -> "fn main() {}"
    ///
    /// Returns the commit OID at the tip of `main`.
    fn init_test_remote(repo_path: &Path) -> Oid {
        let remote = Repository::init_bare(repo_path).unwrap();
        let sig = git2::Signature::now("ghfs-test", "test@example.com").unwrap();

        let hello_blob = remote.blob(b"hello world").unwrap();
        let main_blob = remote.blob(b"fn main() {}").unwrap();

        let mut src_tb = remote.treebuilder(None).unwrap();
        src_tb.insert("main.rs", main_blob, 0o100644).unwrap();
        let src_tree_oid = src_tb.write().unwrap();

        let mut root_tb = remote.treebuilder(None).unwrap();
        root_tb.insert("hello.txt", hello_blob, 0o100644).unwrap();
        root_tb.insert("src", src_tree_oid, 0o040000).unwrap();
        let tree_oid = root_tb.write().unwrap();
        let tree = remote.find_tree(tree_oid).unwrap();

        remote
            .commit(Some("refs/heads/main"), &sig, &sig, "initial", &tree, &[])
            .unwrap()
    }

    fn setup_remote() -> (tempfile::TempDir, String, Oid) {
        let remote_root = tempdir().unwrap();
        let repo_path = remote_root.path().join("acme").join("widgets.git");
        std::fs::create_dir_all(repo_path.parent().unwrap()).unwrap();
        let commit_oid = init_test_remote(&repo_path);
        let base = format!("file://{}", remote_root.path().display());
        (remote_root, base, commit_oid)
    }

    #[test]
    fn ensure_clone_initializes_repo_and_checks_out_branch() {
        let (_remote, base, expected_commit) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();

        assert!(!store.has_clone("acme", "widgets"));
        let target = store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();

        assert!(store.has_clone("acme", "widgets"));
        assert_eq!(target, store_dir.path().join("acme").join("widgets"));
        assert!(target.join(".git").join("HEAD").exists());
        assert!(target.join("hello.txt").exists());
        assert_eq!(
            std::fs::read(target.join("hello.txt")).unwrap(),
            b"hello world"
        );

        let tip = store.branch_tip("acme", "widgets", "main").unwrap();
        assert_eq!(tip, expected_commit);
    }

    #[test]
    fn ensure_clone_configures_origin_remote_and_upstream() {
        let (_remote, base, _expected_commit) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        let target = store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();

        let opened = Repository::open(&target).unwrap();
        let origin = opened.find_remote("origin").unwrap();
        let expected_url = format!("{base}/acme/widgets.git");
        assert_eq!(origin.url(), Some(expected_url.as_str()));
        assert_eq!(origin.pushurl(), None);

        let branch = opened.find_branch("main", BranchType::Local).unwrap();
        let upstream = branch.upstream().unwrap();
        assert_eq!(upstream.get().name(), Some("refs/remotes/origin/main"));
    }

    #[test]
    fn ensure_clone_can_persist_ssh_origin_url() {
        let (_remote, base, _expected_commit) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open_with_url_protocol(
            store_dir.path(),
            Token::new("ignored"),
            CloneUrlProtocol::Ssh,
        )
        .unwrap();
        let target = store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();

        let opened = Repository::open(&target).unwrap();
        let origin = opened.find_remote("origin").unwrap();
        assert_eq!(origin.url(), Some("git@github.com:acme/widgets.git"));

        let branch = opened.find_branch("main", BranchType::Local).unwrap();
        let upstream = branch.upstream().unwrap();
        assert_eq!(upstream.get().name(), Some("refs/remotes/origin/main"));
    }

    #[test]
    fn ensure_clone_fetches_every_branch() {
        let (_remote_dir, base, commit_oid) = setup_remote();
        // Add a second branch on the remote, pointing at the same commit.
        let remote_path = std::path::Path::new(base.trim_start_matches("file://"))
            .join("acme")
            .join("widgets.git");
        let remote = Repository::open_bare(&remote_path).unwrap();
        remote
            .reference("refs/heads/dev", commit_oid, true, "create dev")
            .unwrap();

        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();

        // Both branches must be present locally after the single clone.
        assert_eq!(
            store.branch_tip("acme", "widgets", "main").unwrap(),
            commit_oid
        );
        assert_eq!(
            store.branch_tip("acme", "widgets", "dev").unwrap(),
            commit_oid
        );
    }

    #[test]
    fn ensure_clone_is_idempotent() {
        let (_remote, base, _commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();

        let a = store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();
        // Touch the working tree — the second call must not clobber it.
        std::fs::write(a.join("hello.txt"), b"user-edited").unwrap();
        let b = store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(
            std::fs::read(b.join("hello.txt")).unwrap(),
            b"user-edited",
            "second ensure_clone must not overwrite user edits"
        );
    }

    #[test]
    fn ensure_clone_with_branch_containing_slash() {
        // Branches like `feature/x` are common; they must end up as
        // `refs/heads/feature/x` and be checkout-able.
        let remote_root = tempdir().unwrap();
        let repo_path = remote_root.path().join("acme").join("widgets.git");
        std::fs::create_dir_all(repo_path.parent().unwrap()).unwrap();
        let commit_oid = init_test_remote(&repo_path);
        let remote = Repository::open_bare(&repo_path).unwrap();
        remote
            .reference("refs/heads/feature/x", commit_oid, true, "branch")
            .unwrap();
        let base = format!("file://{}", remote_root.path().display());

        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        let target = store
            .ensure_clone(
                "acme",
                "widgets",
                "feature/x",
                &base,
                None,
                &mut no_progress(),
            )
            .unwrap();

        // The clone dir basename is still just `widgets` — no encoding of
        // the branch into the path.
        assert_eq!(target, store_dir.path().join("acme").join("widgets"));
        assert!(target.join("hello.txt").exists());
        // Both branches present in refs.
        store.branch_tip("acme", "widgets", "main").unwrap();
        store.branch_tip("acme", "widgets", "feature/x").unwrap();
    }

    #[test]
    fn build_recursive_tree_matches_github_shape() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();

        let tree = store
            .build_recursive_tree("acme", "widgets", commit_oid)
            .unwrap();

        let paths: Vec<&str> = tree.tree.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"hello.txt"));
        assert!(paths.contains(&"src"));
        assert!(paths.contains(&"src/main.rs"));
        assert!(!tree.truncated);
        assert!(!tree.sha.is_empty());

        let hello = tree.tree.iter().find(|e| e.path == "hello.txt").unwrap();
        assert_eq!(hello.kind, TreeEntryKind::Blob);
        assert_eq!(hello.size, Some(b"hello world".len() as u64));

        let src = tree.tree.iter().find(|e| e.path == "src").unwrap();
        assert_eq!(src.kind, TreeEntryKind::Tree);
        assert!(src.size.is_none());
    }

    #[test]
    fn read_blob_returns_raw_bytes() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();
        let tree = store
            .build_recursive_tree("acme", "widgets", commit_oid)
            .unwrap();
        let hello_sha = &tree
            .tree
            .iter()
            .find(|e| e.path == "hello.txt")
            .unwrap()
            .sha;

        let bytes = store.read_blob("acme", "widgets", hello_sha).unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[test]
    fn commit_tree_sha_matches_built_tree_sha() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();
        let tree_sha = store
            .commit_tree_sha("acme", "widgets", commit_oid)
            .unwrap();
        let tree = store
            .build_recursive_tree("acme", "widgets", commit_oid)
            .unwrap();
        assert_eq!(tree.sha, tree_sha);
    }

    #[test]
    fn build_recursive_tree_from_tree_oid_works() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();
        let tree_sha = store
            .commit_tree_sha("acme", "widgets", commit_oid)
            .unwrap();
        let tree_oid = Oid::from_str(&tree_sha).unwrap();

        let tree = store
            .build_recursive_tree_from_tree("acme", "widgets", tree_oid)
            .unwrap();
        assert_eq!(tree.sha, tree_sha);
        assert!(tree.tree.iter().any(|e| e.path == "src/main.rs"));
    }

    #[test]
    fn has_clone_returns_false_before_any_fetch() {
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        assert!(!store.has_clone("anyone", "anything"));
    }

    #[test]
    fn branch_tip_returns_commit_oid_after_clone() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();
        let tip = store.branch_tip("acme", "widgets", "main").unwrap();
        assert_eq!(tip, commit_oid);
    }

    #[test]
    fn cloned_dir_is_a_real_git_repo_with_branch_checked_out() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        let target = store
            .ensure_clone("acme", "widgets", "main", &base, None, &mut no_progress())
            .unwrap();

        // Opening as a normal (non-bare) git repository must succeed; HEAD
        // must point at the branch tip we asked for.
        let opened = Repository::open(&target).unwrap();
        let head = opened.head().unwrap();
        assert_eq!(head.name(), Some("refs/heads/main"));
        assert_eq!(head.target().unwrap(), commit_oid);
    }

    #[test]
    #[ignore = "libgit2's local (file://) transport does not support shallow fetches; \
                run against a real https remote with `cargo test -- --ignored fetch_depth`"]
    fn fetch_depth_produces_shallow_clone() {
        // Build a remote with two commits so a depth=1 fetch is observably
        // different from a full fetch.
        let remote_root = tempdir().unwrap();
        let repo_path = remote_root.path().join("acme").join("widgets.git");
        std::fs::create_dir_all(repo_path.parent().unwrap()).unwrap();
        let remote = Repository::init_bare(&repo_path).unwrap();
        let sig = git2::Signature::now("ghfs-test", "t@e.com").unwrap();
        let blob1 = remote.blob(b"v1").unwrap();
        let mut tb1 = remote.treebuilder(None).unwrap();
        tb1.insert("f.txt", blob1, 0o100644).unwrap();
        let tree1 = remote.find_tree(tb1.write().unwrap()).unwrap();
        let c1 = remote
            .commit(Some("refs/heads/main"), &sig, &sig, "v1", &tree1, &[])
            .unwrap();
        let parent = remote.find_commit(c1).unwrap();
        let blob2 = remote.blob(b"v2").unwrap();
        let mut tb2 = remote.treebuilder(None).unwrap();
        tb2.insert("f.txt", blob2, 0o100644).unwrap();
        let tree2 = remote.find_tree(tb2.write().unwrap()).unwrap();
        let c2 = remote
            .commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "v2",
                &tree2,
                &[&parent],
            )
            .unwrap();
        let base = format!("file://{}", remote_root.path().display());

        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_clone(
                "acme",
                "widgets",
                "main",
                &base,
                Some(1),
                &mut no_progress(),
            )
            .unwrap();

        // The shallow clone has the tip commit but not its parent.
        let opened = Repository::open(store.repo_path("acme", "widgets")).unwrap();
        assert!(opened.find_commit(c2).is_ok(), "tip must be present");
        assert!(
            opened.find_commit(c1).is_err(),
            "shallow clone with depth=1 must not have c1 in the local odb"
        );
    }
}
