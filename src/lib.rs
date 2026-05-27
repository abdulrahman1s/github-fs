pub mod cache;
pub mod cli;
pub mod config;
pub mod fs;
pub mod github;

use anyhow::Result;
use tracing::warn;

use clap::CommandFactory;

use crate::cli::{Args, Command};

pub use crate::config::CloneTrigger;

pub fn run(args: Args) -> Result<()> {
    cli::init_tracing(args.log_level.as_deref());

    if let Command::Completions { shell } = args.command {
        let mut cmd = Args::command();
        let bin_name = cmd.get_name().to_string();
        clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
        return Ok(());
    }

    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to load config file: {e}; continuing with env-only defaults");
            env_only_config()
        }
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match args.command {
        Command::Whoami => rt.block_on(cli::whoami::run(args.token, cfg)),
        Command::Mount {
            path,
            foreground,
            cache_dir,
        } => cli::mount::run(
            rt.handle().clone(),
            args.token,
            cfg,
            path,
            foreground,
            cache_dir,
        ),
        Command::Unmount { path, strict } => cli::unmount::run(path, !strict),
        Command::Status => cli::status::run(),
        Command::Refresh { cache_dir } => {
            rt.block_on(cli::refresh::run(args.token, cfg, cache_dir))
        }
        Command::Promote {
            path,
            branch,
            cache_dir,
        } => rt.block_on(cli::promote::run(args.token, cfg, path, branch, cache_dir)),
        Command::Info { path, cache_dir } => {
            rt.block_on(cli::info::run(args.token, cfg, path, cache_dir))
        }
        Command::Branch {
            path,
            branch,
            default,
            cache_dir,
        } => rt.block_on(cli::branch::run(
            args.token, cfg, path, branch, default, cache_dir,
        )),
        Command::Completions { .. } => unreachable!("handled before runtime setup"),
    }
}

fn env_only_config() -> config::Config {
    config::Config {
        token: std::env::var("GHFS_TOKEN")
            .ok()
            .or_else(|| std::env::var("GITHUB_TOKEN").ok())
            .filter(|s| !s.is_empty())
            .map(config::token::Token::new),
        mount_path: None,
        cache_dir: std::path::PathBuf::from("/tmp/ghfs-cache"),
        log_level: None,
        cache_ttl_secs: 300,
        auto_refresh_interval_secs: None,
        owners: config::Owners::default(),
        include_forks: false,
        visibility: config::Visibility::default(),
        clone: config::CloneConfig::default(),
        config_file_present: false,
    }
}
