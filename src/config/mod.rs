pub mod token;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::Deserialize;
use thiserror::Error;

use self::token::Token;

const DEFAULT_CACHE_TTL_SECS: u64 = 300;
const DEFAULT_AUTO_REFRESH_INTERVAL_SECS: u64 = 300;
const FALLBACK_CACHE_DIR: &str = "/tmp/ghfs-cache";

/// Which repository owners should appear in the mounted filesystem.
///
/// `SelfOnly` (default) preserves the historical behaviour: only repos owned
/// by the authenticated user. `All` includes every repo the token can see —
/// collaborator repos and organization-member repos in addition to owned
/// ones. `List` restricts the visible set to a fixed allowlist of owner
/// logins (matched case-insensitively against `Repo::owner.login`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Owners {
    #[default]
    SelfOnly,
    All,
    List(Vec<String>),
}

impl Owners {
    /// Returns true iff this filter permits a repo with the given owner login.
    ///
    /// `auth_user_login` is the authenticated user's login, required to
    /// evaluate `SelfOnly` on the client side. Pass `None` when the caller
    /// hasn't resolved `whoami` yet — `SelfOnly` will then deny everything.
    pub fn allows(&self, owner_login: &str, auth_user_login: Option<&str>) -> bool {
        match self {
            Owners::All => true,
            Owners::SelfOnly => auth_user_login
                .map(|u| u.eq_ignore_ascii_case(owner_login))
                .unwrap_or(false),
            Owners::List(list) => list.iter().any(|l| l.eq_ignore_ascii_case(owner_login)),
        }
    }

    fn from_preset_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "self-only" | "self_only" | "selfonly" => Owners::SelfOnly,
            "all" => Owners::All,
            // A bare non-preset string is treated as a single-owner allowlist;
            // it would otherwise be silently ignored and confuse the user.
            other => Owners::List(vec![other.to_string()]),
        }
    }

    fn parse_env(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if !trimmed.contains(',') {
            return Some(Self::from_preset_str(trimmed));
        }
        let list: Vec<String> = trimmed
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if list.is_empty() {
            None
        } else {
            Some(Owners::List(list))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OwnersFile {
    /// `owners = "self-only"` / `owners = "all"` / `owners = "abdulrahman"`.
    Preset(String),
    /// `owners = ["abdulrahman", "rust-lang"]`.
    List(Vec<String>),
}

impl From<OwnersFile> for Owners {
    fn from(f: OwnersFile) -> Self {
        match f {
            OwnersFile::Preset(s) => Owners::from_preset_str(&s),
            OwnersFile::List(list) => {
                let list: Vec<String> = list
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if list.is_empty() {
                    Owners::default()
                } else {
                    Owners::List(list)
                }
            }
        }
    }
}

/// Filter repos by their GitHub visibility flag.
///
/// `All` (default) keeps both public and private repos — historical behaviour.
/// `PublicOnly` keeps repos with `private = false`; `PrivateOnly` keeps repos
/// with `private = true`. The check is applied client-side after the fetch,
/// alongside the existing fork/owner filters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Visibility {
    #[default]
    All,
    PublicOnly,
    PrivateOnly,
}

impl Visibility {
    /// True iff this filter permits a repo with the given private flag.
    pub fn allows(&self, private: bool) -> bool {
        match self {
            Visibility::All => true,
            Visibility::PublicOnly => !private,
            Visibility::PrivateOnly => private,
        }
    }

    fn from_str_opt(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "any" | "both" => Some(Self::All),
            "public" | "public-only" | "public_only" | "publiconly" => Some(Self::PublicOnly),
            "private" | "private-only" | "private_only" | "privateonly" => Some(Self::PrivateOnly),
            _ => None,
        }
    }
}

fn parse_bool_env(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Controls whether (and when) ghfs makes a bare libgit2 clone of a repo
/// alongside the existing GitHub-API path.
///
/// When a clone is present, tree listings and blob reads are served from the
/// local object database (offline-capable, no per-blob HTTP round-trip). The
/// API path remains the fallback when a clone is missing or any libgit2 step
/// fails — so this never *removes* capability, only adds it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneTrigger {
    /// Default: never clone. ghfs behaves exactly as before.
    #[default]
    Never,
    /// Clone the first time a branch's contents are listed (`readdir` on the
    /// branch directory). Browsing into the repo triggers the clone.
    OnList,
    /// Clone the first time a file inside the repo is opened. Listing still
    /// uses the API; only file reads trigger the clone.
    OnRead,
    /// Surface each branch as a *symlink* to a real on-disk worktree under
    /// `<cache>/clones/<owner>/<repo>/<fs_branch_name>/`. The worktree is
    /// materialized synchronously on the first `readlink` (i.e. when the
    /// kernel first needs to walk into the branch). All subsequent ops
    /// inside the branch hit the real filesystem — `git status` works.
    OnAccess,
}

