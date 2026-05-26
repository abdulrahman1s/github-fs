use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use tracing::info;

use crate::cache::MetaCache;
use crate::config::{Config, token::Token};
use crate::github::{Conditional, GithubClient, RepoFilter};

pub async fn run(
    cli_token: Option<String>,
    cfg: Config,
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
             Pass --token, set GHFS_TOKEN or GITHUB_TOKEN, \
             or add `token = \"ghp_...\"` to ~/.config/ghfs/config.toml"
        )
    })?;

    let cache_dir = cache_dir_override.unwrap_or_else(|| cfg.cache_dir.clone());
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
    let meta_path = cache_dir.join("meta.db");
    info!(cache = %meta_path.display(), "opening metadata cache");
    let meta = MetaCache::open(&meta_path).context("opening metadata cache")?;
    let client = GithubClient::new(token).context("building github client")?;
    let filter =
        RepoFilter::new(cfg.owners.clone(), cfg.include_forks).with_visibility(cfg.visibility);

    let count = refresh_repos(&client, &meta, &filter).await?;
    println!("refreshed {count} repos -> {}", meta_path.display());
    Ok(())
}

/// Force a fresh fetch of the user's repo list and overwrite the cache.
///
/// We pass `None` for the conditional ETag so GitHub returns a 200 with the
/// full body rather than a 304 against the stale cache — that 200 *does*
/// count against the authenticated user's rate limit, which is why refresh
/// is a manual command rather than a background poll.
pub async fn refresh_repos(
    client: &GithubClient,
    meta: &MetaCache,
    filter: &RepoFilter,
) -> Result<usize> {
    let result = client
        .list_user_repos(None, filter)
        .await
        .context("listing user repos")?;

    let (etag, repos) = match result {
        Conditional::Modified { etag, body } => (etag, body),
        // A 304 to an unconditional request would be a server bug. Refuse to
        // touch the cache so the operator notices something is off.
        Conditional::NotModified => {
            return Err(anyhow!(
                "github returned 304 to an unconditional request — refusing to overwrite cache"
            ));
        }
    };

    let count = repos.len();
    meta.put_repos(&repos).context("writing repos to cache")?;
    if let Some(e) = etag {
        meta.put_etag(filter.etag_cache_key(), &e)
            .context("writing etag to cache")?;
    }
    Ok(count)
}

fn resolve_token(cli: Option<String>, cfg: &Config) -> Option<Token> {
    cli.filter(|s| !s.is_empty())
        .map(Token::new)
        .or_else(|| cfg.token.clone())
}
