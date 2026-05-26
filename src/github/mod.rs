pub mod errors;
pub mod graphql;
pub mod pagination;
pub mod types;

pub use errors::GithubError;
pub use graphql::{BranchHeadSeed, WarmupRepo};
pub use types::{
    Branch, BranchCommit, BranchSummary, CommitInner, GitRef, Owner, Repo, Tree, TreeEntry,
    TreeEntryKind, User,
};

use bytes::Bytes;
use reqwest::{Client, RequestBuilder, Response, StatusCode, header};
use serde::de::DeserializeOwned;
use tracing::debug;

use crate::cache::{ALL_USER_REPOS_ETAG_KEY, OWNED_USER_REPOS_ETAG_KEY};
use crate::config::{Owners, Visibility, token::Token};

pub const DEFAULT_API_BASE: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("github-fs/", env!("CARGO_PKG_VERSION"));
const API_VERSION: &str = "2022-11-28";
const ACCEPT_JSON: &str = "application/vnd.github+json";
const ACCEPT_RAW: &str = "application/vnd.github.raw";
const PER_PAGE: u32 = 100;

/// Result of a GET that participated in conditional caching (`If-None-Match`).
///
/// When `Modified`, the caller updates its cache with the new body and ETag.
/// When `NotModified`, the caller reuses its previously-cached value (GitHub
/// has confirmed nothing changed, and the 304 did not consume a rate-limit
/// unit).
#[derive(Debug)]
pub enum Conditional<T> {
    Modified { etag: Option<String>, body: T },
    NotModified,
}

impl<T> Conditional<T> {
    pub fn into_modified(self) -> Option<(Option<String>, T)> {
        match self {
            Self::Modified { etag, body } => Some((etag, body)),
            Self::NotModified => None,
        }
    }
}

/// Configures which repositories the user-repo-listing endpoints return.
///
/// `SelfOnly` (default) uses GitHub's `affiliation=owner` server-side filter
/// — cheap, exact, and what the codebase did before this filter existed.
/// `All` and `List(_)` widen the fetch to all affiliations the token can
/// see; `List(_)` additionally filters client-side by owner login. `fork`
/// filtering is always client-side because GitHub's REST list endpoint has
/// no server-side fork filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoFilter {
    pub owners: Owners,
    pub include_forks: bool,
    /// Public/private toggle. Defaults to `All` (no filtering).
    pub visibility: Visibility,
}

impl RepoFilter {
    pub fn new(owners: Owners, include_forks: bool) -> Self {
        Self {
            owners,
            include_forks,
            visibility: Visibility::default(),
        }
    }

    /// Builder-style setter so existing two-arg call sites keep working
    /// (visibility defaults to `All`) and only the ones that care opt in.
    pub fn with_visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Whether this filter narrows the fetch to owned repos only. Used to
    /// pick the server-side affiliation param and the matching ETag cache
    /// key. `List(_)` returns false because its fetch URL must be wide;
    /// the allowlist is applied client-side after the fact.
    fn is_owned_only(&self) -> bool {
        matches!(self.owners, Owners::SelfOnly)
    }

    /// Cache key for the ETag of the fetch this filter produces. Two URL
    /// shapes => two key spaces.
    pub fn etag_cache_key(&self) -> &'static str {
        if self.is_owned_only() {
            OWNED_USER_REPOS_ETAG_KEY
        } else {
            ALL_USER_REPOS_ETAG_KEY
        }
    }

    /// Drops repos rejected by the filter from `repos` in place. Visibility
    /// and fork filters are always client-side; the owner filter is too,
    /// except for the cheap `SelfOnly` case that narrows server-side via
    /// `affiliation=owner`.
    pub fn retain(&self, repos: &mut Vec<Repo>) {
        let include_forks = self.include_forks;
        let visibility = self.visibility;
        match &self.owners {
            Owners::SelfOnly | Owners::All => {
                repos.retain(|r| (include_forks || !r.fork) && visibility.allows(r.private));
            }
            Owners::List(list) => {
                repos.retain(|r| {
                    (include_forks || !r.fork)
                        && visibility.allows(r.private)
                        && list.iter().any(|l| l.eq_ignore_ascii_case(&r.owner.login))
                });
            }
        }
    }
}

pub struct GithubClient {
    http: Client,
    token: Token,
    base: String,
}

impl GithubClient {
    pub fn new(token: Token) -> Result<Self, GithubError> {
        Self::with_base(token, DEFAULT_API_BASE.to_string())
    }

