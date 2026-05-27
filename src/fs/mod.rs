pub mod attr;
pub mod inode;
pub mod open_files;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, Filesystem, KernelConfig, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow,
};
use libc::c_int;
use tokio::runtime::Handle;
use tracing::{debug, error, info, warn};

use crate::cache::{BlobStore, CloneStore, MetaCache};
use crate::config::CloneTrigger;
use crate::github::{
    Conditional, GithubClient, GithubError, Repo, RepoFilter, Tree, TreeEntry, TreeEntryKind,
};

use self::attr::{
    build_attr, build_attr_from_metadata, is_executable_mode, is_symlink_mode, kind_to_filetype,
};
use self::inode::{FUSE_ROOT_INO, InodeKind, InodeTable, RepoRef};
use self::open_files::OpenFiles;

const TTL: Duration = Duration::from_secs(60);
/// Repo entries use a short TTL so the kernel re-asks promptly after an
/// out-of-band worktree materialization (e.g. `ghfs promote` from another
/// process). The repo *inode* never changes — but its descendants flip
/// between virtual and passthrough, and the kernel's cached child dentries
/// need to expire before the new mode is observed.
const REPO_ENTRY_TTL: Duration = Duration::from_secs(1);
/// Passthrough entries can't safely cache attrs at all: writes through
/// the mount mutate the disk file in place, and the kernel doesn't
/// invalidate attrs on `write` replies. A stale cached size from a
/// post-truncate `setattr` would otherwise persist until the next
/// `lookup`/`getattr` round-trip, making `stat`/`ls -la` show 0 bytes
/// for ~60s after every `O_TRUNC` write. Force a re-ask on every stat.
const ZERO_TTL: Duration = Duration::ZERO;

pub struct Ghfs {
    handle: Handle,
    client: Arc<GithubClient>,
    meta: Arc<MetaCache>,
    blobs: Arc<BlobStore>,
    inodes: Arc<InodeTable>,
    open_files: Arc<OpenFiles>,
    repo_cache: Arc<RwLock<Option<Arc<RepoSnapshot>>>>,
    branch_tree_shas: RwLock<HashMap<(u64, String), BranchTreeSha>>,
    tree_cache: Arc<RwLock<HashMap<String, Arc<TreeIndex>>>>,
    /// Set once per session after the first GraphQL warmup attempt (success
    /// *or* failure). Warmup short-circuits the N+1 REST flow on cold start;
    /// the flag prevents retrying the GraphQL call if it failed once.
    warmed_up: Arc<AtomicBool>,
    /// Serializes the repo-list fetch so a background prefetch and a
    /// foreground FUSE-driven `list_repos` collapse to a single GitHub call:
    /// whichever arrives second waits on the lock, then sees the populated
    /// `repo_cache` and returns it without another fetch.
    fetch_lock: Arc<tokio::sync::Mutex<()>>,
    /// Guards `spawn_background_prefetch` against double-spawning. Set the
    /// first time prefetch is requested; further calls are no-ops.
    prefetch_started: AtomicBool,
    /// Repo-listing filter (`owners`, `include_forks`). Drives the affiliation
    /// query param / GraphQL args at fetch time and the post-fetch trimming;
    /// also selects which ETag cache key to read/write.
    filter: Arc<RepoFilter>,
    /// On-demand libgit2 clone store. `None` when the user disabled cloning
    /// (`[clone] trigger = "never"`); otherwise serves trees/blobs from local
    /// object DB once a clone exists for the repo.
    clone_store: Option<Arc<CloneStore>>,
    clone_trigger: CloneTrigger,
    /// Shallow-clone depth passed to libgit2 on `ensure_clone`. `None` means
    /// full history.
    clone_fetch_depth: Option<u32>,
    /// Base URL used when CloneStore needs to fetch (`https://github.com` in
    /// production; overridden by tests to point at a local `file://` remote).
    clone_remote_base: String,
    /// Tracks repo_ids for which we've already attempted to clone via libgit2
    /// during this session — success or failure. Prevents re-trying every
    /// readdir/open after a clone failed.
    clone_attempted: RwLock<std::collections::HashSet<u64>>,
    /// Single timestamp used for all file attrs in this mount session. GitHub's
    /// tree API doesn't surface per-blob mtimes, so giving everything the same
    /// (mount-time) timestamp is the cheapest sensible answer.
    now: SystemTime,
    uid: u32,
    gid: u32,
}

