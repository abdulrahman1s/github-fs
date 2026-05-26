use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use fuser::{MountOption, Session};
use tokio::runtime::Handle;
use tracing::{info, warn};

use crate::cache::{BlobStore, CloneStore, MetaCache, default_remote_base};
use crate::config::{Config, token::Token};
use crate::fs::Ghfs;
use crate::github::{GithubClient, RepoFilter};

pub fn run(
    handle: Handle,
    cli_token: Option<String>,
    cfg: Config,
    mount_path: PathBuf,
    foreground: bool,
    cache_dir_override: Option<PathBuf>,
) -> Result<()> {
    let token = resolve_token(cli_token, &cfg).ok_or_else(|| {
        let hint = if cfg.config_file_present {
            "config file is present but no `token` key was set"
        } else {
            "no config file found"
        };
        anyhow!(
            "no GitHub token available ({hint}). \
             Pass --token, set GHFS_TOKEN/GITHUB_TOKEN, \
             or add `token = \"ghp_...\"` to ~/.config/ghfs/config.toml"
        )
    })?;

    if !mount_path.exists() {
        return Err(anyhow!(
            "mountpoint does not exist: {}",
            mount_path.display()
        ));
    }
    if !mount_path.is_dir() {
        return Err(anyhow!(
            "mountpoint is not a directory: {}",
            mount_path.display()
        ));
    }

    let cache_dir = cache_dir_override.unwrap_or_else(|| cfg.cache_dir.clone());
    let meta_path = cache_dir.join("meta.db");
    let blob_root = cache_dir.join("blobs");
    let clone_root = cache_dir.join("clones");

    info!(
        mountpoint = %mount_path.display(),
        cache_dir = %cache_dir.display(),
        clone_trigger = ?cfg.clone.trigger,
        "opening caches"
    );
    let meta = Arc::new(MetaCache::open(&meta_path).context("opening metadata cache")?);
    let blobs = Arc::new(BlobStore::open(&blob_root).context("opening blob store")?);
    // Always open the clone store, even with `trigger = "never"`. The trigger
    // controls *automatic* fetching from the FUSE callbacks; manual
    // `ghfs promote` writes worktrees into this same dir, and the FS layer
    // routes ops to those worktrees on lookup (passthrough) so the mount and
    // the CLI interoperate regardless of trigger.
    let clone_store = Some(Arc::new(
        CloneStore::open(&clone_root, token.clone()).context("opening clone store")?,
    ));
    let client = Arc::new(GithubClient::new(token).context("building github client")?);
    let filter =
        RepoFilter::new(cfg.owners.clone(), cfg.include_forks).with_visibility(cfg.visibility);

    let fs = Ghfs::new(
        handle.clone(),
        client,
        meta,
        blobs,
        filter,
        clone_store,
        cfg.clone.trigger,
        default_remote_base(),
    );

    // Warm the repo-list cache before the kernel asks for it. The first
    // `ls` of the mountpoint blocks on this fetch otherwise; by spawning
    // it here we usually have a populated snapshot by the time the user
    // touches the mount. If the kernel races ahead, the FUSE-driven
    // `list_repos` waits on `fetch_lock` instead of duplicating the call.
    fs.spawn_background_prefetch();

    // The kernel `RO` flag would block every write op up front — but that
    // also blocks writes to materialized branches, which we want to allow
    // as passthrough to the on-disk worktree. The FUSE layer enforces the
    // read-only policy itself by returning `EROFS` from every write op
    // unless the target ino resolves to a worktree-backed path. See the
    // AGENTS.md invariant "Default is read-only; materialized branches
    // are writable."
    let options = vec![
        MountOption::FSName("ghfs".to_string()),
        MountOption::AutoUnmount,
        MountOption::DefaultPermissions,
    ];

    if !foreground {
        // v0.1: backgrounding (daemonize) is a stretch goal; we always run in
        // the foreground. Surface this clearly so users don't expect a
        // detached process.
        warn!("backgrounding not yet implemented; running in foreground (Ctrl-C to unmount)");
    }

    install_signal_unmounter(handle, mount_path.clone());

    info!("mounting at {}", mount_path.display());
    let mut session =
        Session::new(fs, &mount_path, &options).context("fuser::Session::new failed")?;
    session.run().context("fuser session loop failed")?;
    info!("unmounted cleanly");
    Ok(())
}

/// Spawn a thread that uses the existing tokio runtime to await SIGINT/SIGTERM,
/// then shells out to `fusermount3 -u` to make `fuser::mount2` return cleanly.
///
/// We use a separate OS thread rather than running on the runtime directly so
/// `mount::run` stays synchronous (it still blocks in `fuser::mount2` on the
/// main thread). The runtime is shared, not stolen.
///
/// Installing `tokio::signal::unix::signal` replaces the default SIGINT/SIGTERM
/// dispositions process-wide; the kernel will no longer kill us on Ctrl-C, so
/// the unmount path here is the only thing that ends the process.
fn install_signal_unmounter(handle: Handle, mount_path: PathBuf) {
    let result = std::thread::Builder::new()
        .name("ghfs-signal".into())
        .spawn(move || {
            handle.block_on(async move {
                use tokio::signal::unix::{SignalKind, signal};

                let mut sigint = match signal(SignalKind::interrupt()) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "failed to install SIGINT handler; Ctrl-C will kill the process");
                        return;
                    }
                };
                let mut sigterm = match signal(SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "failed to install SIGTERM handler");
                        return;
                    }
                };

                let sig = tokio::select! {
                    _ = sigint.recv() => "SIGINT",
                    _ = sigterm.recv() => "SIGTERM",
                };
                info!(signal = sig, mountpoint = %mount_path.display(), "shutdown signal received; unmounting");

                match std::process::Command::new("fusermount3")
                    .arg("-u")
                    .arg(&mount_path)
                    .status()
                {
                    Ok(status) if status.success() => {}
                    Ok(status) => warn!(
                        %status,
                        mountpoint = %mount_path.display(),
                        "fusermount3 -u exited non-zero; the kernel may already have unmounted"
                    ),
                    Err(e) => warn!(
                        error = %e,
                        "failed to invoke fusermount3 -u; falling back to process exit"
                    ),
                }
            });
        });
    if let Err(e) = result {
        warn!(error = %e, "failed to spawn signal-handler thread; Ctrl-C will use default disposition");
    }
}

fn resolve_token(cli: Option<String>, cfg: &Config) -> Option<Token> {
    cli.filter(|s| !s.is_empty())
        .map(Token::new)
        .or_else(|| cfg.token.clone())
}
