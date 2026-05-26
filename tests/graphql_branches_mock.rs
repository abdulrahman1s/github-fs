use github_fs::config::token::Token;
use github_fs::github::{GithubClient, GithubError};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> GithubClient {
    GithubClient::with_base(Token::new("ghp_test"), server.uri()).unwrap()
}

fn ref_node(name: &str, commit: &str, tree: &str) -> serde_json::Value {
    json!({
        "name": name,
        "target": { "oid": commit, "tree": { "oid": tree } }
    })
}

#[tokio::test]
async fn list_branches_graphql_returns_name_commit_and_tree_for_each_branch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "repository": {
                    "refs": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [
                            ref_node("main", "c1", "t1"),
                            ref_node("dev", "c2", "t2"),
                        ]
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let seeds = client(&server)
        .list_branches_graphql("u", "r")
        .await
        .unwrap();
    assert_eq!(seeds.len(), 2);
    assert_eq!(seeds[0].branch, "main");
    assert_eq!(seeds[0].commit_sha, "c1");
    assert_eq!(seeds[0].tree_sha, "t1");
    assert_eq!(seeds[1].branch, "dev");
    assert_eq!(seeds[1].tree_sha, "t2");
}

#[tokio::test]
async fn list_branches_graphql_paginates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(r#""cursor":null"#))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "repository": {
                    "refs": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "p1" },
                        "nodes": [ref_node("main", "c1", "t1")]
                    }
                }
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(r#""cursor":"p1""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "repository": {
                    "refs": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [ref_node("feature/foo", "c2", "t2")]
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let seeds = client(&server)
        .list_branches_graphql("u", "r")
        .await
        .unwrap();
    assert_eq!(
        seeds.iter().map(|s| s.branch.as_str()).collect::<Vec<_>>(),
        vec!["main", "feature/foo"]
    );
}

#[tokio::test]
async fn list_branches_graphql_skips_refs_with_missing_target() {
    // Branches whose target isn't a Commit (e.g. a Tag object) deserialize
    // with no oid/tree and must be dropped silently.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "repository": {
                    "refs": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [
                            { "name": "weird", "target": null },
                            ref_node("main", "c1", "t1"),
                        ]
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let seeds = client(&server)
        .list_branches_graphql("u", "r")
        .await
        .unwrap();
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].branch, "main");
}

#[tokio::test]
async fn list_branches_graphql_repository_null_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "repository": null }
        })))
        .mount(&server)
        .await;
    let err = client(&server)
        .list_branches_graphql("u", "missing")
        .await
        .unwrap_err();
    assert!(matches!(err, GithubError::NotFound));
}

#[tokio::test]
async fn list_branches_graphql_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let err = client(&server)
        .list_branches_graphql("u", "r")
        .await
        .unwrap_err();
    assert!(matches!(err, GithubError::Unauthorized));
}

#[tokio::test]
async fn list_branches_graphql_errors_array_surfaces_as_unexpected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [{ "message": "Could not resolve to a Repository" }]
        })))
        .mount(&server)
        .await;
    let err = client(&server)
        .list_branches_graphql("u", "r")
        .await
        .unwrap_err();
    assert!(matches!(err, GithubError::Unexpected { status: 200, .. }));
}