impl Ghfs {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        handle: Handle,
        client: Arc<GithubClient>,
        meta: Arc<MetaCache>,
        blobs: Arc<BlobStore>,
        filter: RepoFilter,
        clone_store: Option<Arc<CloneStore>>,
        clone_trigger: CloneTrigger,
        clone_fetch_depth: Option<u32>,
        clone_remote_base: String,
    ) -> Self {
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        Self {
            handle,
            client,
            meta,
            blobs,
            inodes: Arc::new(InodeTable::new()),
            open_files: Arc::new(OpenFiles::new()),
            repo_cache: Arc::new(RwLock::new(None)),
            branch_tree_shas: RwLock::new(HashMap::new()),
            tree_cache: Arc::new(RwLock::new(HashMap::new())),
            warmed_up: Arc::new(AtomicBool::new(false)),
            fetch_lock: Arc::new(tokio::sync::Mutex::new(())),
            prefetch_started: AtomicBool::new(false),
            filter: Arc::new(filter),
            clone_store,
            clone_trigger,
            clone_fetch_depth,
            clone_remote_base,
            clone_attempted: RwLock::new(std::collections::HashSet::new()),
            now: SystemTime::now(),
            uid,
            gid,
        }
    }

    /// Stat-equivalent for a FUSE inode. Routes to the on-disk worktree
    /// when one exists for this inode's branch (so size/mtime/perms reflect
    /// real disk state), otherwise builds the virtual attr from the inode
    /// kind's cached fields.
    fn attr(&self, ino: u64, kind: &InodeKind) -> FileAttr {
        if let Some(meta) = self.passthrough_metadata(ino) {
            return build_attr_from_metadata(ino, &meta, self.uid, self.gid);
        }
        build_attr(ino, kind, self.now, self.uid, self.gid)
    }

    /// TTL to attach to attrs/entries we hand back to the kernel.
    /// Mirrors `ttl_for_kind` for virtual inodes, but forces `ZERO_TTL`
    /// whenever the inode resolves through a materialized worktree:
    /// writes via the mount, `git`-side mutations, and external edits
    /// against the on-disk worktree all bypass the kernel's attr cache,
    /// so we can't safely cache disk-backed size/mtime/perm at all. The
    /// extra getattr round-trip per stat is cheap; the alternative is
    /// `stat` returning a 0-byte size for ~60s after every `O_TRUNC`.
    fn ttl_for_attr(&self, ino: u64, kind: &InodeKind) -> &'static Duration {
        if self.passthrough_disk_path(ino).is_some() {
            return &ZERO_TTL;
        }
        ttl_for_kind(kind)
    }

    /// Return the absolute path of the on-disk clone for `repo` iff one has
    /// been materialized. `None` means "no clone yet; serve virtually via
    /// the GitHub API."
    ///
    /// This is the single routing decision that makes the FUSE inode model
    /// stable across `ghfs promote`: the inode kind never changes, but every
    /// op checks this and dispatches to disk vs. virtual per call. The
    /// inode's `branch` is still load-bearing for the *virtual* path (it
    /// selects which branch's tree to fetch from the API), so callers
    /// continue to track it; the passthrough side just uses whatever the
    /// user has currently checked out in the working tree.
    fn clone_root_for(&self, repo: &RepoRef) -> Option<std::path::PathBuf> {
        let store = self.clone_store.as_ref()?;
        if store.has_clone(&repo.owner, &repo.name) {
            Some(store.repo_path(&repo.owner, &repo.name))
        } else {
            None
        }
    }

    /// Project `ino` to `(repo, branch, rel_path_under_repo)` by walking
    /// up through `InodeTable::parent_link` until a `Repo` ancestor with
    /// a resolved branch is reached. `Repo` itself maps to rel_path = "".
    /// `None` means this inode does not sit under a passthrough-eligible
    /// repo (either it's outside the per-repo subtree, or the repo has
    /// an empty branch).
    ///
    /// The walk uses parent links rather than the `path` field on each
    /// kind because that field is captured at allocation time and goes
    /// stale across `rename(2)`. Walking the live linkage keeps disk
    /// resolution correct for the renamed inode AND for any inode
    /// underneath it (whose own names don't change on a parent rename).
    fn branch_relative_path(&self, ino: u64) -> Option<(RepoRef, String, String)> {
        let mut names: Vec<String> = Vec::new();
        let mut cur = ino;
        loop {
            let kind = self.inodes.get(cur)?;
            match &*kind {
                InodeKind::Repo { repo, branch } => {
                    if branch.is_empty() {
                        return None;
                    }
                    let rel = if names.is_empty() {
                        String::new()
                    } else {
                        names.reverse();
                        names.join("/")
                    };
                    return Some((repo.clone(), branch.clone(), rel));
                }
                InodeKind::Root | InodeKind::Owner { .. } => return None,
                _ => {
                    let (parent, name) = self.inodes.parent_link(cur)?;
                    names.push(name);
                    cur = parent;
                }
            }
        }
    }

    /// Disk path for `ino` iff its repo has been materialized as an on-disk
    /// clone. Used by every op that wants to read/list/open the real thing
    /// instead of the virtual GitHub view.
    fn passthrough_disk_path(&self, ino: u64) -> Option<std::path::PathBuf> {
        let (repo, _branch, rel) = self.branch_relative_path(ino)?;
        let root = self.clone_root_for(&repo)?;
        Some(if rel.is_empty() { root } else { root.join(rel) })
    }

    /// `symlink_metadata` against the passthrough disk path, if one exists.
    fn passthrough_metadata(&self, ino: u64) -> Option<std::fs::Metadata> {
        let disk = self.passthrough_disk_path(ino)?;
        std::fs::symlink_metadata(&disk).ok()
    }

    /// Allocate (or reuse) an inode for a real on-disk entry under a
    /// materialized worktree. `parent_rel` is the parent's path relative to
    /// the branch root ("" for entries that sit directly under `Branch`).
    /// SHA fields are left empty as sentinels — virtual-mode fallbacks on
    /// these inodes will surface as IO errors, which is the right answer
    /// for "the worktree this was indexed against is now gone."
    fn passthrough_allocate(
        &self,
        parent_ino: u64,
        name: &str,
        repo: &RepoRef,
        branch: &str,
        parent_rel: &str,
        metadata: &std::fs::Metadata,
    ) -> (u64, Arc<InodeKind>) {
        let full_path = if parent_rel.is_empty() {
            name.to_string()
        } else {
            format!("{parent_rel}/{name}")
        };
        self.inodes.lookup_or_create(parent_ino, name, || {
            let ft = metadata.file_type();
            if ft.is_dir() {
                InodeKind::Dir {
                    repo: repo.clone(),
                    branch: branch.to_string(),
                    repo_tree_sha: String::new(),
                    path: full_path.clone(),
                }
            } else if ft.is_symlink() {
                InodeKind::Symlink {
                    repo: repo.clone(),
                    branch: branch.to_string(),
                    path: full_path.clone(),
                    blob_sha: String::new(),
                    size: metadata.len(),
                }
            } else {
                use std::os::unix::fs::PermissionsExt;
                let executable = metadata.permissions().mode() & 0o111 != 0;
                InodeKind::File {
                    repo: repo.clone(),
                    branch: branch.to_string(),
                    path: full_path.clone(),
                    blob_sha: String::new(),
                    size: metadata.len(),
                    executable,
                }
            }
        })
    }

    /// Resolve `(parent_ino, name)` inside a materialized worktree to a
    /// FUSE inode by stat'ing the disk entry. Returns `Ok(None)` for
    /// ENOENT (the child doesn't exist on disk), `Err` for any other I/O
    /// failure so the FUSE layer can map it to a meaningful errno.
    fn passthrough_lookup_child(
        &self,
        parent_ino: u64,
        name: &str,
        repo: &RepoRef,
        branch: &str,
        parent_rel: &str,
        parent_disk: &std::path::Path,
    ) -> Result<Option<(u64, Arc<InodeKind>)>, GhfsError> {
        let child_disk = parent_disk.join(name);
        let metadata = match std::fs::symlink_metadata(&child_disk) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(GhfsError::Logic(format!(
                    "stat {}: {e}",
                    child_disk.display()
                )));
            }
        };
        Ok(Some(self.passthrough_allocate(
            parent_ino, name, repo, branch, parent_rel, &metadata,
        )))
    }

    /// Resolve `ino` to its passthrough context — the on-disk path plus
    /// the `(repo, branch, rel_path)` triple needed to allocate fresh
    /// inodes for any new children created under it. `None` means this
    /// inode does not sit under a materialized worktree, which is the
    /// signal write ops use to short-circuit with `EROFS`.
    fn passthrough_ctx(&self, ino: u64) -> Option<(std::path::PathBuf, RepoRef, String, String)> {
        let (repo, branch, rel) = self.branch_relative_path(ino)?;
        let disk = self.passthrough_disk_path(ino)?;
        Some((disk, repo, branch, rel))
    }

    /// `read_dir` against a materialized worktree directory and allocate
    /// FUSE inodes for each entry. Entries whose names aren't valid UTF-8
    /// are skipped (FUSE wants `&str` and our inode table is keyed by
    /// `String`); that's the same compromise we already make virtually.
    fn passthrough_collect_children(
        &self,
        parent_ino: u64,
        repo: &RepoRef,
        branch: &str,
        parent_rel: &str,
        parent_disk: &std::path::Path,
    ) -> Result<Vec<DirChild>, GhfsError> {
        let read_dir = std::fs::read_dir(parent_disk)
            .map_err(|e| GhfsError::Logic(format!("read_dir {}: {e}", parent_disk.display())))?;
        let mut out = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|e| {
                GhfsError::Logic(format!("read_dir entry in {}: {e}", parent_disk.display()))
            })?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(GhfsError::Logic(format!(
                        "metadata {}: {e}",
                        entry.path().display()
                    )));
                }
            };
            let (ino, kind) =
                self.passthrough_allocate(parent_ino, &name, repo, branch, parent_rel, &metadata);
            out.push(DirChild { ino, kind, name });
        }
        Ok(out)
    }

    /// After a worktree has been materialized for a branch, derive the
    /// branch-head metadata from the local object DB and write it through
    /// `MetaCache::put_branch_head`. Lets the rest of the session avoid the
    /// GitHub API for this branch's tree resolution.
    fn seed_branch_head_from_clone(&self, repo: &RepoRef, branch: &str) {
        let Some(store) = self.clone_store.as_ref() else {
            return;
        };
        let commit_oid = match store.branch_tip(&repo.owner, &repo.name, branch) {
            Ok(o) => o,
            Err(e) => {
                warn!(error = %e, repo = %repo.name, branch, "seed: branch_tip failed");
                return;
            }
        };
        let tree_sha = match store.commit_tree_sha(&repo.owner, &repo.name, commit_oid) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, repo = %repo.name, branch, "seed: commit_tree_sha failed");
                return;
            }
        };
        if let Err(e) = self.meta.put_branch_head(
            repo.repo_id,
            branch,
            &commit_oid.to_string(),
            &tree_sha,
            None,
        ) {
            warn!(error = %e, repo = %repo.name, branch, "seed: put_branch_head failed");
        }
    }

    /// Resolve the branch the mount should surface under `<mount>/<repo>/`
    /// at the moment a `Repo` inode is being allocated. Precedence:
    ///
    /// 1. A `ghfs branch` override in sqlite (`branch_overrides`).
    /// 2. The repo's GitHub-default branch as cached in the repo list.
    ///
    /// Returns the empty string if neither is set — the mount surfaces an
    /// empty repo dir rather than erroring (e.g. fresh empty repos have no
    /// default and no override). Cache lookups are cheap; we do this once
    /// per repo per session at allocation time.
    fn effective_branch_for(&self, repo_id: u64, default: Option<&str>) -> String {
        match self.meta.get_branch_override(repo_id) {
            Ok(Some(branch)) => branch,
            Ok(None) => default.unwrap_or("").to_string(),
            Err(e) => {
                warn!(error = %e, repo_id, "get_branch_override failed; using default branch");
                default.unwrap_or("").to_string()
            }
        }
    }

    fn path_for_child(&self, parent: &InodeKind, name: &str) -> String {
        let parent_path = path_for_kind(parent);
        if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        }
    }

    // ---- async-via-block_on helpers ----

    /// Return the user's repos. Pinned to a snapshot: once the cache is
    /// populated for this mount we never refetch within the session.
    fn list_repos(&self) -> Result<Arc<RepoSnapshot>, GhfsError> {
        let client = self.client.clone();
        let meta = self.meta.clone();
        let filter = self.filter.clone();
        let repo_cache = Arc::clone(&self.repo_cache);
        let warmed_up = Arc::clone(&self.warmed_up);
        let fetch_lock = Arc::clone(&self.fetch_lock);
        self.handle.block_on(async move {
            list_repos_async(client, meta, filter, repo_cache, warmed_up, fetch_lock).await
        })
    }

    /// Kick off a tokio task that pre-fetches everything `ls` would touch
    /// before the kernel asks for it:
    ///   1. The user's repo list (drives the top-level directory).
    ///   2. The recursive tree at HEAD of each repo's effective branch
    ///      (drives `ls <repo>`).
    ///
    /// Subsequent FUSE-driven calls either hit the populated caches or wait
    /// on `fetch_lock` for the in-flight repo-list prefetch — no duplicate
    /// GitHub call. Tree prefetch is best-effort: errors are logged and the
    /// slow on-demand path still works.
    ///
    /// Idempotent across a mount session via `prefetch_started`.
    pub fn spawn_background_prefetch(&self) {
        if self.prefetch_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let client = self.client.clone();
        let meta = self.meta.clone();
        let filter = self.filter.clone();
        let repo_cache = Arc::clone(&self.repo_cache);
        let warmed_up = Arc::clone(&self.warmed_up);
        let fetch_lock = Arc::clone(&self.fetch_lock);
        let tree_cache = Arc::clone(&self.tree_cache);
        info!("scheduling background prefetch of repo list");
        self.handle.spawn(async move {
            let snapshot = match list_repos_async(
                client.clone(),
                meta.clone(),
                filter,
                repo_cache,
                warmed_up,
                fetch_lock,
            )
            .await
            {
                Ok(s) => {
                    info!(repos = s.len(), "background prefetch: repo list ready");
                    s
                }
                Err(e) => {
                    warn!(error = %e, "background prefetch: repo list failed");
                    return;
                }
            };

            // Phase 2: fan out tree fetches for each repo's effective
            // branch. Cap concurrency so a 500-repo account doesn't open
            // 500 sockets.
            prefetch_default_trees(client, meta, Arc::clone(&tree_cache), Arc::clone(&snapshot))
                .await;
            info!("background prefetch: trees done");
        });
    }

    /// Clonable handle that re-fetches the repo list and swaps the
    /// in-memory snapshot. Used by `spawn_auto_refresh` and by external
    /// triggers (SIGUSR1 from `ghfs refresh`) — both want exactly this
    /// effect with no extra coupling to the rest of `Ghfs`.
    pub fn repo_refresh_handle(&self) -> RepoRefreshHandle {
        RepoRefreshHandle {
            client: self.client.clone(),
            meta: self.meta.clone(),
            filter: self.filter.clone(),
            repo_cache: Arc::clone(&self.repo_cache),
            warmed_up: Arc::clone(&self.warmed_up),
            fetch_lock: Arc::clone(&self.fetch_lock),
        }
    }

    /// Spawn a background task that periodically re-fetches the repo list
    /// so repos created on GitHub mid-session appear in the mount without
    /// a remount. Repeats lean on the ETag stored in `etags`/`branch_heads`
    /// so the steady state is a 304 per tick.
    ///
    /// Only the repo *snapshot* is refreshed — already-allocated inodes
    /// keep their identity, so a shell `cwd` inside a repo is unaffected.
    /// Newly-discovered repos become visible on the next kernel readdir
    /// of the relevant owner dir (subject to the dentry TTL).
    pub fn spawn_auto_refresh(&self, interval: Duration) {
        let handle = self.repo_refresh_handle();
        info!(
            interval_secs = interval.as_secs(),
            "scheduling repo-list auto-refresh"
        );
        self.handle.spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // `interval`'s first tick fires immediately; drop it so we
            // don't double up with the initial prefetch that mount.rs
            // also kicks off at startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match handle.refresh_now().await {
                    Ok(n) => debug!(repos = n, "auto-refresh: repo snapshot updated"),
                    Err(e) => warn!(error = %e, "auto-refresh: repo list refresh failed"),
                }
            }
        });
    }

    /// Spawn a task that listens for SIGUSR1 and refreshes the snapshot
    /// each time. `ghfs refresh` sends this signal after writing the
    /// fresh list to sqlite, so the running mount picks it up
    /// immediately without waiting for the auto-refresh tick.
    pub fn spawn_refresh_signal_listener(&self) {
        let handle = self.repo_refresh_handle();
        self.handle.spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigusr1 = match signal(SignalKind::user_defined1()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "failed to install SIGUSR1 handler; `ghfs refresh` cannot push to this mount");
                    return;
                }
            };
            info!("listening for SIGUSR1 to refresh repo list on demand");
            while sigusr1.recv().await.is_some() {
                match handle.refresh_now().await {
                    Ok(n) => info!(repos = n, "SIGUSR1 refresh: repo snapshot updated"),
                    Err(e) => warn!(error = %e, "SIGUSR1 refresh: repo list refresh failed"),
                }
            }
        });
    }

    /// Resolve HEAD to a root tree SHA once per repo/branch in this mount.
    fn repo_tree_sha(&self, repo: &RepoRef, default_branch: &str) -> Result<String, GhfsError> {
        let key = (repo.repo_id, default_branch.to_string());
        if let Some(cached) = self
            .branch_tree_shas
            .read()
            .expect("Ghfs.branch_tree_shas poisoned")
            .get(&key)
            .cloned()
        {
            return branch_cache_result(repo, default_branch, cached);
        }

        // `on_list` trigger: clone the branch and seed branch_heads from the
        // local object DB before falling through to the API path. We do this
        // even on cache hit in sqlite, because the user explicitly asked for
        // a local clone — but only once per (repo,branch) per session.
        if matches!(self.clone_trigger, CloneTrigger::OnList) {
            self.try_clone_repo(repo, default_branch);
        }

        let tree_sha = match self.meta.get_branch_head(repo.repo_id, default_branch)? {
            Some(bh) => {
                debug!(
                    repo = %repo.name,
                    branch = default_branch,
                    tree_sha = %bh.tree_sha,
                    "branch head loaded from sqlite cache"
                );
                bh.tree_sha
            }
            None => {
                debug!(
                    repo = %repo.name,
                    branch = default_branch,
                    "branch head cache miss; fetching from github"
                );
                let client = self.client.clone();
                let owner = repo.owner.clone();
                let name = repo.name.clone();
                let branch_name = default_branch.to_string();
                let result = self.handle.block_on(async move {
                    client.get_branch(&owner, &name, &branch_name, None).await
                });
                let result = match result {
                    Ok(result) => result,
                    Err(GithubError::NotFound) => {
                        debug!(
                            repo = %repo.name,
                            branch = default_branch,
                            "branch head missing; caching miss for this mount"
                        );
                        self.cache_missing_branch(&key);
                        return Err(GhfsError::Github(GithubError::NotFound));
                    }
                    Err(e) => return Err(e.into()),
                };
                let (etag, branch) = match result {
                    Conditional::Modified { etag, body } => (etag, body),
                    Conditional::NotModified => {
                        return Err(GhfsError::Logic(
                            "got 304 from get_branch without sending If-None-Match".into(),
                        ));
                    }
                };
                self.meta.put_branch_head(
                    repo.repo_id,
                    default_branch,
                    &branch.commit.sha,
                    &branch.commit.commit.tree.sha,
                    etag.as_deref(),
                )?;
                branch.commit.commit.tree.sha
            }
        };

        let mut cache = self
            .branch_tree_shas
            .write()
            .expect("Ghfs.branch_tree_shas poisoned");
        if let Some(existing) = cache.get(&key) {
            return branch_cache_result(repo, default_branch, existing.clone());
        }
        cache.insert(key, BranchTreeSha::Found(tree_sha.clone()));
        Ok(tree_sha)
    }

    fn cache_missing_branch(&self, key: &(u64, String)) {
        self.branch_tree_shas
            .write()
            .expect("Ghfs.branch_tree_shas poisoned")
            .entry(key.clone())
            .or_insert(BranchTreeSha::Missing);
    }

    /// Fetch (or load from cache) the *full recursive tree* at HEAD of
    /// `default_branch`, then index it for hot FUSE lookup/readdir paths.
    fn get_repo_tree_index(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<Arc<TreeIndex>, GhfsError> {
        let tree_sha = self.repo_tree_sha(repo, branch)?;
        self.get_tree_index(repo, &tree_sha)
    }

    fn get_tree_index(&self, repo: &RepoRef, tree_sha: &str) -> Result<Arc<TreeIndex>, GhfsError> {
        if let Some(index) = self
            .tree_cache
            .read()
            .expect("Ghfs.tree_cache poisoned")
            .get(tree_sha)
            .cloned()
        {
            debug!(
                repo = %repo.name,
                tree_sha,
                entries = index.len(),
                "tree index cache hit"
            );
            return Ok(index);
        }

        if let Some(tree) = self.meta.get_tree(tree_sha)? {
            debug!(
                repo = %repo.name,
                tree_sha,
                entries = tree.tree.len(),
                "tree loaded from sqlite cache"
            );
            return Ok(self.cache_tree_index(tree));
        }

        // Clone-backed shortcut: when a clone for this repo already exists,
        // building the recursive tree from the local object DB is faster than
        // the network round-trip. On any libgit2 error we fall through to
        // the GitHub API path so this never *removes* capability.
        if let Some(tree) = self.try_build_tree_from_clone(repo, tree_sha) {
            self.meta.put_tree(&tree)?;
            return Ok(self.cache_tree_index(tree));
        }

        debug!(repo = %repo.name, tree_sha, "tree cache miss; fetching from github");
        let client = self.client.clone();
        let owner = repo.owner.clone();
        let name = repo.name.clone();
        let sha = tree_sha.to_string();
        let result = self
            .handle
            .block_on(async move { client.get_tree(&owner, &name, &sha, true, None).await })?;
        let tree = match result {
            Conditional::Modified { body, .. } => body,
            Conditional::NotModified => {
                return Err(GhfsError::Logic(
                    "got 304 from get_tree without sending If-None-Match".into(),
                ));
            }
        };
        self.meta.put_tree(&tree)?;
        if tree.truncated {
            warn!(
                repo = %repo.name,
                "tree truncated by GitHub (>100k entries or >7MB); v0.1 has no subdir fallback, so some entries will be invisible"
            );
        }
        Ok(self.cache_tree_index(tree))
    }

    fn cache_tree_index(&self, tree: Tree) -> Arc<TreeIndex> {
        let sha = tree.sha.clone();
        let index = Arc::new(TreeIndex::new(tree));
        let mut cache = self.tree_cache.write().expect("Ghfs.tree_cache poisoned");
        if let Some(existing) = cache.get(&sha) {
            debug!(tree_sha = %sha, entries = existing.len(), "tree index filled by another thread");
            return existing.clone();
        }
        debug!(tree_sha = %sha, entries = index.len(), "tree index cached");
        cache.insert(sha, index.clone());
        index
    }

    /// Make sure blob `sha` is on disk; download via GitHub once if needed.
    ///
    /// Prefers the libgit2 clone when one exists for this repo — that read is
    /// a local object-db lookup with no rate-limit cost. Falls back to the
    /// GitHub API on any libgit2 miss/error.
    fn ensure_blob(&self, repo: &RepoRef, sha: &str) -> Result<(), GhfsError> {
        if self.blobs.contains(sha) {
            return Ok(());
        }
        if let Some(bytes) = self.try_read_blob_from_clone(repo, sha) {
            self.blobs.put(sha, &bytes)?;
            return Ok(());
        }
        let client = self.client.clone();
        let owner = repo.owner.clone();
        let name = repo.name.clone();
        let sha_owned = sha.to_string();
        let bytes = self
            .handle
            .block_on(async move { client.get_blob_raw(&owner, &name, &sha_owned).await })?;
        self.blobs.put(sha, &bytes)?;
        Ok(())
    }

    // ---- libgit2 clone integration helpers ----

    /// Clone the whole repo via libgit2 (all branches fetched into
    /// `refs/heads/*`, `branch` checked out in the working tree) and seed the
    /// metadata cache with the resolved tree SHA. No-op when the clone store
    /// is disabled or this repo has already been tried this session. Errors
    /// are logged, not propagated — the API path is the always-available
    /// fallback.
    ///
    /// We materialize a full clone (not a bare repo) so that:
    ///   - blob reads in this session can be served from the local object DB
    ///     via `try_read_blob_from_clone`,
    ///   - the user sees actual files at `<cache>/clones/<owner>/<repo>/`
    ///     immediately after the trigger fires (matches the user's mental
    ///     model of "cloning"),
    ///   - and FUSE ops on the repo start passing through to that working
    ///     tree (writes, reads, stat) on the next `lookup`.
    fn try_clone_repo(&self, repo: &RepoRef, branch: &str) {
        let Some(store) = &self.clone_store else {
            return;
        };
        {
            let attempted = self
                .clone_attempted
                .read()
                .expect("Ghfs.clone_attempted poisoned");
            if attempted.contains(&repo.repo_id) {
                return;
            }
        }
        // Mark attempted before doing the work so a panic mid-fetch doesn't
        // loop forever on retries; libgit2 errors are logged below.
        self.clone_attempted
            .write()
            .expect("Ghfs.clone_attempted poisoned")
            .insert(repo.repo_id);

        info!(
            repo = %repo.name,
            branch,
            trigger = ?self.clone_trigger,
            fetch_depth = ?self.clone_fetch_depth,
            "cloning repo"
        );
        if let Err(e) = store.ensure_clone(
            &repo.owner,
            &repo.name,
            branch,
            &self.clone_remote_base,
            self.clone_fetch_depth,
        ) {
            warn!(error = %e, repo = %repo.name, branch, "clone: ensure_clone failed; falling back to GitHub API");
            return;
        }
        info!(repo = %repo.name, branch, "clone ready");
        // Clone is now on disk. Seed branch_heads from the local object DB
        // so the rest of the session avoids the GitHub API for this branch's
        // tree resolution. The FS layer routes ops to the working tree
        // dynamically (see `clone_root_for`), so no inode invalidation is
        // needed — the existing `Repo` inode keeps the same ino and just
        // starts serving from disk.
        self.seed_branch_head_from_clone(repo, branch);
    }

    /// Try to build a recursive Tree from the local clone. Returns `None` if
    /// no clone exists, the store is disabled, or libgit2 errors (caller
    /// falls back to GitHub).
    fn try_build_tree_from_clone(&self, repo: &RepoRef, tree_sha: &str) -> Option<Tree> {
        let store = self.clone_store.as_ref()?;
        if !store.has_clone(&repo.owner, &repo.name) {
            return None;
        }
        let oid = match git2::Oid::from_str(tree_sha) {
            Ok(o) => o,
            Err(e) => {
                warn!(error = %e, tree_sha, "clone: invalid tree sha; using API");
                return None;
            }
        };
        match store.build_recursive_tree_from_tree(&repo.owner, &repo.name, oid) {
            Ok(tree) => {
                debug!(
                    repo = %repo.name,
                    tree_sha,
                    entries = tree.tree.len(),
                    "clone built recursive tree from local object DB"
                );
                Some(tree)
            }
            Err(e) => {
                debug!(error = %e, tree_sha, repo = %repo.name, "clone: tree not in local object DB; using API");
                None
            }
        }
    }

    /// Try to read blob bytes from the local clone. Returns `None` if no
    /// clone exists, the blob isn't in the object DB, or libgit2 errors.
    fn try_read_blob_from_clone(&self, repo: &RepoRef, sha: &str) -> Option<Vec<u8>> {
        let store = self.clone_store.as_ref()?;
        if !store.has_clone(&repo.owner, &repo.name) {
            return None;
        }
        match store.read_blob(&repo.owner, &repo.name, sha) {
            Ok(bytes) => {
                debug!(
                    repo = %repo.name,
                    sha,
                    bytes = bytes.len(),
                    "clone served blob from local object DB"
                );
                Some(bytes)
            }
            Err(e) => {
                debug!(error = %e, repo = %repo.name, sha, "clone: blob not in local object DB; using API");
                None
            }
        }
    }

    // ---- inode construction ----

    fn entry_to_inode(
        &self,
        name: &str,
        parent: TreeEntryParent<'_>,
        e: &TreeEntry,
    ) -> (u64, Arc<InodeKind>) {
        let full_path = if parent.path.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", parent.path)
        };
        self.inodes
            .lookup_or_create(parent.ino, name, || match e.kind {
                TreeEntryKind::Tree => InodeKind::Dir {
                    repo: parent.repo.clone(),
                    branch: parent.branch.to_string(),
                    repo_tree_sha: parent.tree_sha.to_string(),
                    path: full_path.clone(),
                },
                TreeEntryKind::Blob if is_symlink_mode(&e.mode) => InodeKind::Symlink {
                    repo: parent.repo.clone(),
                    branch: parent.branch.to_string(),
                    path: full_path.clone(),
                    blob_sha: e.sha.clone(),
                    size: e.size.unwrap_or(0),
                },
                TreeEntryKind::Blob => InodeKind::File {
                    repo: parent.repo.clone(),
                    branch: parent.branch.to_string(),
                    path: full_path.clone(),
                    blob_sha: e.sha.clone(),
                    size: e.size.unwrap_or(0),
                    executable: is_executable_mode(&e.mode),
                },
                TreeEntryKind::Commit => InodeKind::Submodule {
                    repo: parent.repo.clone(),
                    branch: parent.branch.to_string(),
                    path: full_path.clone(),
                },
            })
    }

    // ---- op helpers ----

    fn do_lookup(
        &self,
        parent_ino: u64,
        parent: &InodeKind,
        name: &str,
    ) -> Result<Option<(u64, Arc<InodeKind>)>, GhfsError> {
        match parent {
            InodeKind::Root => {
                if is_local_git_metadata_probe(name) {
                    debug!(path = "/.git", "local git metadata probe -> ENOENT");
                    return Ok(None);
                }
                let repos = self.list_repos()?;
                if !repos.has_owner(name) {
                    return Ok(None);
                }
                let owner_login = name.to_string();
                Ok(Some(self.inodes.lookup_or_create(parent_ino, name, || {
                    InodeKind::Owner { login: owner_login }
                })))
            }
            InodeKind::Owner { login } => {
                if is_local_git_metadata_probe(name) {
                    let path = self.path_for_child(parent, name);
                    debug!(path = %path, "local git metadata probe -> ENOENT");
                    return Ok(None);
                }
                let repos = self.list_repos()?;
                let Some(repo) = repos.get(login, name) else {
                    return Ok(None);
                };
                let rref = RepoRef {
                    repo_id: repo.id,
                    owner: repo.owner.login.clone(),
                    name: repo.name.clone(),
                };
                let branch =
                    self.effective_branch_for(rref.repo_id, repo.default_branch.as_deref());
                // `on_access` trigger: synchronously materialize on first
                // lookup so subsequent ops on this repo pass through to
                // the real worktree. The repo inode itself is allocated
                // unconditionally — no kind flip, so cwd inside the repo
                // dir survives the transition.
                if matches!(self.clone_trigger, CloneTrigger::OnAccess) && !branch.is_empty() {
                    self.try_clone_repo(&rref, &branch);
                }
                let rref_for_make = rref.clone();
                let branch_for_make = branch.clone();
                Ok(Some(self.inodes.lookup_or_create(parent_ino, name, || {
                    InodeKind::Repo {
                        repo: rref_for_make,
                        branch: branch_for_make,
                    }
                })))
            }
            InodeKind::Repo { repo, branch } => {
                // Empty branch sentinel: no default, no override — surface
                // the repo as an empty dir instead of erroring. The user
                // can `ghfs branch <repo> <branch>` and remount to fix.
                if branch.is_empty() {
                    return Ok(None);
                }
                if let Some(clone_root) = self.clone_root_for(repo) {
                    // Materialized clone: passthrough. `.git` is real here,
                    // so skip the virtual-mode probe rejection that would
                    // otherwise hide it from `git status`.
                    return self.passthrough_lookup_child(
                        parent_ino,
                        name,
                        repo,
                        branch,
                        "",
                        &clone_root,
                    );
                }
                if is_local_git_metadata_probe(name) {
                    let path = self.path_for_child(parent, name);
                    debug!(path = %path, "local git metadata probe -> ENOENT");
                    return Ok(None);
                }
                let tree = self.get_repo_tree_index(repo, branch)?;
                let Some(entry) = tree.get(name) else {
                    return Ok(None);
                };
                Ok(Some(self.entry_to_inode(
                    name,
                    TreeEntryParent {
                        ino: parent_ino,
                        repo,
                        branch,
                        tree_sha: tree.sha(),
                        path: "",
                    },
                    entry,
                )))
            }
            InodeKind::Dir {
                repo,
                branch,
                repo_tree_sha,
                path,
            } => {
                if let Some(clone_root) = self.clone_root_for(repo) {
                    let parent_disk = clone_root.join(path);
                    return self.passthrough_lookup_child(
                        parent_ino,
                        name,
                        repo,
                        branch,
                        path,
                        &parent_disk,
                    );
                }
                if is_local_git_metadata_probe(name) {
                    let path = self.path_for_child(parent, name);
                    debug!(path = %path, "local git metadata probe -> ENOENT");
                    return Ok(None);
                }
                let tree = self.get_tree_index(repo, repo_tree_sha)?;
                let target_path = format!("{path}/{name}");
                let Some(entry) = tree.get(&target_path) else {
                    return Ok(None);
                };
                Ok(Some(self.entry_to_inode(
                    name,
                    TreeEntryParent {
                        ino: parent_ino,
                        repo,
                        branch,
                        tree_sha: repo_tree_sha,
                        path,
                    },
                    entry,
                )))
            }
            // Files, symlinks, submodules don't contain anything.
            _ => Ok(None),
        }
    }

    fn collect_children(
        &self,
        parent_ino: u64,
        kind: &InodeKind,
    ) -> Result<Vec<DirChild>, GhfsError> {
        match kind {
            InodeKind::Root => {
                let repos = self.list_repos()?;
                Ok(repos
                    .owners()
                    .map(|login| {
                        let name = login.to_string();
                        let login_for_make = name.clone();
                        let (ino, kind) =
                            self.inodes
                                .lookup_or_create(parent_ino, &name, || InodeKind::Owner {
                                    login: login_for_make,
                                });
                        DirChild { ino, kind, name }
                    })
                    .collect())
            }
            InodeKind::Owner { login } => {
                let repos = self.list_repos()?;
                Ok(repos
                    .repos_for_owner(login)
                    .map(|r| {
                        let rref = RepoRef {
                            repo_id: r.id,
                            owner: r.owner.login.clone(),
                            name: r.name.clone(),
                        };
                        let name = r.name.clone();
                        let branch =
                            self.effective_branch_for(rref.repo_id, r.default_branch.as_deref());
                        let rref_for_make = rref.clone();
                        let branch_for_make = branch.clone();
                        let (ino, kind) =
                            self.inodes
                                .lookup_or_create(parent_ino, &name, || InodeKind::Repo {
                                    repo: rref_for_make,
                                    branch: branch_for_make,
                                });
                        DirChild { ino, kind, name }
                    })
                    .collect())
            }
            InodeKind::Repo { repo, branch } => {
                if branch.is_empty() {
                    return Ok(Vec::new());
                }
                if let Some(clone_root) = self.clone_root_for(repo) {
                    return self.passthrough_collect_children(
                        parent_ino,
                        repo,
                        branch,
                        "",
                        &clone_root,
                    );
                }
                let tree = self.get_repo_tree_index(repo, branch)?;
                let sha = tree.sha();
                Ok(tree
                    .children("")
                    .map(|e| {
                        let name = e.path.clone();
                        let (ino, kind) = self.entry_to_inode(
                            &name,
                            TreeEntryParent {
                                ino: parent_ino,
                                repo,
                                branch,
                                tree_sha: sha,
                                path: "",
                            },
                            e,
                        );
                        DirChild { ino, kind, name }
                    })
                    .collect())
            }
            InodeKind::Dir {
                repo,
                branch,
                repo_tree_sha,
                path,
            } => {
                if let Some(clone_root) = self.clone_root_for(repo) {
                    let parent_disk = clone_root.join(path);
                    return self.passthrough_collect_children(
                        parent_ino,
                        repo,
                        branch,
                        path,
                        &parent_disk,
                    );
                }
                let tree = self.get_tree_index(repo, repo_tree_sha)?;
                let prefix_len = if path.is_empty() { 0 } else { path.len() + 1 };
                Ok(tree
                    .children(path)
                    .map(|e| {
                        let name = e.path[prefix_len..].to_string();
                        let (ino, kind) = self.entry_to_inode(
                            &name,
                            TreeEntryParent {
                                ino: parent_ino,
                                repo,
                                branch,
                                tree_sha: repo_tree_sha,
                                path,
                            },
                            e,
                        );
                        DirChild { ino, kind, name }
                    })
                    .collect())
            }
            // Submodules surface as empty dirs.
            InodeKind::Submodule { .. } => Ok(Vec::new()),
            _ => Ok(Vec::new()),
        }
    }
}

