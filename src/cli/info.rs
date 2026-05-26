use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

use crate::cache::MetaCache;
use crate::cli::promote::{parse_fuse_path, resolve_token};
use crate::config::Config;
use crate::github::{Conditional, GithubClient, Repo, RepoFilter};

/// `ghfs info` — print repository metadata for the repo identified by a
/// FUSE-mount path. Reads the cached repo list first; falls back to a live
/// `GET /user/repos` if the cache is empty.
pub async fn run(
    cli_token: Option<String>,
    cfg: Config,
    path_spec: PathBuf,
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
    let meta = MetaCache::open(&meta_path).context("opening metadata cache")?;
    let client = GithubClient::new(token).context("building github client")?;

    let (owner, name) = parse_fuse_path(&path_spec)?;
    let repo = find_repo(&client, &meta, &cfg, &owner, &name).await?;
    let effective_branch = meta
        .get_branch_override(repo.id)
        .context("reading branch override")?;

    print_repo(&repo, effective_branch.as_deref());
    Ok(())
}

async fn find_repo(
    client: &GithubClient,
    meta: &MetaCache,
    cfg: &Config,
    owner: &str,
    name: &str,
) -> Result<Repo> {
    if let Ok(cached) = meta.get_repos()
        && let Some(r) = cached
            .into_iter()
            .find(|r| r.owner.login == owner && r.name == name)
    {
        return Ok(r);
    }

    let filter =
        RepoFilter::new(cfg.owners.clone(), cfg.include_forks).with_visibility(cfg.visibility);
    let result = client
        .list_user_repos(None, &filter)
        .await
        .context("listing user repos to look up repo info")?;
    let body = match result {
        Conditional::Modified { body, .. } => {
            meta.put_repos(&body).context("writing repos to cache")?;
            body
        }
        Conditional::NotModified => Vec::new(),
    };
    body.into_iter()
        .find(|r| r.owner.login == owner && r.name == name)
        .ok_or_else(|| {
            anyhow!(
                "could not find {owner}/{name} in the user's repo list; \
                 are you authenticated for that repo?"
            )
        })
}

fn print_repo(repo: &Repo, override_branch: Option<&str>) {
    // GitHub doesn't include an HTML URL on the JSON we cache, but the
    // canonical web URL is just `https://github.com/<full_name>` — derive
    // rather than persist an extra field.
    let url = format!("https://github.com/{}", repo.full_name);
    let description = repo.description.as_deref().unwrap_or("(none)");
    let default_branch = repo.default_branch.as_deref().unwrap_or("(none)");
    let effective_branch = override_branch.unwrap_or(default_branch);
    let effective_suffix = if override_branch.is_some() {
        " (override)"
    } else {
        ""
    };
    let visibility = if repo.private { "private" } else { "public" };
    let fork = if repo.fork { "yes" } else { "no" };

    println!("{}", repo.full_name);
    println!("  url:              {url}");
    println!("  description:      {description}");
    println!("  visibility:       {visibility}");
    println!("  fork:             {fork}");
    println!("  default branch:   {default_branch}");
    println!("  effective branch: {effective_branch}{effective_suffix}");
}

#[cfg(test)]
mod tests {
    use super::print_repo;
    use crate::github::{Owner, Repo};

    fn sample_repo() -> Repo {
        Repo {
            id: 1,
            name: "widgets".into(),
            full_name: "acme/widgets".into(),
            owner: Owner {
                login: "acme".into(),
                id: 42,
            },
            private: true,
            default_branch: Some("main".into()),
            description: Some("a thing".into()),
            size: 0,
            fork: false,
        }
    }

    #[test]
    fn print_repo_smoke() {
        // No assertion beyond "doesn't panic and uses the override flag
        // without dereferencing None when default branch is set".
        print_repo(&sample_repo(), Some("dev"));
        print_repo(&sample_repo(), None);
    }
}
