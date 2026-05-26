pub mod branch;
pub mod info;
pub mod mount;
pub mod promote;
pub mod refresh;
pub mod status;
pub mod unmount;
pub mod whoami;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "ghfs",
    version,
    about = "Mount GitHub repositories as a FUSE filesystem on Linux (read-only by default; materialized branches become writable passthroughs)"
)]
pub struct Args {
    /// GitHub personal access token. Overrides GHFS_TOKEN, GITHUB_TOKEN, and config file.
    #[arg(long, global = true)]
    pub token: Option<String>,

    /// Log filter (e.g. `ghfs=debug,info`). Overrides RUST_LOG and the `log_level` config key.
    #[arg(long, global = true)]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the authenticated GitHub user (smoke-tests auth wiring).
    Whoami,

    /// Mount the GitHub filesystem at <path>.
    Mount {
        path: PathBuf,
        #[arg(long)]
        foreground: bool,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },

    /// Unmount a GHFS mountpoint (wraps `fusermount3 -u`).
    Unmount {
        path: PathBuf,
        /// Detach the mount immediately even if it is in use. Maps to
        /// `fusermount3 -uz`; the mount is freed once the last open handle
        /// or `cwd` reference goes away.
        #[arg(long)]
        lazy: bool,
    },

    /// List active ghfs mounts by scanning /proc/mounts.
    Status,

    /// Re-fetch the authenticated user's repo list and update the on-disk
    /// cache. Useful when you've added/removed repos on GitHub and don't
    /// want to wait for the next remount.
    Refresh {
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },

    /// Manually clone a repo (and a branch) into the local clone store,
    /// materializing a non-bare worktree on disk. Equivalent to what
    /// `[clone] trigger = "on_access"` would do on first FUSE access, but
    /// invoked explicitly. Prints the worktree path on success.
    Promote {
        /// Path inside an active ghfs mount (e.g. `~/ghfs/<owner>/<repo>`,
        /// or any deeper path under the repo dir). The owner and repo are
        /// inferred from the first two components after the mount root.
        path: PathBuf,
        /// Branch to materialize. Defaults to the repo's effective branch
        /// (override set via `ghfs branch`, falling back to the GitHub default).
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },

    /// Print metadata for the repo identified by a path inside an active
    /// ghfs mount (URL, description, visibility, fork flag, default and
    /// effective branches).
    Info {
        /// Path inside an active ghfs mount (e.g. `~/ghfs/<owner>/<repo>`,
        /// or any deeper path under the repo dir). The owner and repo are
        /// inferred from the first two components after the mount root.
        path: PathBuf,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },

    /// Set (or clear) the branch the mount surfaces under
    /// `<mount>/<owner>/<repo>/`. Persisted in the metadata cache; takes
    /// effect on the next mount.
    Branch {
        /// Path inside an active ghfs mount (e.g. `~/ghfs/<owner>/<repo>`,
        /// or any deeper path under the repo dir). The owner and repo are
        /// inferred from the first two components after the mount root.
        path: PathBuf,
        /// Branch to surface. Omit and pass `--default` to clear the
        /// override (fall back to the repo's GitHub default).
        branch: Option<String>,
        /// Reset to the repo's GitHub-default branch by deleting the override.
        #[arg(long, conflicts_with = "branch")]
        default: bool,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },

    /// Print a shell-completion script to stdout.
    ///
    /// Example: `ghfs completions zsh > ~/.zfunc/_ghfs` (then ensure
    /// `~/.zfunc` is on `fpath` and `autoload -U compinit && compinit`).
    Completions {
        /// Target shell.
        shell: Shell,
    },
}

/// Initialise the tracing subscriber. Precedence:
///   1. explicit `level` (from CLI flag)
///   2. RUST_LOG env var
///   3. fallback to `ghfs=info,warn`.
///
/// Writes to stderr so subcommand stdout (e.g. `whoami` user info) stays clean.
pub fn init_tracing(level: Option<&str>) {
    let filter = level
        .and_then(|s| EnvFilter::try_new(s).ok())
        .or_else(|| EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| EnvFilter::new("ghfs=info,warn"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
