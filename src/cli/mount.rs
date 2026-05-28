use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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

    raise_nofile_to_hard_limit();

    let cache_dir = cache_dir_override.unwrap_or_else(|| cfg.cache_dir.clone());
    let meta_path = cache_dir.join("meta.db");
    let blob_root = cache_dir.join("blobs");
    let clone_root = cache_dir.join("clones");

    info!(
        mountpoint = %mount_path.display(),
        cache_dir = %cache_dir.display(),
        clone_trigger = ?cfg.clone.trigger,
        clone_fetch_depth = ?cfg.clone.fetch_depth,
        "opening caches"
    );
    let meta = Arc::new(MetaCache::open(&meta_path).context("opening metadata cache")?);
    let blobs = Arc::new(BlobStore::open(&blob_root).context("opening blob store")?);
    // Always open the clone store, even with `trigger = "never"`. The trigger
    // controls *automatic* fetching from the FUSE callbacks; manual
    // `ghfs promote` writes clones into this same dir, and the FS layer
    // routes ops to those clones on lookup (passthrough) so the mount and
    // the CLI interoperate regardless of trigger.
    let clone_store = Some(Arc::new(
        CloneStore::open_with_url_protocol(&clone_root, token.clone(), cfg.clone.url_protocol)
            .context("opening clone store")?,
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
        cfg.clone.fetch_depth,
        default_remote_base(),
    );

    // Warm the repo-list cache before the kernel asks for it. The first
    // `ls` of the mountpoint blocks on this fetch otherwise; by spawning
    // it here we usually have a populated snapshot by the time the user
    // touches the mount. If the kernel races ahead, the FUSE-driven
    // `list_repos` waits on `fetch_lock` instead of duplicating the call.
    fs.spawn_background_prefetch();
    fs.spawn_refresh_signal_listener();

    if let Some(secs) = cfg.auto_refresh_interval_secs {
        fs.spawn_auto_refresh(Duration::from_secs(secs));
    }

    // Drop a pidfile keyed by mount path so `ghfs refresh` can find and
    // SIGUSR1 every running mount sharing this cache dir. Cleaned up by
    // the signal-unmounter on graceful shutdown; stale entries from
    // crashes are reaped by `ghfs refresh` itself.
    let pidfile = pidfile_path(&cache_dir, &mount_path);
    if let Some(parent) = pidfile.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Two lines: PID and mountpoint. The pidfile name is encoded, so
    // storing the mountpoint inside lets `ghfs refresh` print a
    // human-readable path without round-tripping the encoding.
    let pidfile_contents = format!("{}\n{}\n", std::process::id(), mount_path.display());
    if let Err(e) = std::fs::write(&pidfile, &pidfile_contents) {
        warn!(error = %e, path = %pidfile.display(), "could not write mount pidfile; `ghfs refresh` won't notify this mount");
    }

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

    install_signal_unmounter(handle, mount_path.clone(), pidfile.clone());

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
/// Deterministic pidfile location for a given (cache_dir, mount_path).
/// `ghfs refresh` reads every entry under `<cache>/mounts/` so the
/// mapping just needs to be collision-resistant across mountpoints.
pub fn pidfile_path(cache_dir: &std::path::Path, mount_path: &std::path::Path) -> PathBuf {
    let encoded = mount_path
        .to_string_lossy()
        .replace('%', "%25")
        .replace('/', "%2F");
    cache_dir.join("mounts").join(format!("{encoded}.pid"))
}

fn install_signal_unmounter(handle: Handle, mount_path: PathBuf, pidfile: PathBuf) {
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

                if let Err(e) = std::fs::remove_file(&pidfile)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(error = %e, path = %pidfile.display(), "failed to remove mount pidfile");
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

/// Raise this process's `RLIMIT_NOFILE` soft limit to the hard limit.
///
/// FUSE is fundamentally an fd-multiplier: every kernel-side `open` of a
/// passthrough file holds one real fd in this process for the duration of
/// the kernel's file handle. Under heavy load (e.g. `cargo test` linking
/// many crates concurrently) we can blow through a low session default
/// (commonly 1024) long before the system's hard cap. The hard cap is
/// already what the admin allows; claiming it on startup just turns a
/// surprising EMFILE into the steady-state behavior.
fn raise_nofile_to_hard_limit() {
    // SAFETY: `getrlimit`/`setrlimit` write to caller-owned storage and
    // mutate only this process's limits.
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        warn!(error = %err, "getrlimit(RLIMIT_NOFILE) failed; leaving fd limit untouched");
        return;
    }
    if rlim.rlim_cur >= rlim.rlim_max {
        info!(
            soft = rlim.rlim_cur,
            hard = rlim.rlim_max,
            "RLIMIT_NOFILE already at hard cap"
        );
        return;
    }
    let new = libc::rlimit {
        rlim_cur: rlim.rlim_max,
        rlim_max: rlim.rlim_max,
    };
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        warn!(
            error = %err,
            soft = rlim.rlim_cur,
            hard = rlim.rlim_max,
            "setrlimit(RLIMIT_NOFILE) failed; leaving soft limit unchanged"
        );
        return;
    }
    info!(
        from_soft = rlim.rlim_cur,
        to = rlim.rlim_max,
        "raised RLIMIT_NOFILE soft limit to hard cap"
    );
}
