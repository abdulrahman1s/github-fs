use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use tracing::{info, warn};

use crate::cache::MetaCache;
use crate::config::{Config, token::Token};
use crate::github::{Conditional, GithubClient, Repo, RepoFilter};

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

    let report = refresh_repos_with_report(&client, &meta, &filter).await?;
    let events = notify_live_mounts(&cache_dir);
    print!("{}", render_refresh_output(&report, &meta_path, &events));
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    pub total_repos: usize,
    pub added: Vec<RepoSummary>,
    pub removed: Vec<RepoSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSummary {
    pub full_name: String,
    pub private: bool,
    pub fork: bool,
    pub default_branch: Option<String>,
    pub description: Option<String>,
}

impl RepoSummary {
    fn from_repo(repo: &Repo) -> Self {
        Self {
            full_name: repo_full_name(repo),
            private: repo.private,
            fork: repo.fork,
            default_branch: repo.default_branch.clone(),
            description: repo.description.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum NotifyEvent {
    Notified {
        pid: i32,
        mount_path: String,
    },
    ReapedStale {
        pid: i32,
        mount_path: String,
    },
    SignalFailed {
        pid: i32,
        mount_path: String,
        error: String,
    },
}

/// SIGUSR1 every running mount whose pidfile lives under
/// `<cache_dir>/mounts/`. Each mount listens for SIGUSR1 and
/// re-fetches the repo list, so this is the cross-process bridge
/// between the on-disk sqlite cache and the running mount's
/// in-memory snapshot.
///
/// Pidfiles whose process has gone away (crash, kill -9, ...) are
/// reaped here so they don't accumulate. We use `kill(1)` rather
/// than the `libc` FFI to keep the module `unsafe`-free; the cost
/// is one extra `fork+exec` per live mount per `ghfs refresh`,
/// which is negligible.
fn notify_live_mounts(cache_dir: &Path) -> Vec<NotifyEvent> {
    let mounts_dir = cache_dir.join("mounts");
    let entries = match std::fs::read_dir(&mounts_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn!(error = %e, dir = %mounts_dir.display(), "could not list mount pidfiles");
            return Vec::new();
        }
    };

    let mut events = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("pid")) {
            continue;
        }
        let Some(record) = read_pidfile(&path) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };

        if !process_alive(record.pid) {
            let _ = std::fs::remove_file(&path);
            events.push(NotifyEvent::ReapedStale {
                pid: record.pid,
                mount_path: record.mount_path,
            });
            continue;
        }

        match send_sigusr1(record.pid) {
            Ok(()) => events.push(NotifyEvent::Notified {
                pid: record.pid,
                mount_path: record.mount_path,
            }),
            Err(e) => events.push(NotifyEvent::SignalFailed {
                pid: record.pid,
                mount_path: record.mount_path,
                error: e,
            }),
        }
    }
    events
}

struct PidfileRecord {
    pid: i32,
    mount_path: String,
}

fn read_pidfile(path: &Path) -> Option<PidfileRecord> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut lines = raw.lines();
    let pid: i32 = lines.next()?.trim().parse().ok().filter(|&p| p > 0)?;
    // Older mounts wrote only the PID; treat a missing second line as
    // unknown rather than rejecting the file outright.
    let mount_path = lines
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<unknown>".to_string());
    Some(PidfileRecord { pid, mount_path })
}

fn process_alive(pid: i32) -> bool {
    // `/proc/<pid>` existing is the cheapest, syscall-only liveness
    // probe — no fork required, no `libc::kill` FFI.
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn send_sigusr1(pid: i32) -> Result<(), String> {
    let status = Command::new("kill")
        .arg("-USR1")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match status {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if msg.is_empty() {
                format!("kill exited {}", out.status)
            } else {
                msg
            })
        }
        Err(e) => Err(e.to_string()),
    }
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
    Ok(refresh_repos_with_report(client, meta, filter)
        .await?
        .total_repos)
}

pub async fn refresh_repos_with_report(
    client: &GithubClient,
    meta: &MetaCache,
    filter: &RepoFilter,
) -> Result<RefreshReport> {
    let before = meta.get_repos().context("reading cached repos")?;
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

    let report = build_refresh_report(&before, &repos);
    meta.put_repos(&repos).context("writing repos to cache")?;
    if let Some(e) = etag {
        meta.put_etag(filter.etag_cache_key(), &e)
            .context("writing etag to cache")?;
    }
    Ok(report)
}

fn resolve_token(cli: Option<String>, cfg: &Config) -> Option<Token> {
    cli.filter(|s| !s.is_empty())
        .map(Token::new)
        .or_else(|| cfg.token.clone())
}