impl CloneTrigger {
    fn from_str_opt(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "never" | "off" | "disabled" => Some(Self::Never),
            "on_list" | "on-list" | "list" => Some(Self::OnList),
            "on_read" | "on-read" | "read" => Some(Self::OnRead),
            "on_access" | "on-access" | "access" => Some(Self::OnAccess),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CloneConfigFile {
    trigger: Option<String>,
}

/// Resolved clone settings. Currently a single field, but kept as a struct so
/// future knobs (clone depth, ref shape) can be added without changing the
/// `Config` shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloneConfig {
    pub trigger: CloneTrigger,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file at {path:?}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file at {path:?}: {source}")]
    ParseFile {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    token: Option<String>,
    mount_path: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    log_level: Option<String>,
    cache_ttl_secs: Option<u64>,
    auto_refresh_interval_secs: Option<u64>,
    owners: Option<OwnersFile>,
    include_forks: Option<bool>,
    visibility: Option<String>,
    clone: Option<CloneConfigFile>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub token: Option<Token>,
    pub mount_path: Option<PathBuf>,
    pub cache_dir: PathBuf,
    pub log_level: Option<String>,
    pub cache_ttl_secs: u64,
    /// How often a running mount re-fetches the repo list in the
    /// background, so repos created on GitHub mid-session appear in the
    /// mount without a restart. Defaults to 5 minutes; set to `0` to
    /// disable. Repeat fetches send `If-None-Match` and usually 304, so
    /// the cost is one cheap conditional request per tick.
    pub auto_refresh_interval_secs: Option<u64>,
    /// Restricts which repo owners are mounted. Defaults to `SelfOnly`.
    pub owners: Owners,
    /// When false (default), fork repos are hidden from the mount. Forks are
    /// typically noise for read-only browsing and the existing REST/GraphQL
    /// repo-listing code already filters them out — this flag lets users opt
    /// back in.
    pub include_forks: bool,
    /// Filters repos by their GitHub `private` flag. Defaults to `All`
    /// (no filtering); set to keep only public or only private repos.
    pub visibility: Visibility,
    /// On-demand libgit2 clone settings.
    pub clone: CloneConfig,
    /// Whether the config file existed on disk. Useful for `whoami` to report
    /// "no config file" vs "config file present but no token".
    pub config_file_present: bool,
}

impl Config {
    /// Load config from the default file path + the process env.
    pub fn load() -> Result<Self, ConfigError> {
        let path = default_config_path();
        let (file, present) = match &path {
            Some(p) if p.exists() => (load_file(p)?, true),
            _ => (ConfigFile::default(), false),
        };
        let env: HashMap<String, String> = std::env::vars().collect();
        Ok(resolve(file, &env, present))
    }
}

fn load_file(path: &Path) -> Result<ConfigFile, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&raw).map_err(|source| ConfigError::ParseFile {
        path: path.to_path_buf(),
        source,
    })
}