    pub fn with_base(token: Token, base: String) -> Result<Self, GithubError> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(GithubError::Build)?;
        Ok(Self {
            http,
            token,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    /// `GET /user` — returns the authenticated user.
    pub async fn whoami(&self) -> Result<User, GithubError> {
        let url = format!("{}/user", self.base);
        let (_, user) = self
            .get_json_conditional::<User>(&url, ACCEPT_JSON, None)
            .await?
            .into_modified()
            .expect("server cannot return 304 when no If-None-Match was sent");
        Ok(user)
    }

    /// `GET /user/repos?[affiliation=owner&]per_page=100` — paginates via
    /// `Link: rel="next"`.
    ///
    /// `etag` is sent as `If-None-Match` *only on page 1*. If GitHub responds
    /// 304 to page 1, we return `Conditional::NotModified` and the caller
    /// reuses its cached list. Otherwise we walk all pages and return the
    /// concatenated list plus the ETag of page 1.
    ///
    /// `filter` controls both the URL (whether `affiliation=owner` is sent
    /// — narrows server-side to owned repos) and the post-fetch filtering
    /// (fork hiding and owner-allowlist trimming).
    pub async fn list_user_repos(
        &self,
        etag: Option<&str>,
        filter: &RepoFilter,
    ) -> Result<Conditional<Vec<Repo>>, GithubError> {
        // `affiliation=owner` is the historical owned-only fetch. Omitting the
        // param lets GitHub's default (owner + collaborator + organization
        // member) widen the result set for `All` / `List`.
        let first_url = if filter.is_owned_only() {
            format!(
                "{}/user/repos?affiliation=owner&per_page={}",
                self.base, PER_PAGE
            )
        } else {
            format!("{}/user/repos?per_page={}", self.base, PER_PAGE)
        };
        let mut url = first_url;
        let mut conditional_etag = etag;
        let mut etag_out: Option<String> = None;
        let mut all: Vec<Repo> = Vec::new();

        loop {
            let resp = self.send_get(&url, ACCEPT_JSON, conditional_etag).await?;
            match resp.status() {
                StatusCode::NOT_MODIFIED => return Ok(Conditional::NotModified),
                StatusCode::OK => {
                    if conditional_etag.is_some() || etag_out.is_none() {
                        etag_out = extract_etag(&resp);
                    }
                    let next = pagination::parse_next_link(resp.headers().get(header::LINK));
                    let page: Vec<Repo> = resp.json().await.map_err(GithubError::Decode)?;
                    all.extend(page);
                    match next {
                        Some(u) => {
                            url = u;
                            // ETag and If-None-Match are per-resource; don't
                            // resend on subsequent pages.
                            conditional_etag = None;
                        }
                        None => {
                            // Apply post-fetch filtering once we have every
                            // page; doing it per-page would require duplicating
                            // the predicate and risks page boundaries.
                            filter.retain(&mut all);
                            return Ok(Conditional::Modified {
                                etag: etag_out,
                                body: all,
                            });
                        }
                    }
                }
                StatusCode::UNAUTHORIZED => return Err(GithubError::Unauthorized),
                StatusCode::FORBIDDEN => return Err(classify_forbidden(resp).await),
                StatusCode::NOT_FOUND => return Err(GithubError::NotFound),
                other => return Err(unexpected(resp, other).await),
            }
        }
    }

    /// `GET /repos/{owner}/{repo}/branches/{branch}` — returns the branch with
    /// the HEAD commit (and tree) SHA, enabling tree fetches without a second
    /// round-trip to resolve the ref.
    pub async fn get_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        etag: Option<&str>,
    ) -> Result<Conditional<Branch>, GithubError> {
        let branch = encode_path_segment(branch);
        let url = format!("{}/repos/{}/{}/branches/{}", self.base, owner, repo, branch);
        self.get_json_conditional::<Branch>(&url, ACCEPT_JSON, etag)
            .await
    }

    /// `GET /repos/{owner}/{repo}/branches?per_page=100` — paginates via
    /// `Link: rel="next"`.
    pub async fn list_branches(
        &self,
        owner: &str,
        repo: &str,
        etag: Option<&str>,
    ) -> Result<Conditional<Vec<BranchSummary>>, GithubError> {
        let first_url = format!(
            "{}/repos/{}/{}/branches?per_page={}",
            self.base, owner, repo, PER_PAGE
        );
        let mut url = first_url;
        let mut conditional_etag = etag;
        let mut etag_out: Option<String> = None;
        let mut all: Vec<BranchSummary> = Vec::new();

        loop {
            let resp = self.send_get(&url, ACCEPT_JSON, conditional_etag).await?;
            match resp.status() {
                StatusCode::NOT_MODIFIED => return Ok(Conditional::NotModified),
                StatusCode::OK => {
                    if conditional_etag.is_some() || etag_out.is_none() {
                        etag_out = extract_etag(&resp);
                    }
                    let next = pagination::parse_next_link(resp.headers().get(header::LINK));
                    let mut page: Vec<BranchSummary> =
                        resp.json().await.map_err(GithubError::Decode)?;
                    all.append(&mut page);
                    match next {
                        Some(u) => {
                            url = u;
                            conditional_etag = None;
                        }
                        None => {
                            return Ok(Conditional::Modified {
                                etag: etag_out,
                                body: all,
                            });
                        }
                    }
                }
                StatusCode::UNAUTHORIZED => return Err(GithubError::Unauthorized),
                StatusCode::FORBIDDEN => return Err(classify_forbidden(resp).await),
                StatusCode::NOT_FOUND => return Err(GithubError::NotFound),
                other => return Err(unexpected(resp, other).await),
            }
        }
    }

    /// `GET /repos/{owner}/{repo}/git/trees/{sha}[?recursive=1]`.
    ///
    /// `sha` may be a tree SHA or a commit SHA. With `recursive=true` GitHub
    /// returns the entire tree in one call — much cheaper than walking
    /// directories. Beware `Tree::truncated`: GitHub may bail at ~100k entries
    /// or ~7MB.
    pub async fn get_tree(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
        recursive: bool,
        etag: Option<&str>,
    ) -> Result<Conditional<Tree>, GithubError> {
        let mut url = format!("{}/repos/{}/{}/git/trees/{}", self.base, owner, repo, sha);
        if recursive {
            url.push_str("?recursive=1");
        }
        self.get_json_conditional::<Tree>(&url, ACCEPT_JSON, etag)
            .await
    }

    /// `GET /repos/{owner}/{repo}/git/blobs/{sha}` with `Accept:
    /// application/vnd.github.raw`. Returns the raw bytes of the blob.
    ///
    /// No ETag handling: blob contents are identified by their git SHA, which
    /// is a content hash — the bytes for a given SHA can never change.
    pub async fn get_blob_raw(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Bytes, GithubError> {
        let url = format!("{}/repos/{}/{}/git/blobs/{}", self.base, owner, repo, sha);
        let resp = self.send_get(&url, ACCEPT_RAW, None).await?;
        let status = resp.status();
        match status {
            StatusCode::OK => resp.bytes().await.map_err(GithubError::Decode),
            StatusCode::UNAUTHORIZED => Err(GithubError::Unauthorized),
            StatusCode::FORBIDDEN => Err(classify_forbidden(resp).await),
            StatusCode::NOT_FOUND => Err(GithubError::NotFound),
            other => Err(unexpected(resp, other).await),
        }
    }

    // --- shared helpers ---

    async fn send_get(
        &self,
        url: &str,
        accept: &str,
        etag: Option<&str>,
    ) -> Result<Response, GithubError> {
        debug!(target: "ghfs", url, "GET");
        let mut req: RequestBuilder = self
            .http
            .get(url)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.token.expose()),
            )
            .header(header::ACCEPT, accept)
            .header("X-GitHub-Api-Version", API_VERSION);
        if let Some(tag) = etag {
            req = req.header(header::IF_NONE_MATCH, tag);
        }
        req.send().await.map_err(GithubError::Request)
    }

