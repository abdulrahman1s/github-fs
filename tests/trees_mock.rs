use github_fs::config::token::Token;
use github_fs::github::{Conditional, GithubClient, TreeEntryKind};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> GithubClient {
    GithubClient::with_base(Token::new("ghp_test"), server.uri()).unwrap()
}

#[tokio::test]
async fn get_branch_returns_head_tree_sha() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/abdul/github-fs/branches/main"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"branch-etag\"")
                .set_body_json(json!({
                    "name": "main",
                    "commit": {
                        "sha": "commit-sha-abc",
                        "commit": {
                            "tree": { "sha": "tree-sha-xyz" }
                        }
                    }
                })),
        )
        .mount(&server)
        .await;

    let result = client(&server)
        .get_branch("abdul", "github-fs", "main", None)
        .await
        .unwrap();
    let (etag, branch) = result.into_modified().unwrap();
    assert_eq!(branch.name, "main");
    assert_eq!(branch.commit.sha, "commit-sha-abc");
    assert_eq!(branch.commit.commit.tree.sha, "tree-sha-xyz");
    assert_eq!(etag.as_deref(), Some("\"branch-etag\""));
}

#[tokio::test]
async fn get_branch_304_returns_not_modified() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches/main"))
        .and(header("if-none-match", "\"prev\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;

    let result = client(&server)
        .get_branch("u", "r", "main", Some("\"prev\""))
        .await
        .unwrap();
    assert!(matches!(result, Conditional::NotModified));
}

#[tokio::test]
async fn get_branch_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches/nope"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let err = client(&server)
        .get_branch("u", "r", "nope", None)
        .await
        .unwrap_err();
    assert!(matches!(err, github_fs::github::GithubError::NotFound));
}

#[tokio::test]
async fn get_branch_percent_encodes_branch_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches/feature%2Fone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "feature/one",
            "commit": {
                "sha": "commit-sha",
                "commit": {
                    "tree": { "sha": "tree-sha" }
                }
            }
        })))
        .mount(&server)
        .await;

    let (_, branch) = client(&server)
        .get_branch("u", "r", "feature/one", None)
        .await
        .unwrap()
        .into_modified()
        .unwrap();

    assert_eq!(branch.name, "feature/one");
    assert_eq!(branch.commit.commit.tree.sha, "tree-sha");
}

#[tokio::test]
async fn get_tree_recursive_parses_blobs_dirs_symlinks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/git/trees/treeSha"))
        .and(query_param("recursive", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"tree-etag\"")
                .set_body_json(json!({
                    "sha": "treeSha",
                    "url": "https://example/treeSha",
                    "truncated": false,
                    "tree": [
                        {"path": "README.md", "mode": "100644", "type": "blob", "sha": "b1", "size": 12 },
                        {"path": "src",       "mode": "040000", "type": "tree", "sha": "t1" },
                        {"path": "src/main.rs", "mode": "100644", "type": "blob", "sha": "b2", "size": 99 },
                        {"path": "link",      "mode": "120000", "type": "blob", "sha": "b3", "size": 7 },
                        {"path": "vendor",    "mode": "160000", "type": "commit", "sha": "csub" }
                    ]
                })),
        )
        .mount(&server)
        .await;

    let result = client(&server)
        .get_tree("u", "r", "treeSha", true, None)
        .await
        .unwrap();
    let (etag, tree) = result.into_modified().unwrap();

    assert_eq!(tree.sha, "treeSha");
    assert_eq!(etag.as_deref(), Some("\"tree-etag\""));
    assert!(!tree.truncated);
    assert_eq!(tree.tree.len(), 5);

    assert_eq!(tree.tree[0].kind, TreeEntryKind::Blob);
    assert_eq!(tree.tree[0].mode, "100644");
    assert_eq!(tree.tree[0].size, Some(12));

    assert_eq!(tree.tree[1].kind, TreeEntryKind::Tree);
    assert!(tree.tree[1].size.is_none());

    // Symlinks have mode 120000 but type blob — caller distinguishes via mode.
    assert_eq!(tree.tree[3].mode, "120000");
    assert_eq!(tree.tree[3].kind, TreeEntryKind::Blob);

    // Submodules are typed as "commit" (gitlinks).
    assert_eq!(tree.tree[4].kind, TreeEntryKind::Commit);
    assert!(tree.tree[4].size.is_none());
}

#[tokio::test]
async fn get_tree_non_recursive_omits_query_param() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/git/trees/abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sha": "abc",
            "url": "https://x",
            "tree": []
        })))
        .mount(&server)
        .await;

    let (_, tree) = client(&server)
        .get_tree("u", "r", "abc", false, None)
        .await
        .unwrap()
        .into_modified()
        .unwrap();
    assert!(tree.tree.is_empty());

    let recv = server.received_requests().await.unwrap();
    let req = recv.first().unwrap();
    assert!(
        req.url.query().is_none_or(|q| !q.contains("recursive")),
        "should not include ?recursive when caller asked for non-recursive"
    );
}

#[tokio::test]
async fn get_tree_propagates_truncated_flag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/git/trees/big"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sha": "big",
            "url": "https://x",
            "truncated": true,
            "tree": []
        })))
        .mount(&server)
        .await;

    let (_, tree) = client(&server)
        .get_tree("u", "r", "big", false, None)
        .await
        .unwrap()
        .into_modified()
        .unwrap();
    assert!(
        tree.truncated,
        "callers need to know to fall back to per-dir listings"
    );
}

#[tokio::test]
async fn get_tree_304_returns_not_modified() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/git/trees/abc"))
        .and(header("if-none-match", "\"t\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;

    let result = client(&server)
        .get_tree("u", "r", "abc", false, Some("\"t\""))
        .await
        .unwrap();
    assert!(matches!(result, Conditional::NotModified));
}
