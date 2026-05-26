//! GraphQL fast-paths for the warmup and per-repo branch listing.
//!
//! GitHub's GraphQL API has no conditional-request (`If-None-Match`) support,
//! so it is unsuitable for any refresh/poll path. The hybrid strategy here is
//! narrow: use GraphQL only when REST would be N+1 (warmup of the repo list
//! plus per-repo default-branch HEAD) or when REST payloads are needlessly
//! fat for the data we actually consume (branch listings — we only need name
//! + commit SHA + tree SHA).

use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use tracing::debug;

use super::{API_VERSION, ACCEPT_JSON, GithubClient, GithubError, Owner, Repo, RepoFilter,
            classify_forbidden, unexpected};
use crate::config::Owners;

/// Seed for a single (repo, branch) HEAD, sourced from GraphQL. The tree SHA
/// is what callers actually want — having it pre-resolved lets us skip the
/// REST `get_branch` round-trip when the user navigates into a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchHeadSeed {
    pub branch: String,
    pub commit_sha: String,
    pub tree_sha: String,
}

/// One repo's worth of warmup data: the `Repo` payload, its default branch
/// HEAD pre-resolved to a tree SHA, *and* the first page of all its other
/// branches (also resolved). For repos with more branches than fit in one
/// page, `branches_complete` is false and callers must fall back to
/// `list_branches_graphql` to enumerate the full set.
#[derive(Debug, Clone)]
pub struct WarmupRepo {
    pub repo: Repo,
    pub default_branch_head: Option<BranchHeadSeed>,
    pub branches: Vec<BranchHeadSeed>,
    pub branches_complete: bool,
}

// ---- request/response wire types ----

#[derive(Serialize)]
struct GqlRequest<'a, V: Serialize> {
    query: &'a str,
    variables: V,
}