fn build_refresh_report(before: &[Repo], after: &[Repo]) -> RefreshReport {
    let before_names: BTreeSet<String> = before.iter().map(repo_full_name).collect();
    let after_names: BTreeSet<String> = after.iter().map(repo_full_name).collect();

    let mut added: Vec<RepoSummary> = after
        .iter()
        .filter(|repo| !before_names.contains(&repo_full_name(repo)))
        .map(RepoSummary::from_repo)
        .collect();
    let mut removed: Vec<RepoSummary> = before
        .iter()
        .filter(|repo| !after_names.contains(&repo_full_name(repo)))
        .map(RepoSummary::from_repo)
        .collect();

    added.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    removed.sort_by(|a, b| a.full_name.cmp(&b.full_name));

    RefreshReport {
        total_repos: after.len(),
        added,
        removed,
    }
}

fn repo_full_name(repo: &Repo) -> String {
    format!("{}/{}", repo.owner.login, repo.name)
}

fn render_refresh_output(
    report: &RefreshReport,
    meta_path: &Path,
    events: &[NotifyEvent],
) -> String {
    let mut out = String::new();
    writeln!(&mut out, "Repo cache refreshed").unwrap();
    writeln!(&mut out, "  cache: {}", meta_path.display()).unwrap();
    writeln!(
        &mut out,
        "  repos: {} total, {} added, {} removed",
        report.total_repos,
        report.added.len(),
        report.removed.len()
    )
    .unwrap();

    if report.added.is_empty() && report.removed.is_empty() {
        writeln!(&mut out).unwrap();
        writeln!(&mut out, "Changes: none").unwrap();
    } else {
        if !report.added.is_empty() {
            writeln!(&mut out).unwrap();
            writeln!(&mut out, "Added repos:").unwrap();
            for repo in &report.added {
                write_repo_summary(&mut out, '+', repo);
            }
        }

        if !report.removed.is_empty() {
            writeln!(&mut out).unwrap();
            writeln!(&mut out, "Removed repos:").unwrap();
            for repo in &report.removed {
                write_repo_summary(&mut out, '-', repo);
            }
        }
    }

    writeln!(&mut out).unwrap();
    writeln!(&mut out, "Live mounts:").unwrap();
    if events.is_empty() {
        writeln!(&mut out, "  none").unwrap();
    } else {
        for ev in events {
            match ev {
                NotifyEvent::Notified { pid, mount_path } => {
                    writeln!(&mut out, "  notified {mount_path} (pid {pid})").unwrap();
                }
                NotifyEvent::ReapedStale { mount_path, pid } => {
                    writeln!(
                        &mut out,
                        "  reaped stale pidfile for {mount_path} (pid {pid} gone)"
                    )
                    .unwrap();
                }
                NotifyEvent::SignalFailed {
                    pid,
                    mount_path,
                    error,
                } => {
                    writeln!(
                        &mut out,
                        "  could not notify {mount_path} (pid {pid}): {error}"
                    )
                    .unwrap();
                }
            }
        }
    }

    out
}

fn write_repo_summary(out: &mut String, marker: char, repo: &RepoSummary) {
    let visibility = if repo.private { "private" } else { "public" };
    let fork = if repo.fork { ", fork" } else { "" };
    let branch = repo.default_branch.as_deref().unwrap_or("<none>");
    match compact_description(repo.description.as_deref()) {
        Some(description) => writeln!(
            out,
            "  {marker} {} ({visibility}{fork}, default: {branch}) - {description}",
            repo.full_name
        )
        .unwrap(),
        None => writeln!(
            out,
            "  {marker} {} ({visibility}{fork}, default: {branch})",
            repo.full_name
        )
        .unwrap(),
    }
}