struct DirChild {
    ino: u64,
    kind: Arc<InodeKind>,
    name: String,
}

struct TreeEntryParent<'a> {
    ino: u64,
    repo: &'a RepoRef,
    branch: &'a str,
    tree_sha: &'a str,
    path: &'a str,
}

#[derive(Clone)]
enum BranchTreeSha {
    Found(String),
    Missing,
}

/// Return the user's repos. Pinned to a snapshot: once the cache is
/// populated for this mount we never refetch within the session.
///
/// Async so both the FUSE-driven sync path (via `block_on`) and the
/// background prefetch task (via `handle.spawn`) can share one
/// implementation. The fetch is serialized through `fetch_lock` so a
/// background prefetch already in flight collapses the foreground
/// call onto its result instead of issuing a second GitHub request.
async fn list_repos_async(
    client: Arc<GithubClient>,
    meta: Arc<MetaCache>,
    filter: Arc<RepoFilter>,
    repo_cache: Arc<RwLock<Option<Arc<RepoSnapshot>>>>,
    warmed_up: Arc<AtomicBool>,
    fetch_lock: Arc<tokio::sync::Mutex<()>>,
) -> Result<Arc<RepoSnapshot>, GhfsError> {
    if let Some(snapshot) = repo_cache
        .read()
        .expect("Ghfs.repo_cache poisoned")
        .as_ref()
        .cloned()
    {
        debug!(repos = snapshot.len(), "repo snapshot cache hit");
        return Ok(snapshot);
    }

    let _guard = fetch_lock.lock().await;
    if let Some(snapshot) = repo_cache
        .read()
        .expect("Ghfs.repo_cache poisoned")
        .as_ref()
        .cloned()
    {
        debug!(
            repos = snapshot.len(),
            "repo snapshot filled while waiting on fetch lock"
        );
        return Ok(snapshot);
    }

    debug!("repo snapshot cache miss");
    let repos = load_repos_async(client, meta, filter, warmed_up).await?;
    let snapshot = Arc::new(RepoSnapshot::new(repos));
    debug!(repos = snapshot.len(), "repo snapshot cached");
    *repo_cache.write().expect("Ghfs.repo_cache poisoned") = Some(snapshot.clone());
    Ok(snapshot)
}