    async fn get_json_conditional<T: DeserializeOwned>(
        &self,
        url: &str,
        accept: &str,
        etag: Option<&str>,
    ) -> Result<Conditional<T>, GithubError> {
        let resp = self.send_get(url, accept, etag).await?;
        let status = resp.status();
        match status {
            StatusCode::OK => {
                let new_etag = extract_etag(&resp);
                let body: T = resp.json().await.map_err(GithubError::Decode)?;
                Ok(Conditional::Modified {
                    etag: new_etag,
                    body,
                })
            }
            StatusCode::NOT_MODIFIED => Ok(Conditional::NotModified),
            StatusCode::UNAUTHORIZED => Err(GithubError::Unauthorized),
            StatusCode::FORBIDDEN => Err(classify_forbidden(resp).await),
            StatusCode::NOT_FOUND => Err(GithubError::NotFound),
            other => Err(unexpected(resp, other).await),
        }
    }
}

fn extract_etag(resp: &Response) -> Option<String> {
    resp.headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(&mut out, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    out
}

async fn unexpected(resp: Response, status: StatusCode) -> GithubError {
    let body = resp.text().await.unwrap_or_default();
    GithubError::Unexpected {
        status: status.as_u16(),
        body,
    }
}

async fn classify_forbidden(resp: Response) -> GithubError {
    let remaining = resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let reset = resp
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let body = resp.text().await.unwrap_or_default();

    if remaining.as_deref() == Some("0") {
        GithubError::RateLimited { reset_unix: reset }
    } else {
        GithubError::Forbidden(body)
    }
}
