use github_fs::config::token::Token;
use github_fs::github::{Conditional, GithubClient, GithubError};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> GithubClient {
    GithubClient::with_base(Token::new("ghp_test"), server.uri()).unwrap()
}

fn branch_json(name: &str, sha: &str) -> serde_json::Value {
    json!({
        "name": name,
        "commit": {
            "sha": sha,
            "url": format!("https://example/commits/{sha}")
        }
    })
}

#[tokio::test]
async fn list_branches_single_page_captures_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches"))
        .and(query_param("per_page", "100"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"branches-etag\"")
                .set_body_json(json!([branch_json("main", "c1"), branch_json("dev", "c2")])),
        )
        .mount(&server)
        .await;

    let result = client(&server).list_branches("u", "r", None).await.unwrap();
    let (etag, branches) = result.into_modified().unwrap();

    assert_eq!(etag.as_deref(), Some("\"branches-etag\""));
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[0].name, "main");
    assert_eq!(branches[0].commit.sha, "c1");
    assert_eq!(branches[1].name, "dev");
}

#[tokio::test]
async fn list_branches_short_circuits_on_304() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches"))
        .and(query_param("per_page", "100"))
        .and(header("if-none-match", "\"prev\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;

    let result = client(&server)
        .list_branches("u", "r", Some("\"prev\""))
        .await
        .unwrap();
    assert!(matches!(result, Conditional::NotModified));
}

#[tokio::test]
async fn list_branches_paginates_via_link_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches"))
        .and(query_param("per_page", "100"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"page1\"")
                .insert_header("link", format!(r#"<{}/page2>; rel="next""#, server.uri()))
                .set_body_json(json!([branch_json("main", "c1")])),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([branch_json("dev", "c2")])))
        .mount(&server)
        .await;

    let result = client(&server).list_branches("u", "r", None).await.unwrap();
    let (etag, branches) = result.into_modified().unwrap();

    assert_eq!(etag.as_deref(), Some("\"page1\""));
    assert_eq!(
        branches.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        vec!["main", "dev"]
    );
}

#[tokio::test]
async fn list_branches_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = client(&server)
        .list_branches("u", "r", None)
        .await
        .unwrap_err();
    assert!(matches!(err, GithubError::Unauthorized));
}

#[tokio::test]
async fn list_branches_forbidden() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches"))
        .respond_with(ResponseTemplate::new(403).set_body_string("nope"))
        .mount(&server)
        .await;

    let err = client(&server)
        .list_branches("u", "r", None)
        .await
        .unwrap_err();
    assert!(matches!(err, GithubError::Forbidden(_)));
}

#[tokio::test]
async fn list_branches_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "123")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let err = client(&server)
        .list_branches("u", "r", None)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        GithubError::RateLimited {
            reset_unix: Some(123)
        }
    ));
}

#[tokio::test]
async fn list_branches_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = client(&server)
        .list_branches("u", "r", None)
        .await
        .unwrap_err();
    assert!(matches!(err, GithubError::NotFound));
}

#[tokio::test]
async fn list_branches_unexpected_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/branches"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let err = client(&server)
        .list_branches("u", "r", None)
        .await
        .unwrap_err();
    assert!(matches!(err, GithubError::Unexpected { status: 500, .. }));
}