/// Pure resolver — combines a parsed config file with env vars to produce the
/// final `Config`. Env wins over file. CLI flags (passed separately by the
/// caller at the point of use) win over both.
fn resolve(file: ConfigFile, env: &HashMap<String, String>, file_present: bool) -> Config {
    let token = env
        .get("GHFS_TOKEN")
        .cloned()
        .or_else(|| env.get("GITHUB_TOKEN").cloned())
        .or(file.token)
        .filter(|s| !s.is_empty())
        .map(Token::new);

    let cache_dir = env
        .get("GHFS_CACHE_DIR")
        .map(PathBuf::from)
        .or(file.cache_dir)
        .unwrap_or_else(default_cache_dir);

    let mount_path = env
        .get("GHFS_MOUNT_PATH")
        .map(PathBuf::from)
        .or(file.mount_path);

    let log_level = env.get("GHFS_LOG_LEVEL").cloned().or(file.log_level);

    let cache_ttl_secs = env
        .get("GHFS_CACHE_TTL_SECS")
        .and_then(|s| s.parse().ok())
        .or(file.cache_ttl_secs)
        .unwrap_or(DEFAULT_CACHE_TTL_SECS);

    // `0` is the explicit "disabled" sentinel — distinguished from
    // "user didn't set it" so the default doesn't override a deliberate
    // opt-out.
    let auto_refresh_interval_secs = env
        .get("GHFS_AUTO_REFRESH_INTERVAL_SECS")
        .and_then(|s| s.parse().ok())
        .or(file.auto_refresh_interval_secs)
        .unwrap_or(DEFAULT_AUTO_REFRESH_INTERVAL_SECS);
    let auto_refresh_interval_secs = if auto_refresh_interval_secs == 0 {
        None
    } else {
        Some(auto_refresh_interval_secs)
    };

    let owners = env
        .get("GHFS_OWNERS")
        .and_then(|s| Owners::parse_env(s))
        .or_else(|| file.owners.map(Owners::from))
        .unwrap_or_default();

    let include_forks = env
        .get("GHFS_INCLUDE_FORKS")
        .and_then(|s| parse_bool_env(s))
        .or(file.include_forks)
        .unwrap_or(false);

    let visibility = env
        .get("GHFS_VISIBILITY")
        .and_then(|s| Visibility::from_str_opt(s))
        .or_else(|| {
            file.visibility
                .as_deref()
                .and_then(Visibility::from_str_opt)
        })
        .unwrap_or_default();

    let clone_trigger = env
        .get("GHFS_CLONE_TRIGGER")
        .and_then(|s| CloneTrigger::from_str_opt(s))
        .or_else(|| {
            file.clone
                .as_ref()
                .and_then(|c| c.trigger.as_deref())
                .and_then(CloneTrigger::from_str_opt)
        })
        .unwrap_or_default();

    Config {
        token,
        mount_path,
        cache_dir,
        log_level,
        cache_ttl_secs,
        auto_refresh_interval_secs,
        owners,
        include_forks,
        visibility,
        clone: CloneConfig {
            trigger: clone_trigger,
        },
        config_file_present: file_present,
    }
}

fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "ghfs").map(|p| p.config_dir().join("config.toml"))
}

