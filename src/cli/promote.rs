use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use tracing::info;

use crate::cache::{CloneStore, MetaCache, default_remote_base};
use crate::cli::status::list_ghfs_mounts;
use crate::config::{Config, token::Token};
use crate::github::{Conditional, GithubClient, RepoFilter};

/// `ghfs promote` — manually fetch a repo (and a branch) into the local
/// CloneStore, materializing a non-bare worktree on disk.
///
/// This is the same operation the FUSE layer would perform on first access
/// under `[clone] trigger = "on_access"`, but invoked explicitly so the user
/// can pre-stage a repo without going through the mount. Works regardless of
/// the configured trigger — useful for "I want this repo on disk right now."
///
/// The argument must be a path inside an active ghfs FUSE mount, e.g.
/// `~/ghfs/acme/widgets` (or anywhere deeper — `~/ghfs/acme/widgets/src/x.rs`
/// works too). `/proc/mounts` is consulted to find the mount root, and the
/// first two path components after the root are taken as `<owner>/<repo>`.
/// The branch comes from `--branch` or the repo's effective default.
pub async fn run(
    cli_token: Option<String>,
    cfg: Config,
    path_spec: PathBuf,
    branch_arg: Option<String>,
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
    let clone_root = cache_dir.join("clones");

    let meta = MetaCache::open(&meta_path).context("opening metadata cache")?;
    let client = GithubClient::new(token.clone()).context("building github client")?;

    let (owner, name) = parse_fuse_path(&path_spec)?;
    let branch = match branch_arg {
        Some(b) => b,
        None => resolve_default_branch(&client, &meta, &owner, &name).await?,
    };

    let fs_name = encode_branch_name(&branch);
    info!(
        owner,
        repo = name,
        branch,
        fs_name,
        cache = %clone_root.display(),
        "promoting repo into local clone store"
    );

    let store =
        CloneStore::open(&clone_root, token).context("opening clone store under cache dir")?;
    let path = store
        .ensure_worktree(&owner, &name, &branch, &fs_name, &default_remote_base())
        .context("materializing worktree via libgit2")?;

    println!("{}", path.display());

    // Surface the FUSE-side effect on stderr so stdout stays a single
    // path (scriptable). Any live mount sharing this cache will start
    // serving the repo dir from this worktree on its next lookup
    // (within `REPO_ENTRY_TTL` ~1s) *if* this is the repo's effective
    // branch — otherwise the worktree exists on disk but the mount
    // continues to show the configured branch.
    // `/proc/mounts` doesn't expose `--cache-dir`, so we list every ghfs
    // mount and let the user pick the relevant one. Loop emits nothing
    // when no mount is running.
    for mount in list_ghfs_mounts().unwrap_or_default() {
        let mount_repo_path = mount.mountpoint.join(&owner).join(&name);
        eprintln!(
            "{} will serve files from this worktree on the next lookup (~1s) if `{}` is the repo's effective branch",
            mount_repo_path.display(),
            branch
        );
    }
    Ok(())
}

/// Extract `(owner, repo)` from a path inside a ghfs FUSE mount. The first
/// two path components after the mount root identify the repo; deeper
/// components are subpaths inside it and are ignored. Returns an error if
/// the path isn't inside an active mount or doesn't reach the repo level.
pub(crate) fn parse_fuse_path(path: &Path) -> Result<(String, String)> {
    // Canonicalize so we can compare against `/proc/mounts` entries without
    // worrying about `~`, `..`, or relative segments. We attempt
    // canonicalize first and fall back to lexical normalization if it
    // fails — the user's intent is captured by the literal path either way.
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let mounts = list_ghfs_mounts().context("reading /proc/mounts to find ghfs mounts")?;
    let root = mounts
        .iter()
        .find(|m| canon.starts_with(&m.mountpoint))
        .map(|m| m.mountpoint.clone())
        .ok_or_else(|| {
            anyhow!(
                "path {} is not inside any active ghfs mount (checked /proc/mounts); \
                 mount ghfs first, then pass a path under it",
                canon.display()
            )
        })?;

    let rel = canon
        .strip_prefix(&root)
        .context("stripping mount prefix")?;
    let mut comps = rel.components().filter_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str().map(str::to_owned),
        _ => None,
    });
    let owner = comps.next().ok_or_else(|| {
        anyhow!(
            "path {} is the ghfs mount root itself; specify at least <owner>/<repo>",
            canon.display()
        )
    })?;
    let repo = comps.next().ok_or_else(|| {
        anyhow!(
            "path {} only names an owner dir; specify <owner>/<repo>",
            canon.display()
        )
    })?;
    Ok((owner, repo))
}

/// Find the default branch for `owner/name`. Tries the cached repo list
/// first, otherwise asks GitHub. Errors out if the repo has no recorded
/// default (e.g. a freshly-created empty repo).
pub(crate) async fn resolve_default_branch(
    client: &GithubClient,
    meta: &MetaCache,
    owner: &str,
    name: &str,
) -> Result<String> {
    if let Ok(cached) = meta.get_repos()
        && let Some(r) = cached
            .into_iter()
            .find(|r| r.owner.login == owner && r.name == name)
        && let Some(b) = r.default_branch
    {
        return Ok(b);
    }

    // Fallback: hit the live API. We don't have a per-repo metadata endpoint
    // wired up here, so re-list with a narrow filter and pick our row. This
    // is a one-off command, so the extra API call is fine.
    let filter = RepoFilter::default();
    let result = client
        .list_user_repos(None, &filter)
        .await
        .context("listing user repos to find default branch")?;
    let body = match result {
        Conditional::Modified { body, .. } => body,
        Conditional::NotModified => Vec::new(),
    };
    body.into_iter()
        .find(|r| r.owner.login == owner && r.name == name)
        .and_then(|r| r.default_branch)
        .ok_or_else(|| {
            anyhow!(
                "could not determine default branch for {owner}/{name}; pass --branch explicitly"
            )
        })
}

/// Same encoding the FUSE layer uses so the worktree dir basename matches the
/// FUSE entry name. Duplicated rather than imported to keep this CLI module
/// independent of `fs`.
pub(crate) fn encode_branch_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn resolve_token(cli: Option<String>, cfg: &Config) -> Option<Token> {
    cli.filter(|s| !s.is_empty())
        .map(Token::new)
        .or_else(|| cfg.token.clone())
}

#[cfg(test)]
mod tests {
    use super::encode_branch_name;

    #[test]
    fn branch_name_encoding_matches_fs_layer() {
        assert_eq!(encode_branch_name("main"), "main");
        assert_eq!(encode_branch_name("feature/x"), "feature%2Fx");
        assert_eq!(encode_branch_name("release%2Ftest"), "release%252Ftest");
    }
}