fn compact_description(description: Option<&str>) -> Option<String> {
    let mut compact = String::new();
    for word in description?.split_whitespace() {
        if !compact.is_empty() {
            compact.push(' ');
        }
        compact.push_str(word);
    }
    if compact.is_empty() {
        return None;
    }

    const MAX_DESCRIPTION_CHARS: usize = 96;
    if compact.chars().count() <= MAX_DESCRIPTION_CHARS {
        return Some(compact);
    }

    let mut truncated: String = compact
        .chars()
        .take(MAX_DESCRIPTION_CHARS.saturating_sub(3))
        .collect();
    truncated.push_str("...");
    Some(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Owner;

    fn repo(id: u64, owner: &str, name: &str) -> Repo {
        Repo {
            id,
            name: name.to_string(),
            full_name: format!("{owner}/{name}"),
            owner: Owner {
                login: owner.to_string(),
                id,
            },
            private: false,
            default_branch: Some("main".to_string()),
            description: None,
            size: 1,
            fork: false,
        }
    }

    #[test]
    fn build_refresh_report_detects_added_and_removed_repos() {
        let before = vec![repo(1, "abdul", "old"), repo(2, "abdul", "stay")];
        let after = vec![repo(2, "abdul", "stay"), repo(3, "org", "new")];

        let report = build_refresh_report(&before, &after);

        assert_eq!(report.total_repos, 2);
        assert_eq!(report.added.len(), 1);
        assert_eq!(report.added[0].full_name, "org/new");
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].full_name, "abdul/old");
    }

    #[test]
    fn render_refresh_output_includes_repo_changes_and_mounts() {
        let report = RefreshReport {
            total_repos: 2,
            added: vec![RepoSummary {
                full_name: "abdul/new".to_string(),
                private: true,
                fork: true,
                default_branch: Some("trunk".to_string()),
                description: Some("newly visible repo".to_string()),
            }],
            removed: vec![RepoSummary {
                full_name: "abdul/old".to_string(),
                private: false,
                fork: false,
                default_branch: Some("main".to_string()),
                description: None,
            }],
        };
        let events = vec![NotifyEvent::Notified {
            pid: 42,
            mount_path: "/mnt/ghfs".to_string(),
        }];

        let rendered = render_refresh_output(&report, Path::new("/tmp/ghfs/meta.db"), &events);

        assert!(rendered.contains("Repo cache refreshed"));
        assert!(rendered.contains("repos: 2 total, 1 added, 1 removed"));
        assert!(
            rendered.contains("+ abdul/new (private, fork, default: trunk) - newly visible repo")
        );
        assert!(rendered.contains("- abdul/old (public, default: main)"));
        assert!(rendered.contains("notified /mnt/ghfs (pid 42)"));
    }

    #[test]
    fn render_refresh_output_reports_no_changes_or_mounts() {
        let report = RefreshReport {
            total_repos: 1,
            added: Vec::new(),
            removed: Vec::new(),
        };

        let rendered = render_refresh_output(&report, Path::new("/tmp/ghfs/meta.db"), &[]);

        assert!(rendered.contains("Changes: none"));
        assert!(rendered.contains("Live mounts:\n  none"));
    }

    #[test]
    fn read_pidfile_parses_two_line_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.pid");
        std::fs::write(&path, "42\n/home/u/ghfs\n").unwrap();
        let rec = read_pidfile(&path).unwrap();
        assert_eq!(rec.pid, 42);
        assert_eq!(rec.mount_path, "/home/u/ghfs");
    }

    #[test]
    fn read_pidfile_handles_pid_only_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.pid");
        std::fs::write(&path, "42\n").unwrap();
        let rec = read_pidfile(&path).unwrap();
        assert_eq!(rec.pid, 42);
        assert_eq!(rec.mount_path, "<unknown>");
    }

    #[test]
    fn read_pidfile_rejects_invalid_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.pid");
        std::fs::write(&path, "not-a-pid\n/x\n").unwrap();
        assert!(read_pidfile(&path).is_none());
    }

    #[test]
    fn read_pidfile_rejects_zero_or_negative_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.pid");
        std::fs::write(&path, "0\n/x\n").unwrap();
        assert!(read_pidfile(&path).is_none());
        std::fs::write(&path, "-1\n/x\n").unwrap();
        assert!(read_pidfile(&path).is_none());
    }

    #[test]
    fn process_alive_for_current_pid() {
        let me: i32 = std::process::id() as i32;
        assert!(process_alive(me));
    }

    #[test]
    fn process_alive_false_for_nonexistent_pid() {
        // 2^22 - 1 is past the typical pid_max on Linux; effectively
        // guaranteed to be free.
        assert!(!process_alive(4_194_303));
    }

    #[test]
    fn notify_live_mounts_reaps_stale_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mounts = dir.path().join("mounts");
        std::fs::create_dir_all(&mounts).unwrap();
        let stale = mounts.join("dead.pid");
        std::fs::write(&stale, "4194303\n/some/path\n").unwrap();

        let events = notify_live_mounts(dir.path());

        assert_eq!(events.len(), 1);
        matches!(events[0], NotifyEvent::ReapedStale { .. });
        assert!(!stale.exists(), "stale pidfile should have been removed");
    }

    #[test]
    fn notify_live_mounts_skips_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let events = notify_live_mounts(dir.path());
        assert!(events.is_empty());
    }
}