fn default_cache_dir() -> PathBuf {
    ProjectDirs::from("", "", "ghfs")
        .map(|p| p.cache_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(FALLBACK_CACHE_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn parses_empty_config_file() {
        let cf: ConfigFile = toml::from_str("").unwrap();
        assert!(cf.token.is_none());
        assert!(cf.cache_dir.is_none());
    }

    #[test]
    fn parses_full_config_file() {
        let toml_src = r#"
            token = "ghp_xxx"
            mount_path = "/home/u/ghfs"
            cache_dir = "/home/u/.cache/ghfs"
            log_level = "debug"
            cache_ttl_secs = 600
            owners = ["abdulrahman", "rust-lang"]
            include_forks = true
        "#;
        let cf: ConfigFile = toml::from_str(toml_src).unwrap();
        assert_eq!(cf.token.as_deref(), Some("ghp_xxx"));
        assert_eq!(cf.cache_dir, Some(PathBuf::from("/home/u/.cache/ghfs")));
        assert_eq!(cf.cache_ttl_secs, Some(600));
        assert_eq!(cf.include_forks, Some(true));
        let cfg = resolve(cf, &HashMap::new(), true);
        assert_eq!(
            cfg.owners,
            Owners::List(vec!["abdulrahman".into(), "rust-lang".into()])
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        let res: Result<ConfigFile, _> = toml::from_str(r#"unknown_key = 1"#);
        assert!(
            res.is_err(),
            "should reject typo'd keys to avoid silent misconfig"
        );
    }

    #[test]
    fn ghfs_token_env_wins_over_file() {
        let file = ConfigFile {
            token: Some("from-file".into()),
            ..ConfigFile::default()
        };
        let cfg = resolve(file, &env(&[("GHFS_TOKEN", "from-env")]), true);
        assert_eq!(cfg.token.unwrap().expose(), "from-env");
    }

    #[test]
    fn github_token_env_used_when_ghfs_token_absent() {
        let cfg = resolve(
            ConfigFile::default(),
            &env(&[("GITHUB_TOKEN", "from-gh-env")]),
            false,
        );
        assert_eq!(cfg.token.unwrap().expose(), "from-gh-env");
    }

    #[test]
    fn ghfs_token_beats_github_token_env() {
        let cfg = resolve(
            ConfigFile::default(),
            &env(&[("GHFS_TOKEN", "a"), ("GITHUB_TOKEN", "b")]),
            false,
        );
        assert_eq!(cfg.token.unwrap().expose(), "a");
    }

    #[test]
    fn file_token_used_when_no_env() {
        let file = ConfigFile {
            token: Some("from-file".into()),
            ..Default::default()
        };
        let cfg = resolve(file, &HashMap::new(), true);
        assert_eq!(cfg.token.unwrap().expose(), "from-file");
    }

    #[test]
    fn empty_string_token_treated_as_none() {
        let cfg = resolve(ConfigFile::default(), &env(&[("GHFS_TOKEN", "")]), false);
        assert!(cfg.token.is_none());
    }

    #[test]
    fn defaults_when_nothing_set() {
        let cfg = resolve(ConfigFile::default(), &HashMap::new(), false);
        assert!(cfg.token.is_none());
        assert!(cfg.mount_path.is_none());
        assert_eq!(cfg.cache_ttl_secs, DEFAULT_CACHE_TTL_SECS);
        // cache_dir resolves either to a ProjectDirs path or the fallback —
        // both are absolute and non-empty.
        assert!(!cfg.cache_dir.as_os_str().is_empty());
    }

    #[test]
    fn auto_refresh_defaults_to_five_minutes() {
        let cfg = resolve(ConfigFile::default(), &HashMap::new(), false);
        assert_eq!(
            cfg.auto_refresh_interval_secs,
            Some(DEFAULT_AUTO_REFRESH_INTERVAL_SECS)
        );
    }

    #[test]
    fn auto_refresh_parses_from_toml() {
        let cf: ConfigFile = toml::from_str(r#"auto_refresh_interval_secs = 60"#).unwrap();
        let cfg = resolve(cf, &HashMap::new(), true);
        assert_eq!(cfg.auto_refresh_interval_secs, Some(60));
    }

    #[test]
    fn auto_refresh_env_overrides_file() {
        let cf: ConfigFile = toml::from_str(r#"auto_refresh_interval_secs = 60"#).unwrap();
        let cfg = resolve(
            cf,
            &env(&[("GHFS_AUTO_REFRESH_INTERVAL_SECS", "120")]),
            true,
        );
        assert_eq!(cfg.auto_refresh_interval_secs, Some(120));
    }

    #[test]
    fn auto_refresh_zero_is_disabled() {
        let cf: ConfigFile = toml::from_str(r#"auto_refresh_interval_secs = 0"#).unwrap();
        let cfg = resolve(cf, &HashMap::new(), true);
        assert_eq!(cfg.auto_refresh_interval_secs, None);
    }

    #[test]
    fn auto_refresh_env_unparseable_falls_back_to_file() {
        let cf: ConfigFile = toml::from_str(r#"auto_refresh_interval_secs = 90"#).unwrap();
        let cfg = resolve(
            cf,
            &env(&[("GHFS_AUTO_REFRESH_INTERVAL_SECS", "nope")]),
            true,
        );
        assert_eq!(cfg.auto_refresh_interval_secs, Some(90));
    }

    #[test]
    fn cache_ttl_env_overrides_file() {
        let file = ConfigFile {
            cache_ttl_secs: Some(60),
            ..Default::default()
        };
        let cfg = resolve(file, &env(&[("GHFS_CACHE_TTL_SECS", "999")]), true);
        assert_eq!(cfg.cache_ttl_secs, 999);
    }

    #[test]
    fn cache_ttl_env_falls_back_when_unparseable() {
        let file = ConfigFile {
            cache_ttl_secs: Some(60),
            ..Default::default()
        };
        let cfg = resolve(file, &env(&[("GHFS_CACHE_TTL_SECS", "not-a-number")]), true);
        assert_eq!(cfg.cache_ttl_secs, 60);
    }

    #[test]
    fn owners_defaults_to_self_only() {
        let cfg = resolve(ConfigFile::default(), &HashMap::new(), false);
        assert_eq!(cfg.owners, Owners::SelfOnly);
        assert!(!cfg.include_forks);
    }

    #[test]
    fn owners_parses_preset_strings_from_toml() {
        for (raw, expected) in [
            (r#"owners = "self-only""#, Owners::SelfOnly),
            (r#"owners = "self_only""#, Owners::SelfOnly),
            (r#"owners = "all""#, Owners::All),
        ] {
            let cf: ConfigFile = toml::from_str(raw).unwrap();
            let cfg = resolve(cf, &HashMap::new(), true);
            assert_eq!(cfg.owners, expected, "raw: {raw}");
        }
    }

    #[test]
    fn owners_parses_list_from_toml() {
        let cf: ConfigFile = toml::from_str(r#"owners = ["abdulrahman", "rust-lang"]"#).unwrap();
        let cfg = resolve(cf, &HashMap::new(), true);
        assert_eq!(
            cfg.owners,
            Owners::List(vec!["abdulrahman".into(), "rust-lang".into()])
        );
    }

    #[test]
    fn owners_bare_string_is_single_element_list() {
        // Letting `owners = "abdulrahman"` mean "just that owner" avoids the
        // footgun where a typo'd preset silently falls back to SelfOnly.
        let cf: ConfigFile = toml::from_str(r#"owners = "abdulrahman""#).unwrap();
        let cfg = resolve(cf, &HashMap::new(), true);
        assert_eq!(cfg.owners, Owners::List(vec!["abdulrahman".into()]));
    }

    #[test]
    fn owners_empty_list_falls_back_to_default() {
        let cf: ConfigFile = toml::from_str(r#"owners = []"#).unwrap();
        let cfg = resolve(cf, &HashMap::new(), true);
        assert_eq!(cfg.owners, Owners::SelfOnly);
    }

    #[test]
    fn owners_env_overrides_file_preset() {
        let cf: ConfigFile = toml::from_str(r#"owners = "self-only""#).unwrap();
        let cfg = resolve(cf, &env(&[("GHFS_OWNERS", "all")]), true);
        assert_eq!(cfg.owners, Owners::All);
    }

    #[test]
    fn owners_env_comma_list() {
        let cfg = resolve(
            ConfigFile::default(),
            &env(&[("GHFS_OWNERS", "abdulrahman, rust-lang ,  ")]),
            false,
        );
        assert_eq!(
            cfg.owners,
            Owners::List(vec!["abdulrahman".into(), "rust-lang".into()])
        );
    }

    #[test]
    fn owners_env_single_value_treated_as_list() {
        let cfg = resolve(
            ConfigFile::default(),
            &env(&[("GHFS_OWNERS", "abdulrahman")]),
            false,
        );
        assert_eq!(cfg.owners, Owners::List(vec!["abdulrahman".into()]));
    }

    #[test]
    fn include_forks_parses_from_toml() {
        let cf: ConfigFile = toml::from_str(r#"include_forks = true"#).unwrap();
        let cfg = resolve(cf, &HashMap::new(), true);
        assert!(cfg.include_forks);
    }

    #[test]
    fn include_forks_env_overrides_file() {
        let cf: ConfigFile = toml::from_str(r#"include_forks = false"#).unwrap();
        let cfg = resolve(cf, &env(&[("GHFS_INCLUDE_FORKS", "1")]), true);
        assert!(cfg.include_forks);
    }

    #[test]
    fn include_forks_env_off() {
        let cf: ConfigFile = toml::from_str(r#"include_forks = true"#).unwrap();
        let cfg = resolve(cf, &env(&[("GHFS_INCLUDE_FORKS", "off")]), true);
        assert!(!cfg.include_forks);
    }

    #[test]
    fn include_forks_env_unparseable_falls_back_to_file() {
        let cf: ConfigFile = toml::from_str(r#"include_forks = true"#).unwrap();
        let cfg = resolve(cf, &env(&[("GHFS_INCLUDE_FORKS", "maybe")]), true);
        assert!(cfg.include_forks);
    }

    #[test]
    fn owners_allows_self_only() {
        let f = Owners::SelfOnly;
        assert!(f.allows("alice", Some("alice")));
        assert!(f.allows("Alice", Some("alice")));
        assert!(!f.allows("bob", Some("alice")));
        assert!(!f.allows("alice", None));
    }

    #[test]
    fn owners_allows_all() {
        let f = Owners::All;
        assert!(f.allows("alice", Some("alice")));
        assert!(f.allows("bob", Some("alice")));
        assert!(f.allows("anyone", None));
    }

    #[test]
    fn clone_trigger_defaults_to_never() {
        let cfg = resolve(ConfigFile::default(), &HashMap::new(), false);
        assert_eq!(cfg.clone.trigger, CloneTrigger::Never);
    }

    #[test]
    fn clone_trigger_parses_from_toml() {
        for (raw, expected) in [
            (
                r#"[clone]
trigger = "never""#,
                CloneTrigger::Never,
            ),
            (
                r#"[clone]
trigger = "on_list""#,
                CloneTrigger::OnList,
            ),
            (
                r#"[clone]
trigger = "on_read""#,
                CloneTrigger::OnRead,
            ),
            (
                r#"[clone]
trigger = "on-list""#,
                CloneTrigger::OnList,
            ),
            (
                r#"[clone]
trigger = "disabled""#,
                CloneTrigger::Never,
            ),
            (
                r#"[clone]
trigger = "on_access""#,
                CloneTrigger::OnAccess,
            ),
            (
                r#"[clone]
trigger = "on-access""#,
                CloneTrigger::OnAccess,
            ),
            (
                r#"[clone]
trigger = "access""#,
                CloneTrigger::OnAccess,
            ),
        ] {
            let cf: ConfigFile = toml::from_str(raw).unwrap();
            let cfg = resolve(cf, &HashMap::new(), true);
            assert_eq!(cfg.clone.trigger, expected, "raw: {raw}");
        }
    }

    #[test]
    fn clone_trigger_env_overrides_file() {
        let cf: ConfigFile = toml::from_str(
            r#"[clone]
trigger = "never""#,
        )
        .unwrap();
        let cfg = resolve(cf, &env(&[("GHFS_CLONE_TRIGGER", "on_read")]), true);
        assert_eq!(cfg.clone.trigger, CloneTrigger::OnRead);
    }

    #[test]
    fn clone_trigger_unparseable_env_falls_back_to_file() {
        let cf: ConfigFile = toml::from_str(
            r#"[clone]
trigger = "on_list""#,
        )
        .unwrap();
        let cfg = resolve(cf, &env(&[("GHFS_CLONE_TRIGGER", "tomorrow")]), true);
        assert_eq!(cfg.clone.trigger, CloneTrigger::OnList);
    }

    #[test]
    fn visibility_defaults_to_all() {
        let cfg = resolve(ConfigFile::default(), &HashMap::new(), false);
        assert_eq!(cfg.visibility, Visibility::All);
    }

    #[test]
    fn visibility_parses_from_toml() {
        for (raw, expected) in [
            (r#"visibility = "all""#, Visibility::All),
            (r#"visibility = "public""#, Visibility::PublicOnly),
            (r#"visibility = "public-only""#, Visibility::PublicOnly),
            (r#"visibility = "private""#, Visibility::PrivateOnly),
            (r#"visibility = "private_only""#, Visibility::PrivateOnly),
        ] {
            let cf: ConfigFile = toml::from_str(raw).unwrap();
            let cfg = resolve(cf, &HashMap::new(), true);
            assert_eq!(cfg.visibility, expected, "raw: {raw}");
        }
    }

    #[test]
    fn visibility_env_overrides_file() {
        let cf: ConfigFile = toml::from_str(r#"visibility = "all""#).unwrap();
        let cfg = resolve(cf, &env(&[("GHFS_VISIBILITY", "private")]), true);
        assert_eq!(cfg.visibility, Visibility::PrivateOnly);
    }

    #[test]
    fn visibility_unparseable_env_falls_back_to_file() {
        let cf: ConfigFile = toml::from_str(r#"visibility = "public""#).unwrap();
        let cfg = resolve(cf, &env(&[("GHFS_VISIBILITY", "maybe")]), true);
        assert_eq!(cfg.visibility, Visibility::PublicOnly);
    }

    #[test]
    fn visibility_allows_matches_private_flag() {
        assert!(Visibility::All.allows(true));
        assert!(Visibility::All.allows(false));
        assert!(Visibility::PublicOnly.allows(false));
        assert!(!Visibility::PublicOnly.allows(true));
        assert!(Visibility::PrivateOnly.allows(true));
        assert!(!Visibility::PrivateOnly.allows(false));
    }

    #[test]
    fn owners_allows_list_case_insensitive() {
        let f = Owners::List(vec!["Rust-Lang".into(), "abdulrahman".into()]);
        assert!(f.allows("rust-lang", Some("alice")));
        assert!(f.allows("Abdulrahman", None));
        assert!(!f.allows("someone-else", Some("alice")));
    }
}
