use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use tracing::info;

use crate::cache::MetaCache;
use crate::cli::promote::{parse_fuse_path, resolve_token};
use crate::cli::status::list_ghfs_mounts;
use crate::config::Config;
use crate::github::{Conditional, GithubClient, GithubError, RepoFilter};

/// `ghfs branch` — set or clear the branch the mount surfaces under
/// `<mount>/<owner>/<repo>/`.
///
/// Without `--default`, an explicit branch name is required and is written
/// to the `branch_overrides` row keyed by `repo_id`. With `--default`, the
/// override row is deleted so the mount falls back to the repo's
/// GitHub-default branch.
///
/// Persistence is in sqlite — the next `ghfs mount` picks the change up.
/// Running mounts continue to surface whatever branch they resolved at
/// allocation time; the user must remount for the change to take effect
/// (mirrors how the FS pins repo→branch resolution per session).
pub async fn run(
    cli_token: Option<String>,
    cfg: Config,
    path_spec: PathBuf,
    branch_arg: Option<String>,
    reset_to_default: bool,
    cache_dir_override: Option<PathBuf>,
) -> Result<()> {
    if !reset_to_default && branch_arg.is_none() {
        return Err(anyhow!(
            "missing branch argument; pass a branch name or `--default` to clear"
        ));
    }

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
    let meta = MetaCache::open(&meta_path).context("opening metadata cache")?;
    let client = GithubClient::new(token).context("building github client")?;

    let (owner, name) = parse_fuse_path(&path_spec)?;

    let repo_id = resolve_repo_id(&client, &meta, &cfg, &owner, &name).await?;

    if reset_to_default {
        meta.delete_branch_override(repo_id)
            .context("clearing branch override")?;
        info!(owner, repo = name, repo_id, "cleared branch override");
        println!("cleared branch override for {owner}/{name}");
    } else {
        let branch = branch_arg.expect("guarded above");
        validate_branch_exists(&client, &owner, &name, &branch).await?;
        meta.put_branch_override(repo_id, &branch)
            .context("writing branch override")?;
        info!(owner, repo = name, repo_id, branch, "set branch override");
        println!("set {owner}/{name} branch -> {branch}");
    }

    // Hint at the remount step so running mounts don't silently surface
    // stale content. The FS pins the repo's effective branch at allocation
    // time for inode-identity stability across `ghfs promote` — the price
    // is that overrides require a remount to take effect.
    for mount in list_ghfs_mounts().unwrap_or_default() {
        eprintln!(
            "active mount at {} — remount to apply the new branch (`ghfs unmount {}` then `ghfs mount {}`)",
            mount.mountpoint.display(),
            mount.mountpoint.display(),
            mount.mountpoint.display(),
        );
    }
    Ok(())
}

/// Look up the repo id from the cached repo list, with a live API fallback
/// when the cache is empty (e.g. user ran `branch` before ever mounting).
async fn resolve_repo_id(
    client: &GithubClient,
    meta: &MetaCache,
    cfg: &Config,
    owner: &str,
    name: &str,
) -> Result<u64> {
    if let Ok(cached) = meta.get_repos()
        && let Some(r) = cached
            .into_iter()
            .find(|r| r.owner.login == owner && r.name == name)
    {
        return Ok(r.id);
    }

    let filter =
        RepoFilter::new(cfg.owners.clone(), cfg.include_forks).with_visibility(cfg.visibility);
    let result = client
        .list_user_repos(None, &filter)
        .await
        .context("listing user repos to find repo id")?;
    let body = match result {
        Conditional::Modified { body, .. } => {
            meta.put_repos(&body).context("writing repos to cache")?;
            body
        }
        Conditional::NotModified => Vec::new(),
    };
    body.into_iter()
        .find(|r| r.owner.login == owner && r.name == name)
        .map(|r| r.id)
        .ok_or_else(|| {
            anyhow!(
                "could not find {owner}/{name} in the user's repo list; \
                 are you authenticated for that repo?"
            )
        })
}

/// Sanity-check that `branch` exists on the remote before persisting the
/// override. Catches typos up front rather than producing an "empty repo"
/// on next mount.
async fn validate_branch_exists(
    client: &GithubClient,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<()> {
    match client.get_branch(owner, name, branch, None).await {
        Ok(_) => Ok(()),
        Err(GithubError::NotFound) => Err(anyhow!(
            "branch `{branch}` does not exist on {owner}/{name}"
        )),
        Err(e) => Err(anyhow::Error::from(e).context("validating branch")),
    }
}
