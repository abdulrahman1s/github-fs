//! On-demand bare clones of GitHub repositories via libgit2.
//!
//! Acts as a parallel data source to the REST/GraphQL path: when a clone for
//! a repo exists, callers may prefer it for tree listings and blob reads
//! (offline-capable, no per-blob HTTP round-trip). When a clone is missing or
//! any libgit2 step fails, the caller falls back to the GitHub API path.
//!
//! Layout on disk: `<cache_dir>/clones/<owner>/<repo>.git` (bare).
//! Repos are fetched single-branch on demand; visiting an additional branch
//! adds it to the same bare repo's object DB via a follow-up fetch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use git2::{Cred, FetchOptions, ObjectType, Oid, RemoteCallbacks, Repository};

use crate::cache::CacheError;
use crate::config::token::Token;
use crate::github::{Tree, TreeEntry, TreeEntryKind};

const GITHUB_HTTPS_BASE: &str = "https://github.com";

type BranchKey = (String, String, String);
type BranchLocks = Mutex<HashMap<BranchKey, Arc<Mutex<()>>>>;

pub struct CloneStore {
    root: PathBuf,
    token: Token,
    /// One Mutex per (owner, repo, branch) — serializes fetches and worktree
    /// materializations for the same branch. Different branches of the same
    /// repo can proceed concurrently. Read-only ops (`find_blob`,
    /// `find_commit`, tree walks) bypass this lock entirely.
    locks: BranchLocks,
}

impl CloneStore {
    pub fn open<P: AsRef<Path>>(root: P, token: Token) -> Result<Self, CacheError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            token,
            locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the bare repo for `<owner>/<repo>` (may not exist yet).
    pub fn repo_path(&self, owner: &str, repo: &str) -> PathBuf {
        self.root.join(owner).join(format!("{repo}.git"))
    }

    /// Path of the per-branch worktree dir for `<owner>/<repo>/<fs_name>`
    /// (may not exist yet). `fs_name` is the same `%`-encoded branch name
    /// used as the FUSE entry, so it's always a single path component.
    pub fn worktree_path(&self, owner: &str, repo: &str, fs_name: &str) -> PathBuf {
        self.root.join(owner).join(repo).join(fs_name)
    }

    /// True iff a bare clone for this repo exists on disk.
    pub fn has_clone(&self, owner: &str, repo: &str) -> bool {
        self.repo_path(owner, repo).join("HEAD").exists()
    }

    /// True iff a materialized worktree exists for `<owner>/<repo>/<fs_name>`.
    ///
    /// `fs_name` is the `%`-encoded branch name used as the directory basename
    /// (same one the FUSE layer uses as the entry name). The FS layer uses
    /// this to decide whether to passthrough that branch's ops to the
    /// on-disk worktree — once a worktree exists, the mount serves files
    /// from disk regardless of the configured clone trigger.
    pub fn has_worktree(&self, owner: &str, repo: &str, fs_name: &str) -> bool {
        self.worktree_path(owner, repo, fs_name).is_dir()
    }

    /// Resolve the tip commit for `branch` in the bare clone. Pure object-db
    /// read (no network); errors if the branch ref isn't present locally.
    pub fn branch_tip(&self, owner: &str, repo: &str, branch: &str) -> Result<Oid, CacheError> {
        let repo_handle = Repository::open_bare(self.repo_path(owner, repo))?;
        let reference = repo_handle.find_reference(&format!("refs/heads/{branch}"))?;
        reference
            .target()
            .ok_or_else(|| CacheError::Git(git2::Error::from_str("branch ref has no target oid")))
    }

    fn lock_for(&self, owner: &str, repo: &str, branch: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().expect("CloneStore.locks poisoned");
        locks
            .entry((owner.to_string(), repo.to_string(), branch.to_string()))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Ensure a bare clone exists and `branch` is fetched. Returns the commit
    /// OID at the branch tip.
    ///
    /// Successive calls add more branches to the same bare repo: each call
    /// fetches just the requested branch. Auth uses HTTP basic with
    /// username `x-access-token`, which works for both classic and
    /// fine-grained GitHub PATs.
    pub fn ensure_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        remote_base: &str,
    ) -> Result<Oid, CacheError> {
        let lock = self.lock_for(owner, repo, branch);
        let _guard = lock.lock().expect("CloneStore per-branch lock poisoned");

        let path = self.repo_path(owner, repo);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let repo_handle = if path.join("HEAD").exists() {
            Repository::open_bare(&path)?
        } else {
            Repository::init_bare(&path)?
        };

        let url = format!("{remote_base}/{owner}/{repo}.git");
        let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
        let token = self.token.clone();

        let mut cbs = RemoteCallbacks::new();
        cbs.credentials(move |_url, _username, _allowed| {
            Cred::userpass_plaintext("x-access-token", token.expose())
        });
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(cbs);

        // Anonymous remote: don't persist remote config we'd never use again.
        let mut remote = repo_handle.remote_anonymous(&url)?;
        remote.fetch(&[refspec.as_str()], Some(&mut fo), None)?;

        let reference = repo_handle.find_reference(&format!("refs/heads/{branch}"))?;
        reference
            .target()
            .ok_or_else(|| CacheError::Git(git2::Error::from_str("branch ref has no target oid")))
    }