async fn load_repos_async(
    client: Arc<GithubClient>,
    meta: Arc<MetaCache>,
    filter: Arc<RepoFilter>,
    warmed_up: Arc<AtomicBool>,
) -> Result<Vec<Repo>, GhfsError> {
    // Try GraphQL warmup once per session. On success this skips REST
    // list_user_repos *and* the N per-repo get_branch calls used to
    // resolve default-branch tree SHAs. On any failure we fall through
    // to the REST path below — the flag is set unconditionally so we
    // don't retry the GraphQL request on every readdir.
    if !warmed_up.swap(true, Ordering::Relaxed)
        && let Some(repos) = try_warmup_async(&client, &meta, &filter).await
    {
        return Ok(repos);
    }

    let etag_key = filter.etag_cache_key();
    let etag = meta.get_etag(etag_key)?;
    let cached = meta.get_repos()?;
    // sqlite cache holds whatever the last fetch wrote; re-apply the
    // current filter so a config tweak takes effect even on a 304 / cache
    // hit without forcing a refetch.
    let cached = {
        let mut filtered = cached;
        filter.retain(&mut filtered);
        filtered
    };
    debug!(
        has_etag = etag.is_some(),
        cached_repos = cached.len(),
        "refreshing repo list from github"
    );
    let result = client.list_user_repos(etag.as_deref(), &filter).await;
    let result = match result {
        Ok(result) => result,
        Err(e) if !cached.is_empty() && e.is_retryable() => {
            warn!(
                error = %e,
                repos = cached.len(),
                "repo list refresh failed; using sqlite cache"
            );
            return Ok(cached);
        }
        Err(e) => return Err(e.into()),
    };
    match result {
        Conditional::NotModified => {
            debug!(repos = cached.len(), "repo list not modified");
            Ok(cached)
        }
        Conditional::Modified {
            etag: new_etag,
            body,
        } => {
            debug!(repos = body.len(), "repo list refreshed");
            meta.put_repos(&body)?;
            if let Some(e) = new_etag {
                meta.put_etag(etag_key, &e)?;
            }
            Ok(body)
        }
    }
}

/// Best-effort GraphQL warmup of the repo list and default-branch heads
/// (plus every branch head the warmup returns, so `ghfs branch` lookups
/// can skip the REST `get_branch` call when the override resolves to a
/// branch we've already seen). Returns `None` if anything goes wrong —
/// the caller falls back to REST.
async fn try_warmup_async(
    client: &GithubClient,
    meta: &MetaCache,
    filter: &RepoFilter,
) -> Option<Vec<Repo>> {
    let warmup = match client.warmup_user_repos(filter).await {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "graphql warmup failed; falling back to REST list_user_repos");
            return None;
        }
    };
    let repos: Vec<Repo> = warmup.iter().map(|w| w.repo.clone()).collect();
    if let Err(e) = meta.put_repos(&repos) {
        warn!(error = %e, "warmup: put_repos failed");
        return None;
    }
    let mut heads_seeded = 0usize;
    for w in &warmup {
        // Seed branch_heads for the default branch *and* every branch the
        // warmup returned. Failures are logged but don't abort warmup —
        // worst case the affected branch falls back to a get_branch call
        // (under `ghfs branch` or a remount that picks up an override).
        let mut all_seeds: Vec<&crate::github::BranchHeadSeed> =
            Vec::with_capacity(w.branches.len() + 1);
        if let Some(head) = &w.default_branch_head {
            all_seeds.push(head);
        }
        for b in &w.branches {
            if let Some(default) = &w.default_branch_head
                && default.branch == b.branch
            {
                // Default branch already in the list — avoid double-write.
                continue;
            }
            all_seeds.push(b);
        }
        for seed in &all_seeds {
            if let Err(e) = meta.put_branch_head(
                w.repo.id,
                &seed.branch,
                &seed.commit_sha,
                &seed.tree_sha,
                None,
            ) {
                warn!(
                    error = %e,
                    repo = %w.repo.name,
                    branch = %seed.branch,
                    "warmup: put_branch_head failed"
                );
            } else {
                heads_seeded += 1;
            }
        }
    }
    debug!(
        repos = repos.len(),
        heads = heads_seeded,
        "graphql warmup seeded repos and branch heads"
    );
    Some(repos)
}

/// Cap concurrent prefetch fan-out so a many-repo account doesn't open
/// hundreds of sockets to GitHub at once. Empirically generous — small
/// enough to be polite, large enough to amortize latency.
const PREFETCH_PARALLELISM: usize = 8;

/// For every repo with a known default-branch tree SHA, populate the
/// in-memory tree index. Reads sqlite first; only falls through to the
/// GitHub API when the tree isn't already cached. Errors per-repo are
/// logged, never propagated — the on-demand `get_tree_index` path
/// remains the always-available fallback.
async fn prefetch_default_trees(
    client: Arc<GithubClient>,
    meta: Arc<MetaCache>,
    tree_cache: Arc<RwLock<HashMap<String, Arc<TreeIndex>>>>,
    snapshot: Arc<RepoSnapshot>,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(PREFETCH_PARALLELISM));
    let mut tasks = tokio::task::JoinSet::new();
    let mut seen_shas: std::collections::HashSet<String> = std::collections::HashSet::new();

    for repo in snapshot.iter() {
        let Some(default_branch) = repo.default_branch.as_deref() else {
            continue;
        };
        let tree_sha = match meta.get_branch_head(repo.id, default_branch) {
            Ok(Some(bh)) => bh.tree_sha,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, repo = %repo.name, "prefetch_trees: get_branch_head failed");
                continue;
            }
        };
        if tree_sha.is_empty() || !seen_shas.insert(tree_sha.clone()) {
            continue;
        }
        if tree_cache
            .read()
            .expect("Ghfs.tree_cache poisoned")
            .contains_key(&tree_sha)
        {
            continue;
        }

        let client = client.clone();
        let meta = meta.clone();
        let tree_cache = Arc::clone(&tree_cache);
        let sem = Arc::clone(&semaphore);
        let owner = repo.owner.login.clone();
        let name = repo.name.clone();
        tasks.spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            // Re-check after waiting for the permit; another task may have
            // raced ahead and already populated the cache for this SHA.
            if tree_cache
                .read()
                .expect("Ghfs.tree_cache poisoned")
                .contains_key(&tree_sha)
            {
                return;
            }
            let tree = match meta.get_tree(&tree_sha) {
                Ok(Some(t)) => t,
                Ok(None) => match client.get_tree(&owner, &name, &tree_sha, true, None).await {
                    Ok(Conditional::Modified { body, .. }) => {
                        if let Err(e) = meta.put_tree(&body) {
                            warn!(error = %e, repo = %name, "prefetch_trees: put_tree failed");
                        }
                        if body.truncated {
                            warn!(
                                repo = %name,
                                "prefetch_trees: tree truncated by GitHub; some entries will be invisible"
                            );
                        }
                        body
                    }
                    Ok(Conditional::NotModified) => return,
                    Err(e) => {
                        warn!(error = %e, repo = %name, "prefetch_trees: get_tree failed");
                        return;
                    }
                },
                Err(e) => {
                    warn!(error = %e, repo = %name, "prefetch_trees: meta.get_tree failed");
                    return;
                }
            };
            let sha = tree.sha.clone();
            let index = Arc::new(TreeIndex::new(tree));
            tree_cache
                .write()
                .expect("Ghfs.tree_cache poisoned")
                .entry(sha)
                .or_insert(index);
        });
    }

    let total = tasks.len();
    while tasks.join_next().await.is_some() {}
    debug!(repos = total, "prefetch_trees: done");
}