#[derive(Deserialize)]
struct GqlResponse<D> {
    data: Option<D>,
    errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
struct GqlError {
    message: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct WarmupData {
    viewer: WarmupViewer,
}

#[derive(Deserialize)]
struct WarmupViewer {
    repositories: WarmupRepoConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WarmupRepoConnection {
    page_info: PageInfo,
    nodes: Vec<WarmupRepoNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WarmupRepoNode {
    database_id: u64,
    name: String,
    name_with_owner: String,
    is_private: bool,
    #[serde(default)]
    is_fork: bool,
    disk_usage: Option<u64>,
    description: Option<String>,
    owner: WarmupOwner,
    default_branch_ref: Option<WarmupRef>,
    refs: WarmupRefConnection,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WarmupRefConnection {
    #[serde(default)]
    page_info: PageInfo,
    #[serde(default)]
    nodes: Vec<WarmupRef>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WarmupOwner {
    login: String,
    database_id: Option<u64>,
}

#[derive(Deserialize)]
struct WarmupRef {
    name: String,
    target: Option<WarmupCommit>,
}

#[derive(Deserialize)]
struct WarmupCommit {
    oid: Option<String>,
    tree: Option<WarmupTreeRef>,
}

#[derive(Deserialize)]
struct WarmupTreeRef {
    oid: String,
}

#[derive(Deserialize)]
struct BranchesData {
    repository: Option<BranchesRepo>,
}

#[derive(Deserialize)]
struct BranchesRepo {
    refs: BranchesRefConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchesRefConnection {
    page_info: PageInfo,
    nodes: Vec<WarmupRef>,
}

#[derive(Serialize)]
struct WarmupVars<'a> {
    cursor: Option<&'a str>,
}

#[derive(Serialize)]
struct BranchesVars<'a> {
    owner: &'a str,
    repo: &'a str,
    cursor: Option<&'a str>,
}

// Bulk warmup: per outer page of 50 repos we also pull each repo's first 100
// branches inline. Outer × inner = 5000 nodes per request, well within
// GraphQL's per-query node limit. Repos with >100 branches surface
// `branches_complete = false` and the caller falls back to
// `list_branches_graphql` for the few outliers.
//
// The `repositories(...)` args are built from the active `RepoFilter`:
// `ownerAffiliations: [OWNER]` for `SelfOnly` (narrowed server-side); omitted
// otherwise to get GitHub's default (OWNER + COLLABORATOR + ORGANIZATION_MEMBER).
// `isFork: false` is sent when forks are excluded; omitted otherwise.
// `Owners::List` is enforced client-side after the fetch.
fn build_warmup_query(filter: &RepoFilter) -> String {
    let owner_aff = if matches!(filter.owners, Owners::SelfOnly) {
        "ownerAffiliations: [OWNER], "
    } else {
        ""
    };
    let fork_arg = if filter.include_forks {
        ""
    } else {
        "isFork: false, "
    };
    format!(
        r#"
query Warmup($cursor: String) {{
  viewer {{
    repositories(first: 50, {owner_aff}{fork_arg}after: $cursor) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{
        databaseId
        name
        nameWithOwner
        isPrivate
        isFork
        diskUsage
        description
        owner {{
          login
          ... on User {{ databaseId }}
          ... on Organization {{ databaseId }}
        }}
        defaultBranchRef {{
          name
          target {{
            ... on Commit {{
              oid
              tree {{ oid }}
            }}
          }}
        }}
        refs(refPrefix: "refs/heads/", first: 100) {{
          pageInfo {{ hasNextPage endCursor }}
          nodes {{
            name
            target {{
              ... on Commit {{
                oid
                tree {{ oid }}
              }}
            }}
          }}
        }}
      }}
    }}
  }}
}}
"#
    )
}

const BRANCHES_QUERY: &str = r#"
query Branches($owner: String!, $repo: String!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    refs(refPrefix: "refs/heads/", first: 100, after: $cursor) {
      pageInfo { hasNextPage endCursor }
      nodes {
        name
        target {
          ... on Commit {
            oid
            tree { oid }
          }
        }
      }
    }
  }
}
"#;

fn seed_from_ref(r: WarmupRef) -> Option<BranchHeadSeed> {
    let commit = r.target?;
    let oid = commit.oid?;
    let tree = commit.tree?.oid;
    Some(BranchHeadSeed {
        branch: r.name,
        commit_sha: oid,
        tree_sha: tree,
    })
}

impl GithubClient {
    /// One GraphQL round-trip (per 50 repos) that replaces both
    /// `list_user_repos` and the per-repo `get_branch` calls used to resolve
    /// each default branch to a tree SHA.
    ///
    /// `filter` shapes the GraphQL query: `SelfOnly` narrows to
    /// `ownerAffiliations: [OWNER]` server-side; `include_forks = false`
    /// adds `isFork: false`. `Owners::List` is enforced client-side after
    /// the response is parsed (GraphQL has no per-owner filter on
    /// `viewer.repositories`).
    pub async fn warmup_user_repos(
        &self,
        filter: &RepoFilter,
    ) -> Result<Vec<WarmupRepo>, GithubError> {
        let query = build_warmup_query(filter);
        let allow_list: Option<&[String]> = match &filter.owners {
            Owners::List(list) => Some(list.as_slice()),
            _ => None,
        };
        let mut cursor: Option<String> = None;
        let mut out: Vec<WarmupRepo> = Vec::new();
        loop {
            let vars = WarmupVars { cursor: cursor.as_deref() };
            let data: WarmupData = self.post_graphql(&query, &vars).await?;
            let connection = data.viewer.repositories;
            for node in connection.nodes {
                // Defensive client-side fork filter: if GitHub ever drops
                // `isFork:false` we still honour the user's preference.
                if !filter.include_forks && node.is_fork {
                    continue;
                }
                if !filter.visibility.allows(node.is_private) {
                    continue;
                }
                if let Some(list) = allow_list
                    && !list
                        .iter()
                        .any(|l| l.eq_ignore_ascii_case(&node.owner.login))
                {
                    continue;
                }
                let owner_id = node.owner.database_id.unwrap_or(0);
                let default_branch = node.default_branch_ref.as_ref().map(|r| r.name.clone());
                let head = node.default_branch_ref.and_then(seed_from_ref);
                let branches: Vec<BranchHeadSeed> =
                    node.refs.nodes.into_iter().filter_map(seed_from_ref).collect();
                let branches_complete = !node.refs.page_info.has_next_page;
                let repo = Repo {
                    id: node.database_id,
                    name: node.name,
                    full_name: node.name_with_owner,
                    owner: Owner { login: node.owner.login, id: owner_id },
                    private: node.is_private,
                    default_branch,
                    description: node.description,
                    size: node.disk_usage.unwrap_or(0),
                    fork: node.is_fork,
                };
                out.push(WarmupRepo {
                    repo,
                    default_branch_head: head,
                    branches,
                    branches_complete,
                });
            }
            if !connection.page_info.has_next_page {
                return Ok(out);
            }
            match connection.page_info.end_cursor {
                Some(c) => cursor = Some(c),
                // Defensive: server claimed hasNextPage but gave no cursor.
                None => return Ok(out),
            }
        }
    }