    /// Return the root tree SHA for the commit at `commit_oid`. Pure object-db
    /// read — no network.
    pub fn commit_tree_sha(
        &self,
        owner: &str,
        repo: &str,
        commit_oid: Oid,
    ) -> Result<String, CacheError> {
        let repo_handle = Repository::open_bare(self.repo_path(owner, repo))?;
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
        let repo_handle = Repository::open_bare(self.repo_path(owner, repo))?;
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
        let repo_handle = Repository::open_bare(self.repo_path(owner, repo))?;
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

    /// Materialize a non-bare worktree for `branch` rooted at the bare repo
    /// for `<owner>/<repo>`. Fetches the branch first if it isn't already in
    /// the object DB. Returns the path to the worktree directory.
    ///
    /// `branch` is the raw branch name (used for the refspec and as the git
    /// internal worktree name). `fs_name` is the `%`-encoded variant used as
    /// the directory basename on disk — keeping these in sync with the FUSE
    /// entry name means the symlink target is a single path component.
    ///
    /// Idempotent: if the worktree directory already exists, returns its path
    /// without touching git. Concurrent calls for the same branch serialize
    /// on the per-`(owner, repo, branch)` lock.
    pub fn ensure_worktree(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        fs_name: &str,
        remote_base: &str,
    ) -> Result<PathBuf, CacheError> {
        // Fast path: worktree already on disk. Skip even the lock — the
        // common steady-state case is "exists already" and we want it to be
        // essentially free.
        let target = self.worktree_path(owner, repo, fs_name);
        if target.exists() {
            return Ok(target);
        }

        let lock = self.lock_for(owner, repo, branch);
        let _guard = lock.lock().expect("CloneStore per-branch lock poisoned");
        if target.exists() {
            // Materialized by another thread while we waited for the lock.
            return Ok(target);
        }

        // Make sure the branch ref is in the bare repo. `ensure_branch`
        // re-acquires the same lock recursively — but `std::sync::Mutex` is
        // not reentrant. To avoid deadlock, do the fetch inline here using
        // the same machinery rather than calling ensure_branch.
        let bare_path = self.repo_path(owner, repo);
        if let Some(parent) = bare_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bare_handle = if bare_path.join("HEAD").exists() {
            Repository::open_bare(&bare_path)?
        } else {
            Repository::init_bare(&bare_path)?
        };
        let url = format!("{remote_base}/{owner}/{repo}.git");
        let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
        let token = self.token.clone();
        let mut cbs = RemoteCallbacks::new();
        cbs.credentials(move |_url, _username, _allowed| {
            Cred::userpass_plaintext("x-access-token", token.expose())
        });
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(cbs);
        let mut remote = bare_handle.remote_anonymous(&url)?;
        remote.fetch(&[refspec.as_str()], Some(&mut fo), None)?;

        // Defensive prune: if the user deleted the worktree dir manually,
        // libgit2 still has a record in `<bare>/worktrees/<name>` and
        // refuses to re-add. `worktree_prune` cleans those orphans.
        for wt_name in bare_handle.worktrees()?.iter().flatten() {
            if let Ok(wt) = bare_handle.find_worktree(wt_name)
                && wt.validate().is_err()
            {
                let mut prune_opts = git2::WorktreePruneOptions::new();
                prune_opts.valid(true).locked(true).working_tree(true);
                let _ = wt.prune(Some(&mut prune_opts));
            }
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let reference = bare_handle.find_reference(&format!("refs/heads/{branch}"))?;
        let mut wt_opts = git2::WorktreeAddOptions::new();
        wt_opts.reference(Some(&reference));
        // git's worktree internal name must be a single component; reuse
        // fs_name so each worktree dir has a 1:1 metadata entry. fs_name is
        // already `%`-encoded so it contains no `/`.
        bare_handle.worktree(fs_name, &target, Some(&wt_opts))?;
        Ok(target)
    }

    /// Read the raw bytes of blob `sha` from the local object DB.
    pub fn read_blob(&self, owner: &str, repo: &str, sha: &str) -> Result<Vec<u8>, CacheError> {
        let repo_handle = Repository::open_bare(self.repo_path(owner, repo))?;
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

        // Build nested src/main.rs subtree first.
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
    fn ensure_branch_fetches_and_resolves_tip_commit() {
        let (_remote, base, expected_commit) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();

        assert!(!store.has_clone("acme", "widgets"));
        let oid = store
            .ensure_branch("acme", "widgets", "main", &base)
            .unwrap();
        assert_eq!(oid, expected_commit);
        assert!(store.has_clone("acme", "widgets"));
        assert!(
            store_dir.path().join("acme").join("widgets.git").exists(),
            "bare clone laid out under root/<owner>/<repo>.git"
        );
    }

    #[test]
    fn build_recursive_tree_matches_github_shape() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_branch("acme", "widgets", "main", &base)
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
            .ensure_branch("acme", "widgets", "main", &base)
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
    fn ensure_branch_is_idempotent_and_adds_branches() {
        let (_remote_dir, base, commit_oid) = setup_remote();
        // Add a second branch on the remote, pointing at the same commit.
        // We look up `refs/heads/main` directly rather than `HEAD` because
        // `init_bare` defaults HEAD to `master` regardless of what branches
        // we created.
        let remote_path = std::path::Path::new(base.trim_start_matches("file://"))
            .join("acme")
            .join("widgets.git");
        let remote = Repository::open_bare(&remote_path).unwrap();
        remote
            .reference("refs/heads/dev", commit_oid, true, "create dev")
            .unwrap();

        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        let main_oid = store
            .ensure_branch("acme", "widgets", "main", &base)
            .unwrap();
        // Re-fetching the same branch is a no-op apart from network — still works.
        let main_oid_again = store
            .ensure_branch("acme", "widgets", "main", &base)
            .unwrap();
        assert_eq!(main_oid, main_oid_again);

        let dev_oid = store
            .ensure_branch("acme", "widgets", "dev", &base)
            .unwrap();
        assert_eq!(dev_oid, commit_oid);
    }

    #[test]
    fn commit_tree_sha_matches_built_tree_sha() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_branch("acme", "widgets", "main", &base)
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
            .ensure_branch("acme", "widgets", "main", &base)
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
    fn has_worktree_tracks_materialization() {
        let (_remote, base, _commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();

        assert!(!store.has_worktree("acme", "widgets", "main"));
        store
            .ensure_worktree("acme", "widgets", "main", "main", &base)
            .unwrap();
        assert!(store.has_worktree("acme", "widgets", "main"));
        // Sibling branches don't accidentally count.
        assert!(!store.has_worktree("acme", "widgets", "dev"));
    }

    #[test]
    fn branch_tip_returns_commit_oid_after_fetch() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        store
            .ensure_worktree("acme", "widgets", "main", "main", &base)
            .unwrap();
        let tip = store.branch_tip("acme", "widgets", "main").unwrap();
        assert_eq!(tip, commit_oid);
    }

    #[test]
    fn ensure_worktree_creates_real_checkout() {
        let (_remote, base, _commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();

        let target = store
            .ensure_worktree("acme", "widgets", "main", "main", &base)
            .unwrap();

        assert!(target.is_dir(), "worktree dir must exist on disk");
        let hello = target.join("hello.txt");
        assert!(hello.exists(), "checked-out file must exist on disk");
        assert_eq!(std::fs::read(&hello).unwrap(), b"hello world");

        let nested = target.join("src").join("main.rs");
        assert_eq!(std::fs::read(&nested).unwrap(), b"fn main() {}");
    }

    #[test]
    fn ensure_worktree_two_branches_share_bare_repo() {
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
            .ensure_worktree("acme", "widgets", "main", "main", &base)
            .unwrap();
        store
            .ensure_worktree("acme", "widgets", "dev", "dev", &base)
            .unwrap();

        // Exactly one bare repo, two worktrees, all under the same owner dir.
        let owner_dir = store_dir.path().join("acme");
        assert!(owner_dir.join("widgets.git").join("HEAD").exists());
        assert!(owner_dir.join("widgets").join("main").is_dir());
        assert!(owner_dir.join("widgets").join("dev").is_dir());
    }

    #[test]
    fn ensure_worktree_is_idempotent() {
        let (_remote, base, _commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();

        let a = store
            .ensure_worktree("acme", "widgets", "main", "main", &base)
            .unwrap();
        let b = store
            .ensure_worktree("acme", "widgets", "main", "main", &base)
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn worktree_target_is_a_real_git_repo() {
        let (_remote, base, commit_oid) = setup_remote();
        let store_dir = tempdir().unwrap();
        let store = CloneStore::open(store_dir.path(), Token::new("ignored")).unwrap();
        let target = store
            .ensure_worktree("acme", "widgets", "main", "main", &base)
            .unwrap();

        // Opening the worktree as a normal (non-bare) git repository must
        // succeed and HEAD must point at the branch tip we asked for.
        let opened = Repository::open(&target).unwrap();
        let head = opened.head().unwrap();
        let head_oid = head.target().unwrap();
        assert_eq!(head_oid, commit_oid);
    }

    #[test]
    fn ensure_worktree_encoded_branch_name_is_one_component() {
        // Branch with `/` in it — fs_name pre-encoded by the FUSE layer.
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
            .ensure_worktree("acme", "widgets", "feature/x", "feature%2Fx", &base)
            .unwrap();

        assert_eq!(
            target,
            store_dir.path().join("acme").join("widgets").join("feature%2Fx")
        );
        assert!(target.is_dir());
    }
}