fn branch_cache_result(
    repo: &RepoRef,
    default_branch: &str,
    cached: BranchTreeSha,
) -> Result<String, GhfsError> {
    match cached {
        BranchTreeSha::Found(sha) => {
            debug!(
                repo = %repo.name,
                branch = default_branch,
                tree_sha = %sha,
                "branch head session cache hit"
            );
            Ok(sha)
        }
        BranchTreeSha::Missing => {
            debug!(
                repo = %repo.name,
                branch = default_branch,
                "branch head negative cache hit"
            );
            Err(GhfsError::Github(GithubError::NotFound))
        }
    }
}

/// Shared state needed to re-fetch the repo list and swap the live
/// in-memory snapshot. Cheap to clone (all `Arc`s).
#[derive(Clone)]
pub struct RepoRefreshHandle {
    client: Arc<GithubClient>,
    meta: Arc<MetaCache>,
    filter: Arc<RepoFilter>,
    repo_cache: Arc<RwLock<Option<Arc<RepoSnapshot>>>>,
    warmed_up: Arc<AtomicBool>,
    fetch_lock: Arc<tokio::sync::Mutex<()>>,
}

impl RepoRefreshHandle {
    /// Re-fetch from GitHub (ETag-conditional, so a steady-state mount
    /// pays a 304) and replace the in-memory snapshot. Returns the new
    /// snapshot size.
    pub async fn refresh_now(&self) -> Result<usize, GhfsError> {
        let _guard = self.fetch_lock.lock().await;
        let repos = load_repos_async(
            self.client.clone(),
            self.meta.clone(),
            self.filter.clone(),
            self.warmed_up.clone(),
        )
        .await?;
        let snapshot = Arc::new(RepoSnapshot::new(repos));
        let len = snapshot.len();
        *self.repo_cache.write().expect("Ghfs.repo_cache poisoned") = Some(snapshot);
        Ok(len)
    }
}

struct RepoSnapshot {
    repos: Vec<Repo>,
    by_owner_name: HashMap<(String, String), Repo>,
    /// Owner logins in first-seen order so `ls <mount>` is deterministic.
    /// Membership only depends on what's in `repos`; duplicates are dropped
    /// at construction time.
    owners: Vec<String>,
}

impl RepoSnapshot {
    fn new(repos: Vec<Repo>) -> Self {
        let by_owner_name = repos
            .iter()
            .map(|repo| ((repo.owner.login.clone(), repo.name.clone()), repo.clone()))
            .collect();
        let mut owners = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for repo in &repos {
            if seen.insert(repo.owner.login.clone()) {
                owners.push(repo.owner.login.clone());
            }
        }
        Self {
            repos,
            by_owner_name,
            owners,
        }
    }

    fn get(&self, owner: &str, name: &str) -> Option<&Repo> {
        self.by_owner_name
            .get(&(owner.to_string(), name.to_string()))
    }

    fn has_owner(&self, owner: &str) -> bool {
        self.owners.iter().any(|o| o == owner)
    }

    fn owners(&self) -> impl Iterator<Item = &str> {
        self.owners.iter().map(String::as_str)
    }

    fn repos_for_owner<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = &'a Repo> {
        self.repos.iter().filter(move |r| r.owner.login == owner)
    }

    fn iter(&self) -> impl Iterator<Item = &Repo> {
        self.repos.iter()
    }

    fn len(&self) -> usize {
        self.repos.len()
    }
}

struct TreeIndex {
    sha: String,
    by_path: HashMap<String, Arc<TreeEntry>>,
    children_by_parent: HashMap<String, Vec<Arc<TreeEntry>>>,
}

impl TreeIndex {
    fn new(tree: Tree) -> Self {
        let mut by_path = HashMap::with_capacity(tree.tree.len());
        let mut children_by_parent: HashMap<String, Vec<Arc<TreeEntry>>> = HashMap::new();

        for entry in tree.tree {
            let path = entry.path.clone();
            let parent = parent_path(&path).to_string();
            let entry = Arc::new(entry);
            by_path.insert(path, entry.clone());
            children_by_parent.entry(parent).or_default().push(entry);
        }

        Self {
            sha: tree.sha,
            by_path,
            children_by_parent,
        }
    }

    fn sha(&self) -> &str {
        &self.sha
    }

    fn get(&self, path: &str) -> Option<&TreeEntry> {
        self.by_path.get(path).map(|entry| entry.as_ref())
    }

    fn children(&self, parent: &str) -> impl Iterator<Item = &TreeEntry> {
        self.children_by_parent
            .get(parent)
            .into_iter()
            .flat_map(|entries| entries.iter().map(|entry| entry.as_ref()))
    }

    fn len(&self) -> usize {
        self.by_path.len()
    }
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn is_local_git_metadata_probe(name: &str) -> bool {
    name == ".git"
}

/// Pick the kernel entry-cache TTL based on inode kind alone. Repo
/// entries get `REPO_ENTRY_TTL` so an out-of-band clone (e.g. `ghfs
/// promote` from another process) is picked up promptly — the inode
/// itself doesn't flip any more, but the kernel's *next* lookup is what
/// swings the FS into passthrough mode for the rest of the tree. Other
/// entry kinds keep the longer `TTL`.
///
/// Prefer `Ghfs::ttl_for_attr` when an `ino` is available: it can detect
/// passthrough resolution and force `ZERO_TTL` so the kernel doesn't
/// cache disk-backed attrs through out-of-band writes.
fn ttl_for_kind(kind: &InodeKind) -> &'static Duration {
    match kind {
        InodeKind::Repo { .. } => &REPO_ENTRY_TTL,
        _ => &TTL,
    }
}

fn path_for_kind(kind: &InodeKind) -> String {
    match kind {
        InodeKind::Root => "/".to_string(),
        InodeKind::Owner { login } => format!("/{login}"),
        InodeKind::Repo { repo, .. } => format!("/{}/{}", repo.owner, repo.name),
        InodeKind::Dir { repo, path, .. }
        | InodeKind::File { repo, path, .. }
        | InodeKind::Symlink { repo, path, .. }
        | InodeKind::Submodule { repo, path, .. } => {
            format!("/{}/{}/{}", repo.owner, repo.name, path)
        }
    }
}

fn kind_name(kind: &InodeKind) -> &'static str {
    match kind {
        InodeKind::Root => "root",
        InodeKind::Owner { .. } => "owner",
        InodeKind::Repo { .. } => "repo",
        InodeKind::Dir { .. } => "dir",
        InodeKind::File { .. } => "file",
        InodeKind::Symlink { .. } => "symlink",
        InodeKind::Submodule { .. } => "submodule",
    }
}

/// Translate a `std::io::Error` from a passthrough syscall into a FUSE
/// errno. Prefers the raw OS code (so EEXIST stays EEXIST, ENOSPC stays
/// ENOSPC, etc.) and falls back to a coarser mapping for synthetic
/// errors that never came from a syscall.
fn io_to_errno(e: &std::io::Error) -> i32 {
    if let Some(raw) = e.raw_os_error() {
        return raw;
    }
    use std::io::ErrorKind as K;
    match e.kind() {
        K::NotFound => libc::ENOENT,
        K::PermissionDenied => libc::EACCES,
        K::AlreadyExists => libc::EEXIST,
        K::InvalidInput | K::InvalidData => libc::EINVAL,
        K::WouldBlock => libc::EAGAIN,
        K::Interrupted => libc::EINTR,
        K::Unsupported => libc::ENOTSUP,
        _ => libc::EIO,
    }
}

/// Filter a recursive tree to the *direct* children of `path_prefix`.
/// - `path_prefix == ""`  -> entries whose path has no '/' (repo root).
/// - `path_prefix == "src"` -> entries starting with `"src/"` and no further '/'.
#[cfg(test)]
pub(crate) fn direct_children<'a>(tree: &'a Tree, path_prefix: &str) -> Vec<&'a TreeEntry> {
    let needle = if path_prefix.is_empty() {
        String::new()
    } else {
        format!("{path_prefix}/")
    };
    tree.tree
        .iter()
        .filter(|e| {
            if !e.path.starts_with(&needle) {
                return false;
            }
            let rest = &e.path[needle.len()..];
            !rest.is_empty() && !rest.contains('/')
        })
        .collect()
}

// ---- error glue ----

#[derive(Debug)]
pub enum GhfsError {
    Github(crate::github::GithubError),
    Cache(crate::cache::CacheError),
    /// Internal invariant violation. Should be unreachable; we surface as EIO.
    Logic(String),
}

impl From<crate::github::GithubError> for GhfsError {
    fn from(e: crate::github::GithubError) -> Self {
        Self::Github(e)
    }
}
impl From<crate::cache::CacheError> for GhfsError {
    fn from(e: crate::cache::CacheError) -> Self {
        Self::Cache(e)
    }
}

impl std::fmt::Display for GhfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Github(e) => write!(f, "github: {e}"),
            Self::Cache(e) => write!(f, "cache: {e}"),
            Self::Logic(s) => write!(f, "logic: {s}"),
        }
    }
}

impl GhfsError {
    pub fn to_errno(&self) -> i32 {
        use crate::github::GithubError as G;
        match self {
            Self::Github(G::Unauthorized) | Self::Github(G::Forbidden(_)) => libc::EACCES,
            Self::Github(G::RateLimited { .. }) => libc::EAGAIN,
            Self::Github(G::NotFound) => libc::ENOENT,
            Self::Github(_) | Self::Cache(_) | Self::Logic(_) => libc::EIO,
        }
    }
}

// ---- Filesystem impl ----

impl Filesystem for Ghfs {
    fn init(&mut self, _req: &Request<'_>, _cfg: &mut KernelConfig) -> Result<(), c_int> {
        info!(ino_root = FUSE_ROOT_INO, "ghfs mounted; ready to serve");
        Ok(())
    }

    fn destroy(&mut self) {
        info!("ghfs unmounting");
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };

        let Some(parent_kind) = self.inodes.get(parent) else {
            debug!(parent, name, "lookup parent missing");
            reply.error(libc::ENOENT);
            return;
        };
        let path = self.path_for_child(&parent_kind, name);
        debug!(parent, name, path = %path, parent_kind = kind_name(&parent_kind), "lookup");
        match self.do_lookup(parent, &parent_kind, name) {
            Ok(Some((ino, kind))) => {
                debug!(ino, path = %path, kind = kind_name(&kind), "lookup hit");
                let attr = self.attr(ino, &kind);
                reply.entry(self.ttl_for_attr(ino, &kind), &attr, 0);
            }
            Ok(None) => {
                debug!(path = %path, "lookup miss");
                reply.error(libc::ENOENT);
            }
            Err(e) => {
                let errno = e.to_errno();
                error!(parent, name, error = %e, errno, "lookup failed");
                reply.error(errno);
            }
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.inodes.get(ino) {
            Some(kind) => {
                debug!(
                    ino,
                    path = %path_for_kind(&kind),
                    kind = kind_name(&kind),
                    "getattr"
                );
                let attr = self.attr(ino, &kind);
                reply.attr(self.ttl_for_attr(ino, &kind), &attr);
            }
            None => {
                debug!(ino, "getattr missing");
                reply.error(libc::ENOENT);
            }
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let Some(kind) = self.inodes.get(ino) else {
            debug!(ino, flags, "opendir missing");
            reply.error(libc::ENOENT);
            return;
        };
        debug!(
            ino,
            flags,
            path = %path_for_kind(&kind),
            kind = kind_name(&kind),
            "opendir"
        );
        if matches!(
            &*kind,
            InodeKind::Root
                | InodeKind::Owner { .. }
                | InodeKind::Repo { .. }
                | InodeKind::Dir { .. }
                | InodeKind::Submodule { .. }
        ) {
            reply.opened(0, 0);
        } else {
            reply.error(libc::ENOTDIR);
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(kind) = self.inodes.get(ino) else {
            debug!(ino, offset, "readdir missing");
            reply.error(libc::ENOENT);
            return;
        };
        let path = path_for_kind(&kind);
        if offset == 0 {
            info!(ino, path = %path, kind = kind_name(&kind), "listing directory");
        } else {
            debug!(ino, offset, path = %path, kind = kind_name(&kind), "readdir");
        }

        if !matches!(
            &*kind,
            InodeKind::Root
                | InodeKind::Owner { .. }
                | InodeKind::Repo { .. }
                | InodeKind::Dir { .. }
                | InodeKind::Submodule { .. }
        ) {
            reply.error(libc::ENOTDIR);
            return;
        }

        let children = match self.collect_children(ino, &kind) {
            Ok(c) => {
                debug!(ino, path = %path, children = c.len(), "readdir children");
                c
            }
            Err(e) => {
                let errno = e.to_errno();
                error!(ino, error = %e, errno, "readdir failed");
                reply.error(errno);
                return;
            }
        };

        // Synthesise "." and "..". We pass `ino` for ".." too: the kernel does
        // its own parent resolution via lookup() when it actually needs it.
        let mut all: Vec<DirChild> = Vec::with_capacity(children.len() + 2);
        all.push(DirChild {
            ino,
            kind: kind.clone(),
            name: ".".to_string(),
        });
        all.push(DirChild {
            ino,
            kind: kind.clone(),
            name: "..".to_string(),
        });
        all.extend(children);

        let start = offset as usize;
        for (i, child) in all.iter().enumerate().skip(start) {
            // The kernel will pass i+1 back as the next offset.
            if reply.add(
                child.ino,
                (i + 1) as i64,
                kind_to_filetype(&child.kind),
                &child.name,
            ) {
                break; // reply buffer full
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let Some(kind) = self.inodes.get(ino) else {
            debug!(ino, offset, "readdirplus missing");
            reply.error(libc::ENOENT);
            return;
        };
        let path = path_for_kind(&kind);
        if offset == 0 {
            info!(ino, path = %path, kind = kind_name(&kind), "listing directory");
        } else {
            debug!(ino, offset, path = %path, kind = kind_name(&kind), "readdirplus");
        }

        if !matches!(
            &*kind,
            InodeKind::Root
                | InodeKind::Owner { .. }
                | InodeKind::Repo { .. }
                | InodeKind::Dir { .. }
                | InodeKind::Submodule { .. }
        ) {
            reply.error(libc::ENOTDIR);
            return;
        }

        let children = match self.collect_children(ino, &kind) {
            Ok(c) => {
                debug!(ino, path = %path, children = c.len(), "readdirplus children");
                c
            }
            Err(e) => {
                let errno = e.to_errno();
                error!(ino, error = %e, errno, "readdirplus failed");
                reply.error(errno);
                return;
            }
        };

        let mut all: Vec<DirChild> = Vec::with_capacity(children.len() + 2);
        all.push(DirChild {
            ino,
            kind: kind.clone(),
            name: ".".to_string(),
        });
        all.push(DirChild {
            ino,
            kind: kind.clone(),
            name: "..".to_string(),
        });
        all.extend(children);

        let start = offset as usize;
        for (i, child) in all.iter().enumerate().skip(start) {
            let attr = self.attr(child.ino, &child.kind);
            if reply.add(
                child.ino,
                (i + 1) as i64,
                &child.name,
                self.ttl_for_attr(child.ino, &child.kind),
                &attr,
                0,
            ) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let Some(kind) = self.inodes.get(ino) else {
            debug!(ino, flags, "open missing");
            reply.error(libc::ENOENT);
            return;
        };
        info!(
            ino,
            flags,
            path = %path_for_kind(&kind),
            kind = kind_name(&kind),
            "reading file"
        );

        let (repo, branch, blob_sha) = match &*kind {
            InodeKind::File {
                repo,
                branch,
                blob_sha,
                ..
            } => (repo.clone(), branch.clone(), blob_sha.clone()),
            InodeKind::Symlink { .. } => {
                // The kernel uses readlink() for symlinks; open() shouldn't be
                // called here. Surface EINVAL if it ever is.
                reply.error(libc::EINVAL);
                return;
            }
            _ => {
                reply.error(libc::EISDIR);
                return;
            }
        };

        // `on_read` trigger: populate the bare clone for this repo/branch on
        // first file open. Even when the requested blob is already on disk,
        // priming the clone lets sibling-blob reads in this branch skip the
        // GitHub API afterwards.
        if matches!(self.clone_trigger, CloneTrigger::OnRead) {
            self.try_clone_repo(&repo, &branch);
        }

        // Passthrough: when the branch has a materialized worktree, open
        // the real on-disk file with whatever access mode the caller
        // asked for (RDONLY / WRONLY / RDWR). Writes happen via `write_at`
        // on this same handle.
        if let Some(disk) = self.passthrough_disk_path(ino) {
            let accmode = flags & libc::O_ACCMODE;
            let mut opts = std::fs::OpenOptions::new();
            opts.read(accmode != libc::O_WRONLY)
                .write(accmode != libc::O_RDONLY);
            if (flags & libc::O_APPEND) != 0 {
                opts.append(true);
            }
            match opts.open(&disk) {
                Ok(file) => {
                    let fh = self.open_files.insert(file);
                    reply.opened(fh, 0);
                }
                Err(e) => {
                    error!(?e, path = %disk.display(), "passthrough open failed");
                    reply.error(io_to_errno(&e));
                }
            }
            return;
        }

        // Virtual mode: only reads are supported. Writes have nowhere to
        // go (the blob cache is content-addressed by SHA), so the carve-out
        // from AGENTS.md invariant "Default is read-only" applies here.
        if (flags & libc::O_ACCMODE) != libc::O_RDONLY {
            reply.error(libc::EROFS);
            return;
        }

        if let Err(e) = self.ensure_blob(&repo, &blob_sha) {
            let errno = e.to_errno();
            error!(ino, error = %e, errno, "open: ensure_blob failed");
            reply.error(errno);
            return;
        }

        let path = self.blobs.path_for(&blob_sha);
        match std::fs::File::open(&path) {
            Ok(file) => {
                let fh = self.open_files.insert(file);
                reply.opened(fh, 0);
            }
            Err(e) => {
                error!(?e, path = %path.display(), "failed to open blob file on disk");
                reply.error(libc::EIO);
            }
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        use std::os::unix::fs::FileExt;

        let Some(file) = self.open_files.get(fh) else {
            debug!(fh, offset, size, "read missing file handle");
            reply.error(libc::EBADF);
            return;
        };

        let mut buf = vec![0u8; size as usize];
        match file.read_at(&mut buf, offset as u64) {
            Ok(n) => {
                buf.truncate(n);
                reply.data(&buf);
            }
            Err(e) => {
                error!(?e, "read_at failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.open_files.remove(fh);
        debug!(fh, "release");
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        debug!(ino, fh, "releasedir");
        reply.ok();
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let Some(kind) = self.inodes.get(ino) else {
            debug!(ino, "readlink missing");
            reply.error(libc::ENOENT);
            return;
        };
        debug!(
            ino,
            path = %path_for_kind(&kind),
            kind = kind_name(&kind),
            "readlink"
        );
        match &*kind {
            InodeKind::Symlink { repo, blob_sha, .. } => {
                // Passthrough: read the real symlink off disk so the kernel
                // walks to whatever the on-disk worktree currently points at.
                if let Some(disk) = self.passthrough_disk_path(ino) {
                    match std::fs::read_link(&disk) {
                        Ok(target) => reply.data(target.as_os_str().as_encoded_bytes()),
                        Err(e) => {
                            error!(?e, path = %disk.display(), "passthrough readlink failed");
                            reply.error(libc::EIO);
                        }
                    }
                    return;
                }
                let repo = repo.clone();
                let blob_sha = blob_sha.clone();
                if let Err(e) = self.ensure_blob(&repo, &blob_sha) {
                    reply.error(e.to_errno());
                    return;
                }
                match self.blobs.read(&blob_sha) {
                    Ok(bytes) => reply.data(&bytes),
                    Err(_) => reply.error(libc::EIO),
                }
            }
            _ => {
                reply.error(libc::EINVAL);
            }
        }
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // Read-only and mostly cosmetic. namelen=255 matches ext4.
        debug!("statfs");
        reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
    }

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let Some(kind) = self.inodes.get(ino) else {
            debug!(ino, xattr = ?name, size, "getxattr missing");
            reply.error(libc::ENOENT);
            return;
        };
        debug!(
            ino,
            xattr = ?name,
            size,
            path = %path_for_kind(&kind),
            kind = kind_name(&kind),
            "getxattr"
        );
        reply.error(libc::ENODATA);
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: ReplyXattr) {
        let Some(kind) = self.inodes.get(ino) else {
            debug!(ino, size, "listxattr missing");
            reply.error(libc::ENOENT);
            return;
        };
        debug!(
            ino,
            size,
            path = %path_for_kind(&kind),
            kind = kind_name(&kind),
            "listxattr"
        );
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    // ---- write ops (passthrough only) ----
    //
    // Each op gates on `passthrough_ctx`: a materialized branch → forward
    // to `std::fs` against the on-disk worktree, no worktree → `EROFS`.
    // The AGENTS.md invariant "Default is read-only; materialized branches
    // are writable" is enforced exactly here.

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some((parent_disk, repo, branch, parent_rel)) = self.passthrough_ctx(parent) else {
            reply.error(libc::EROFS);
            return;
        };
        let child_disk = parent_disk.join(name_str);
        info!(parent, name = name_str, path = %child_disk.display(), "create");

        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        let accmode = flags & libc::O_ACCMODE;
        opts.read(accmode != libc::O_WRONLY)
            .write(accmode != libc::O_RDONLY)
            .create(true)
            .mode(mode & !umask);
        if (flags & libc::O_EXCL) != 0 {
            opts.create_new(true);
        }
        if (flags & libc::O_APPEND) != 0 {
            opts.append(true);
        }
        if (flags & libc::O_TRUNC) != 0 {
            opts.truncate(true);
        }
        let file = match opts.open(&child_disk) {
            Ok(f) => f,
            Err(e) => {
                error!(?e, path = %child_disk.display(), "create failed");
                reply.error(io_to_errno(&e));
                return;
            }
        };
        let metadata = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                reply.error(io_to_errno(&e));
                return;
            }
        };
        let (ino, kind) =
            self.passthrough_allocate(parent, name_str, &repo, &branch, &parent_rel, &metadata);
        let attr = build_attr_from_metadata(ino, &metadata, self.uid, self.gid);
        let fh = self.open_files.insert(file);
        reply.created(self.ttl_for_attr(ino, &kind), &attr, 0, fh, 0);
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        use std::os::unix::fs::FileExt;
        let Some(file) = self.open_files.get(fh) else {
            debug!(fh, offset, len = data.len(), "write missing fh");
            reply.error(libc::EBADF);
            return;
        };
        match file.write_at(data, offset as u64) {
            Ok(n) => reply.written(n as u32),
            Err(e) => {
                error!(?e, fh, offset, "write_at failed");
                reply.error(io_to_errno(&e));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(kind) = self.inodes.get(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        let want_mutation = mode.is_some() || size.is_some() || atime.is_some() || mtime.is_some();
        if !want_mutation {
            // Pure stat. The kernel can issue setattr with no fields set
            // as a getattr-equivalent; return current attrs.
            let attr = self.attr(ino, &kind);
            reply.attr(self.ttl_for_attr(ino, &kind), &attr);
            return;
        }

        let Some((disk, _, _, _)) = self.passthrough_ctx(ino) else {
            reply.error(libc::EROFS);
            return;
        };
        debug!(ino, path = %disk.display(), ?size, ?mode, "setattr");

        if let Some(new_size) = size {
            let res = std::fs::OpenOptions::new()
                .write(true)
                .open(&disk)
                .and_then(|f| f.set_len(new_size));
            if let Err(e) = res {
                error!(?e, path = %disk.display(), "truncate failed");
                reply.error(io_to_errno(&e));
                return;
            }
        }

        if let Some(new_mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            let perm = std::fs::Permissions::from_mode(new_mode & 0o7777);
            if let Err(e) = std::fs::set_permissions(&disk, perm) {
                error!(?e, path = %disk.display(), "chmod failed");
                reply.error(io_to_errno(&e));
                return;
            }
        }

        if atime.is_some() || mtime.is_some() {
            // utimensat lets us set atime/mtime per-spec while leaving the
            // other untouched (UTIME_OMIT). UTIME_NOW handles "set to current
            // time" without us round-tripping through SystemTime::now.
            let now_spec = libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            };
            let omit_spec = libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            };
            let to_spec = |t: TimeOrNow| -> libc::timespec {
                match t {
                    TimeOrNow::Now => now_spec,
                    TimeOrNow::SpecificTime(st) => {
                        let dur = st
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default();
                        libc::timespec {
                            tv_sec: dur.as_secs() as libc::time_t,
                            tv_nsec: dur.subsec_nanos() as i64,
                        }
                    }
                }
            };
            let times = [
                atime.map(to_spec).unwrap_or(omit_spec),
                mtime.map(to_spec).unwrap_or(omit_spec),
            ];
            let Ok(cstr) = std::ffi::CString::new(disk.as_os_str().as_encoded_bytes()) else {
                reply.error(libc::EINVAL);
                return;
            };
            let rc = unsafe {
                libc::utimensat(
                    libc::AT_FDCWD,
                    cstr.as_ptr(),
                    times.as_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if rc != 0 {
                let e = std::io::Error::last_os_error();
                error!(?e, path = %disk.display(), "utimensat failed");
                reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                return;
            }
        }

        // Reply with post-mutation attrs. The ino is unchanged so the
        // kernel's dentry stays valid.
        let metadata = match std::fs::symlink_metadata(&disk) {
            Ok(m) => m,
            Err(e) => {
                reply.error(io_to_errno(&e));
                return;
            }
        };
        let attr = build_attr_from_metadata(ino, &metadata, self.uid, self.gid);
        reply.attr(self.ttl_for_attr(ino, &kind), &attr);
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some((parent_disk, repo, branch, parent_rel)) = self.passthrough_ctx(parent) else {
            reply.error(libc::EROFS);
            return;
        };
        let child_disk = parent_disk.join(name_str);
        info!(parent, name = name_str, path = %child_disk.display(), "mkdir");

        if let Err(e) = std::fs::create_dir(&child_disk) {
            error!(?e, path = %child_disk.display(), "mkdir failed");
            reply.error(io_to_errno(&e));
            return;
        }
        // Apply the requested mode explicitly. `create_dir` honors the
        // process umask but ignores `mode`; do a follow-up `chmod` so
        // callers that pass an explicit mode (e.g. `mkdir -m 0700`) get
        // the bits they asked for.
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(
            &child_disk,
            std::fs::Permissions::from_mode(mode & !umask & 0o7777),
        ) {
            error!(?e, path = %child_disk.display(), "mkdir chmod failed");
            // The dir exists; don't unwind. Surface the chmod error so the
            // caller can decide.
            reply.error(io_to_errno(&e));
            return;
        }

        let metadata = match std::fs::symlink_metadata(&child_disk) {
            Ok(m) => m,
            Err(e) => {
                reply.error(io_to_errno(&e));
                return;
            }
        };
        let (ino, kind) =
            self.passthrough_allocate(parent, name_str, &repo, &branch, &parent_rel, &metadata);
        let attr = build_attr_from_metadata(ino, &metadata, self.uid, self.gid);
        reply.entry(self.ttl_for_attr(ino, &kind), &attr, 0);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some((parent_disk, _, _, _)) = self.passthrough_ctx(parent) else {
            reply.error(libc::EROFS);
            return;
        };
        let target = parent_disk.join(name_str);
        info!(parent, name = name_str, path = %target.display(), "unlink");

        match std::fs::remove_file(&target) {
            Ok(()) => {
                // Drop the (parent, name) mapping so a subsequent `create`
                // with the same name allocates a fresh ino; otherwise the
                // kernel's page cache (keyed by ino) could surface stale
                // data from the deleted file.
                self.inodes.evict(parent, name_str);
                reply.ok();
            }
            Err(e) => {
                error!(?e, path = %target.display(), "unlink failed");
                reply.error(io_to_errno(&e));
            }
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some((parent_disk, _, _, _)) = self.passthrough_ctx(parent) else {
            reply.error(libc::EROFS);
            return;
        };
        let target = parent_disk.join(name_str);
        info!(parent, name = name_str, path = %target.display(), "rmdir");

        match std::fs::remove_dir(&target) {
            Ok(()) => {
                self.inodes.evict(parent, name_str);
                reply.ok();
            }
            Err(e) => {
                error!(?e, path = %target.display(), "rmdir failed");
                reply.error(io_to_errno(&e));
            }
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let (Some(name_str), Some(newname_str)) = (name.to_str(), newname.to_str()) else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some((old_parent_disk, repo_a, branch_a, _)) = self.passthrough_ctx(parent) else {
            reply.error(libc::EROFS);
            return;
        };
        let Some((new_parent_disk, repo_b, branch_b, _)) = self.passthrough_ctx(newparent) else {
            reply.error(libc::EROFS);
            return;
        };
        // A rename across different repos or branches is two distinct
        // worktrees — there is no single underlying inode to relink.
        // `EXDEV` is the conventional "use copy+delete instead" answer.
        if repo_a.repo_id != repo_b.repo_id || branch_a != branch_b {
            reply.error(libc::EXDEV);
            return;
        }
        let src = old_parent_disk.join(name_str);
        let dst = new_parent_disk.join(newname_str);
        info!(src = %src.display(), dst = %dst.display(), "rename");

        match std::fs::rename(&src, &dst) {
            Ok(()) => {
                // Move the inode in our table rather than evicting both
                // names. POSIX rename preserves the inode of the renamed
                // file: any kernel-cached dentry, open fd, or in-flight
                // FUSE op against the source ino must keep resolving to
                // the same file post-rename. Disk-path resolution walks
                // up via `parent_link`, so the relocated ino now points
                // at `dst` automatically. If there was nothing to
                // relocate (kernel-only rename racing our state), fall
                // back to the conservative double-evict.
                if self
                    .inodes
                    .relocate(parent, name_str, newparent, newname_str)
                    .is_none()
                {
                    self.inodes.evict(newparent, newname_str);
                }
                reply.ok();
            }
            Err(e) => {
                error!(?e, src = %src.display(), dst = %dst.display(), "rename failed");
                reply.error(io_to_errno(&e));
            }
        }
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let Some(name_str) = link_name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some((parent_disk, repo, branch, parent_rel)) = self.passthrough_ctx(parent) else {
            reply.error(libc::EROFS);
            return;
        };
        let link_disk = parent_disk.join(name_str);
        info!(parent, name = name_str, target = %target.display(), "symlink");

        if let Err(e) = std::os::unix::fs::symlink(target, &link_disk) {
            error!(?e, link = %link_disk.display(), "symlink failed");
            reply.error(io_to_errno(&e));
            return;
        }
        let metadata = match std::fs::symlink_metadata(&link_disk) {
            Ok(m) => m,
            Err(e) => {
                reply.error(io_to_errno(&e));
                return;
            }
        };
        let (ino, kind) =
            self.passthrough_allocate(parent, name_str, &repo, &branch, &parent_rel, &metadata);
        let attr = build_attr_from_metadata(ino, &metadata, self.uid, self.gid);
        reply.entry(self.ttl_for_attr(ino, &kind), &attr, 0);
    }

    fn fsync(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        let Some(file) = self.open_files.get(fh) else {
            reply.error(libc::EBADF);
            return;
        };
        let res = if datasync {
            file.sync_data()
        } else {
            file.sync_all()
        };
        match res {
            Ok(()) => reply.ok(),
            Err(e) => {
                error!(?e, fh, "fsync failed");
                reply.error(io_to_errno(&e));
            }
        }
    }

    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // Directory contents are written through synchronously via the
        // per-file write/create/unlink/rename ops, all of which talk to
        // the kernel page cache for the underlying disk. There is nothing
        // dir-level for us to sync.
        reply.ok();
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        // Called on every close(2). For passthrough handles the data has
        // already been forwarded to the disk via `write_at`; nothing to
        // do. Returning success is preferable to the trait default's
        // ENOSYS, which some callers treat as a close-time failure.
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RepoSnapshot, TreeIndex, direct_children, is_local_git_metadata_probe, parent_path,
    };
    use crate::github::{Owner, Repo, Tree, TreeEntry, TreeEntryKind};

    fn repo(id: u64, name: &str) -> Repo {
        Repo {
            id,
            name: name.into(),
            full_name: format!("me/{name}"),
            owner: Owner {
                login: "me".into(),
                id: 1,
            },
            private: false,
            default_branch: Some("main".into()),
            description: None,
            size: 0,
            fork: false,
        }
    }

    fn tree(entries: &[(&str, &str, TreeEntryKind)]) -> Tree {
        Tree {
            sha: "t".into(),
            url: "https://x".into(),
            truncated: false,
            tree: entries
                .iter()
                .map(|(path, mode, kind)| TreeEntry {
                    path: (*path).to_string(),
                    mode: (*mode).to_string(),
                    kind: *kind,
                    sha: "s".into(),
                    size: None,
                })
                .collect(),
        }
    }

    #[test]
    fn repo_snapshot_keeps_order_and_indexes_by_owner_and_name() {
        let snapshot = RepoSnapshot::new(vec![repo(1, "beta"), repo(2, "alpha")]);

        assert_eq!(snapshot.get("me", "alpha").unwrap().id, 2);
        assert!(snapshot.get("me", "missing").is_none());
        assert!(snapshot.get("nobody", "alpha").is_none());

        let names: Vec<&str> = snapshot.iter().map(|repo| repo.name.as_str()).collect();
        assert_eq!(names, vec!["beta", "alpha"]);

        // Both repos share owner "me", so it appears once in `owners()`.
        let owners: Vec<&str> = snapshot.owners().collect();
        assert_eq!(owners, vec!["me"]);
        assert!(snapshot.has_owner("me"));
        assert!(!snapshot.has_owner("someone-else"));

        let me_repos: Vec<&str> = snapshot
            .repos_for_owner("me")
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(me_repos, vec!["beta", "alpha"]);
    }

    #[test]
    fn tree_index_supports_path_lookup_and_direct_children() {
        let index = TreeIndex::new(tree(&[
            ("README.md", "100644", TreeEntryKind::Blob),
            ("src", "040000", TreeEntryKind::Tree),
            ("src/main.rs", "100644", TreeEntryKind::Blob),
            ("src/lib.rs", "100644", TreeEntryKind::Blob),
            ("src/nested", "040000", TreeEntryKind::Tree),
            ("src/nested/mod.rs", "100644", TreeEntryKind::Blob),
        ]));

        assert_eq!(index.sha(), "t");
        assert_eq!(index.get("src/main.rs").unwrap().path, "src/main.rs");
        assert!(index.get("main.rs").is_none());

        let root: Vec<&str> = index.children("").map(|e| e.path.as_str()).collect();
        assert_eq!(root, vec!["README.md", "src"]);

        let src: Vec<&str> = index.children("src").map(|e| e.path.as_str()).collect();
        assert_eq!(src, vec!["src/main.rs", "src/lib.rs", "src/nested"]);
    }

    #[test]
    fn parent_path_splits_tree_paths() {
        assert_eq!(parent_path("README.md"), "");
        assert_eq!(parent_path("src/main.rs"), "src");
        assert_eq!(parent_path("src/nested/mod.rs"), "src/nested");
    }

    #[test]
    fn local_git_metadata_probe_is_not_a_github_tree_lookup() {
        assert!(is_local_git_metadata_probe(".git"));
        assert!(!is_local_git_metadata_probe(".github"));
        assert!(!is_local_git_metadata_probe(".gitignore"));
    }

    #[test]
    fn direct_children_at_root_excludes_nested() {
        let t = tree(&[
            ("README.md", "100644", TreeEntryKind::Blob),
            ("Cargo.toml", "100644", TreeEntryKind::Blob),
            ("src", "040000", TreeEntryKind::Tree),
            ("src/main.rs", "100644", TreeEntryKind::Blob),
            ("src/lib.rs", "100644", TreeEntryKind::Blob),
            ("src/foo/bar.rs", "100644", TreeEntryKind::Blob),
        ]);
        let kids: Vec<&str> = direct_children(&t, "")
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(kids, vec!["README.md", "Cargo.toml", "src"]);
    }

    #[test]
    fn direct_children_under_subdir() {
        let t = tree(&[
            ("README.md", "100644", TreeEntryKind::Blob),
            ("src", "040000", TreeEntryKind::Tree),
            ("src/main.rs", "100644", TreeEntryKind::Blob),
            ("src/lib.rs", "100644", TreeEntryKind::Blob),
            ("src/foo", "040000", TreeEntryKind::Tree),
            ("src/foo/bar.rs", "100644", TreeEntryKind::Blob),
        ]);
        let kids: Vec<&str> = direct_children(&t, "src")
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(kids, vec!["src/main.rs", "src/lib.rs", "src/foo"]);
    }

    #[test]
    fn direct_children_under_nested_subdir() {
        // GitHub's recursive tree response includes a `tree`-kind entry for
        // every directory alongside its children — include `src/foo/bar` so
        // the fixture matches what we'd actually see in the wild.
        let t = tree(&[
            ("src/foo/a.rs", "100644", TreeEntryKind::Blob),
            ("src/foo/b.rs", "100644", TreeEntryKind::Blob),
            ("src/foo/bar", "040000", TreeEntryKind::Tree),
            ("src/foo/bar/c.rs", "100644", TreeEntryKind::Blob),
            ("src/other.rs", "100644", TreeEntryKind::Blob),
        ]);
        let kids: Vec<&str> = direct_children(&t, "src/foo")
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(kids, vec!["src/foo/a.rs", "src/foo/b.rs", "src/foo/bar"]);
    }

    #[test]
    fn direct_children_no_false_prefix_match() {
        // "srcz/..." must NOT match prefix "src" — the slash is significant.
        let t = tree(&[
            ("src/main.rs", "100644", TreeEntryKind::Blob),
            ("srcz/main.rs", "100644", TreeEntryKind::Blob),
        ]);
        let kids: Vec<&str> = direct_children(&t, "src")
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(kids, vec!["src/main.rs"]);
    }

    #[test]
    fn direct_children_empty_tree() {
        let t = tree(&[]);
        assert!(direct_children(&t, "").is_empty());
        assert!(direct_children(&t, "anywhere").is_empty());
    }
}

#[cfg(test)]
mod passthrough_tests {
    //! Verifies the "no-flip" behavior that lets a shell cwd inside a
    //! branch dir survive `ghfs promote`: the branch inode is allocated
    //! once and keeps its identity even after a worktree appears, with
    //! every subsequent op routed dynamically to disk.
    use super::*;
    use crate::cache::{BlobStore, CloneStore, MetaCache, default_remote_base};
    use crate::config::CloneTrigger;
    use crate::config::token::Token;
    use crate::fs::inode::RepoRef;
    use crate::github::{GithubClient, RepoFilter};
    use std::sync::Arc;

    /// Construct a `Ghfs` wired against on-disk temp dirs with no real
    /// GitHub connectivity. Passthrough ops never touch the HTTP client,
    /// so pointing the client at `http://invalid` is fine.
    fn build_fs(temp: &std::path::Path) -> (Ghfs, tokio::runtime::Runtime) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let token = Token::new("ignored");
        let client = Arc::new(
            GithubClient::with_base(token.clone(), "http://invalid.example".to_string())
                .expect("github client"),
        );
        let meta = Arc::new(MetaCache::open(temp.join("meta.db")).expect("meta cache"));
        let blobs = Arc::new(BlobStore::open(temp.join("blobs")).expect("blob store"));
        let clone_root = temp.join("clones");
        let store = Arc::new(CloneStore::open(&clone_root, token).expect("clone store"));
        let fs = Ghfs::new(
            runtime.handle().clone(),
            client,
            meta,
            blobs,
            RepoFilter::default(),
            Some(store),
            CloneTrigger::Never,
            None,
            default_remote_base(),
        );
        (fs, runtime)
    }

    /// Mirror what `CloneStore::ensure_clone` would lay down on disk,
    /// without going through libgit2 (which would need a real remote).
    /// `has_clone` checks for `.git/HEAD`, so we have to plant that too.
    fn make_fake_clone(
        clone_root: &std::path::Path,
        owner: &str,
        repo: &str,
    ) -> std::path::PathBuf {
        let wt = clone_root.join(owner).join(repo);
        std::fs::create_dir_all(wt.join(".git")).unwrap();
        std::fs::write(wt.join(".git").join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        wt
    }

    fn rref() -> RepoRef {
        RepoRef {
            repo_id: 42,
            owner: "acme".into(),
            name: "widgets".into(),
        }
    }

    #[test]
    fn clone_root_for_returns_some_when_dir_exists() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, _rt) = build_fs(temp.path());
        let repo = rref();

        assert!(
            fs.clone_root_for(&repo).is_none(),
            "before materialization, no passthrough root"
        );

        let wt = make_fake_clone(&temp.path().join("clones"), "acme", "widgets");
        assert_eq!(fs.clone_root_for(&repo), Some(wt));
    }

    #[test]
    fn repo_inode_is_stable_across_clone_materialization() {
        // The bug this fixes: a shell cwd inside `<repo>/` used to break
        // when promote ran because the inode flipped kinds and got a new
        // number. Here we lock in that the repo ino is allocated once and
        // keeps its identity across the appearance of a clone.
        let temp = tempfile::tempdir().unwrap();
        let (fs, _rt) = build_fs(temp.path());
        let repo = rref();

        // Allocate the repo inode pre-clone (simulating the user cd'ing
        // into the repo dir while the FS is in virtual mode).
        let (repo_ino_before, kind_before) =
            fs.inodes
                .lookup_or_create(FUSE_ROOT_INO, "widgets", || InodeKind::Repo {
                    repo: repo.clone(),
                    branch: "main".into(),
                });
        assert!(matches!(*kind_before, InodeKind::Repo { .. }));

        // Clone materializes.
        make_fake_clone(&temp.path().join("clones"), "acme", "widgets");
        assert!(fs.clone_root_for(&repo).is_some());

        // Re-resolving the same `(parent, name)` returns the *same* ino —
        // no eviction, no flip.
        let (repo_ino_after, kind_after) =
            fs.inodes.lookup_or_create(FUSE_ROOT_INO, "widgets", || {
                panic!("factory must not be called: ino must already be cached")
            });
        assert_eq!(repo_ino_before, repo_ino_after);
        assert!(matches!(*kind_after, InodeKind::Repo { .. }));

        // The disk path is now resolvable from this very same inode.
        assert!(fs.passthrough_disk_path(repo_ino_after).is_some());
    }

    #[test]
    fn passthrough_lookup_resolves_disk_entries() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, _rt) = build_fs(temp.path());
        let repo = rref();
        let wt = make_fake_clone(&temp.path().join("clones"), "acme", "widgets");
        std::fs::write(wt.join("hello.txt"), b"hello world").unwrap();
        std::fs::create_dir(wt.join("src")).unwrap();
        std::fs::write(wt.join("src").join("lib.rs"), b"fn main() {}").unwrap();

        let (repo_ino, _) = fs
            .inodes
            .lookup_or_create(100, "widgets", || InodeKind::Repo {
                repo: repo.clone(),
                branch: "main".into(),
            });

        let (file_ino, file_kind) = fs
            .passthrough_lookup_child(repo_ino, "hello.txt", &repo, "main", "", &wt)
            .unwrap()
            .expect("hello.txt should resolve");
        match &*file_kind {
            InodeKind::File {
                size,
                blob_sha,
                path,
                ..
            } => {
                assert_eq!(*size, b"hello world".len() as u64);
                assert_eq!(path, "hello.txt");
                assert!(blob_sha.is_empty(), "passthrough sentinel");
            }
            other => panic!("expected File, got {other:?}"),
        }

        // Repeated lookup returns the same ino (stable identity).
        let again = fs
            .passthrough_lookup_child(repo_ino, "hello.txt", &repo, "main", "", &wt)
            .unwrap()
            .unwrap();
        assert_eq!(file_ino, again.0);

        // Missing entry surfaces as Ok(None), not an error.
        assert!(
            fs.passthrough_lookup_child(repo_ino, "missing.txt", &repo, "main", "", &wt)
                .unwrap()
                .is_none()
        );

        // Subdirectory walk works the same way one level down.
        let src_disk = wt.join("src");
        let (_, lib_kind) = fs
            .passthrough_lookup_child(repo_ino, "lib.rs", &repo, "main", "src", &src_disk)
            .unwrap()
            .expect("src/lib.rs should resolve");
        match &*lib_kind {
            InodeKind::File { path, .. } => assert_eq!(path, "src/lib.rs"),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn passthrough_collect_children_enumerates_disk_contents() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, _rt) = build_fs(temp.path());
        let repo = rref();
        let wt = make_fake_clone(&temp.path().join("clones"), "acme", "widgets");
        std::fs::write(wt.join("README.md"), b"# hi").unwrap();
        std::fs::create_dir(wt.join("src")).unwrap();
        // `.git` is already on disk from make_fake_clone.

        let children = fs
            .passthrough_collect_children(123, &repo, "main", "", &wt)
            .unwrap();

        let mut names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        names.sort();
        // `.git` IS included — that's the whole point of passthrough; we
        // want git commands to see the real working tree.
        assert_eq!(names, vec![".git", "README.md", "src"]);
    }

    #[test]
    fn passthrough_ctx_only_resolves_under_materialized_repos() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, _rt) = build_fs(temp.path());
        let repo = rref();
        let wt = make_fake_clone(&temp.path().join("clones"), "acme", "widgets");
        std::fs::create_dir(wt.join("src")).unwrap();

        // Repo inode → ctx maps to the worktree root.
        let (repo_ino, _) =
            fs.inodes
                .lookup_or_create(FUSE_ROOT_INO, "widgets", || InodeKind::Repo {
                    repo: repo.clone(),
                    branch: "main".into(),
                });
        let (disk, ctx_repo, ctx_branch, ctx_rel) = fs.passthrough_ctx(repo_ino).expect("repo");
        assert_eq!(disk, wt);
        assert_eq!(ctx_repo, repo);
        assert_eq!(ctx_branch, "main");
        assert_eq!(ctx_rel, "");

        // Nested Dir inode → ctx points one level down.
        let metadata = std::fs::symlink_metadata(wt.join("src")).unwrap();
        let (dir_ino, _) = fs.passthrough_allocate(repo_ino, "src", &repo, "main", "", &metadata);
        let (disk, _, _, rel) = fs.passthrough_ctx(dir_ino).expect("dir");
        assert_eq!(disk, wt.join("src"));
        assert_eq!(rel, "src");

        // Inode for a repo WITHOUT a worktree → ctx is None, signaling
        // EROFS to write ops.
        let other_repo = RepoRef {
            repo_id: 99,
            owner: "other".into(),
            name: "thing".into(),
        };
        let (no_wt_ino, _) =
            fs.inodes
                .lookup_or_create(FUSE_ROOT_INO, "other", || InodeKind::Repo {
                    repo: other_repo,
                    branch: "main".into(),
                });
        assert!(fs.passthrough_ctx(no_wt_ino).is_none());

        // Repo with an empty branch (no default, no override) is never
        // passthrough-eligible — the FS surfaces an empty dir.
        let empty_branch_repo = RepoRef {
            repo_id: 100,
            owner: "acme".into(),
            name: "no-default".into(),
        };
        let (empty_ino, _) =
            fs.inodes
                .lookup_or_create(FUSE_ROOT_INO, "no-default", || InodeKind::Repo {
                    repo: empty_branch_repo,
                    branch: String::new(),
                });
        assert!(fs.passthrough_ctx(empty_ino).is_none());
    }

    #[test]
    fn evict_lets_a_recreated_name_take_a_fresh_inode() {
        // Models the unlink/create cycle: after `unlink` calls
        // `InodeTable::evict`, a subsequent `create` of the same name
        // must hand the kernel a brand-new ino so its page cache for the
        // old inode can't bleed into the new file.
        let temp = tempfile::tempdir().unwrap();
        let (fs, _rt) = build_fs(temp.path());
        let repo = rref();
        let wt = make_fake_clone(&temp.path().join("clones"), "acme", "widgets");
        std::fs::write(wt.join("foo.txt"), b"original").unwrap();

        let (repo_ino, _) =
            fs.inodes
                .lookup_or_create(FUSE_ROOT_INO, "widgets", || InodeKind::Repo {
                    repo: repo.clone(),
                    branch: "main".into(),
                });

        let metadata1 = std::fs::symlink_metadata(wt.join("foo.txt")).unwrap();
        let (ino_before, _) =
            fs.passthrough_allocate(repo_ino, "foo.txt", &repo, "main", "", &metadata1);

        // Simulate unlink → evict + remove on disk.
        fs.inodes.evict(repo_ino, "foo.txt");
        std::fs::remove_file(wt.join("foo.txt")).unwrap();

        // Re-create with same name → fresh content, fresh ino.
        std::fs::write(wt.join("foo.txt"), b"replacement contents").unwrap();
        let metadata2 = std::fs::symlink_metadata(wt.join("foo.txt")).unwrap();
        let (ino_after, _) =
            fs.passthrough_allocate(repo_ino, "foo.txt", &repo, "main", "", &metadata2);

        assert_ne!(
            ino_before, ino_after,
            "post-evict reallocation must produce a new ino so kernel page cache can't alias the two files"
        );
    }

    #[test]
    fn rename_preserves_ino_and_redirects_disk_resolution() {
        // Regression for the nix-direnv pattern that surfaced as EIO:
        //   symlink(.0_foo, target) → rename(.0_foo, foo) → readlink(foo)
        // The kernel keeps using the inode it got for `.0_foo`, so the
        // readlink dispatches with that same ino. If disk-path
        // resolution still believes the inode lives at `.0_foo` (its
        // allocation-time `path` field), it stats a renamed-away name
        // and returns ENOENT/EIO. With parent-link-driven resolution,
        // the same ino now resolves to `foo` automatically.
        let temp = tempfile::tempdir().unwrap();
        let (fs, _rt) = build_fs(temp.path());
        let repo = rref();
        let wt = make_fake_clone(&temp.path().join("clones"), "acme", "widgets");

        let (repo_ino, _) =
            fs.inodes
                .lookup_or_create(FUSE_ROOT_INO, "widgets", || InodeKind::Repo {
                    repo: repo.clone(),
                    branch: "main".into(),
                });

        // Stage the on-disk side of "symlink .0_link → /target".
        std::os::unix::fs::symlink("/target", wt.join(".0_link")).unwrap();
        let metadata = std::fs::symlink_metadata(wt.join(".0_link")).unwrap();
        let (link_ino, link_kind) =
            fs.passthrough_allocate(repo_ino, ".0_link", &repo, "main", "", &metadata);
        assert!(matches!(*link_kind, InodeKind::Symlink { .. }));
        assert_eq!(fs.passthrough_disk_path(link_ino), Some(wt.join(".0_link")));

        // Atomic-rename on disk + in the inode table.
        std::fs::rename(wt.join(".0_link"), wt.join("link")).unwrap();
        let relocated = fs.inodes.relocate(repo_ino, ".0_link", repo_ino, "link");
        assert_eq!(relocated, Some(link_ino), "ino must survive the rename");

        // The same ino now resolves to the new on-disk name — readlink
        // against this ino will succeed instead of returning EIO.
        assert_eq!(fs.passthrough_disk_path(link_ino), Some(wt.join("link")));
        assert_eq!(
            std::fs::read_link(fs.passthrough_disk_path(link_ino).unwrap()).unwrap(),
            std::path::Path::new("/target")
        );
    }
}