    /// Lazy per-repo branch list via GraphQL. Returns each branch's name plus
    /// commit/tree SHAs in one go, so navigating into a non-default branch
    /// doesn't need a follow-up `get_branch` round-trip.
    pub async fn list_branches_graphql(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<BranchHeadSeed>, GithubError> {
        let mut cursor: Option<String> = None;
        let mut out: Vec<BranchHeadSeed> = Vec::new();
        loop {
            let vars = BranchesVars { owner, repo, cursor: cursor.as_deref() };
            let data: BranchesData = self.post_graphql(BRANCHES_QUERY, &vars).await?;
            let Some(repository) = data.repository else {
                return Err(GithubError::NotFound);
            };
            for node in repository.refs.nodes {
                let Some(target) = node.target else { continue };
                let (Some(oid), Some(tree)) = (target.oid, target.tree) else { continue };
                out.push(BranchHeadSeed {
                    branch: node.name,
                    commit_sha: oid,
                    tree_sha: tree.oid,
                });
            }
            if !repository.refs.page_info.has_next_page {
                return Ok(out);
            }
            match repository.refs.page_info.end_cursor {
                Some(c) => cursor = Some(c),
                None => return Ok(out),
            }
        }
    }

    async fn post_graphql<V: Serialize, R: DeserializeOwned>(
        &self,
        query: &str,
        variables: &V,
    ) -> Result<R, GithubError> {
        let url = format!("{}/graphql", self.base);
        debug!(target: "ghfs", url = %url, "POST graphql");
        let body = GqlRequest { query, variables };
        let resp = self
            .http
            .post(&url)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token.expose()))
            .header(header::ACCEPT, ACCEPT_JSON)
            .header("X-GitHub-Api-Version", API_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(GithubError::Request)?;

        let status = resp.status();
        match status {
            StatusCode::OK => {
                let parsed: GqlResponse<R> = resp.json().await.map_err(GithubError::Decode)?;
                // GraphQL signals query-level failures with HTTP 200 + an
                // `errors` array. Surface those as Unexpected so the caller
                // can fall back to REST.
                if let Some(errors) = parsed.errors
                    && !errors.is_empty()
                {
                    let joined = errors
                        .iter()
                        .map(|e| e.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(GithubError::Unexpected { status: 200, body: joined });
                }
                parsed.data.ok_or_else(|| GithubError::Unexpected {
                    status: 200,
                    body: "graphql response had neither data nor errors".into(),
                })
            }
            StatusCode::UNAUTHORIZED => Err(GithubError::Unauthorized),
            StatusCode::FORBIDDEN => Err(classify_forbidden(resp).await),
            StatusCode::NOT_FOUND => Err(GithubError::NotFound),
            other => Err(unexpected(resp, other).await),
        }
    }
}
