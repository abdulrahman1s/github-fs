use anyhow::{Result, anyhow};
use tracing::info;

use crate::config::{Config, token::Token};
use crate::github::GithubClient;

pub async fn run(cli_token: Option<String>, cfg: Config) -> Result<()> {
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

    info!("calling GET /user");
    let client = GithubClient::new(token)?;
    let user = client.whoami().await?;

    println!("Logged in as: {} (id={})", user.login, user.id);
    if let Some(name) = &user.name {
        println!("Name:         {name}");
    }
    if let Some(email) = &user.email {
        println!("Email:        {email}");
    }
    println!("Profile:      {}", user.html_url);
    Ok(())
}

fn resolve_token(cli: Option<String>, cfg: &Config) -> Option<Token> {
    cli.filter(|s| !s.is_empty())
        .map(Token::new)
        .or_else(|| cfg.token.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg_with(token: Option<Token>) -> Config {
        Config {
            token,
            mount_path: None,
            cache_dir: PathBuf::from("/tmp"),
            log_level: None,
            cache_ttl_secs: 0,
            owners: crate::config::Owners::default(),
            include_forks: false,
            visibility: crate::config::Visibility::default(),
            clone: crate::config::CloneConfig::default(),
            config_file_present: false,
        }
    }

    #[test]
    fn cli_token_wins_over_config() {
        let cfg = cfg_with(Some(Token::new("from-cfg")));
        let resolved = resolve_token(Some("from-cli".into()), &cfg).unwrap();
        assert_eq!(resolved.expose(), "from-cli");
    }

    #[test]
    fn falls_back_to_config_when_cli_absent() {
        let cfg = cfg_with(Some(Token::new("from-cfg")));
        let resolved = resolve_token(None, &cfg).unwrap();
        assert_eq!(resolved.expose(), "from-cfg");
    }

    #[test]
    fn empty_cli_token_treated_as_absent() {
        let cfg = cfg_with(Some(Token::new("from-cfg")));
        let resolved = resolve_token(Some(String::new()), &cfg).unwrap();
        assert_eq!(resolved.expose(), "from-cfg");
    }

    #[test]
    fn none_when_nothing_available() {
        let cfg = cfg_with(None);
        assert!(resolve_token(None, &cfg).is_none());
    }
}
