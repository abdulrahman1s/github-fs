use github_fs::config::token::Token;
use github_fs::github::{GithubClient, GithubError};
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer, token: &str) -> GithubClient {
    GithubClient::with_base(Token::new(token), server.uri()).expect("client builds")
}

#[tokio::test]
async fn whoami_parses_200_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer ghp_test_token"))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "login": "octocat",
            "id": 1,
            "name": "The Octocat",
            "email": "octocat@example.com",
            "html_url": "https://github.com/octocat"
        })))
        .mount(&server)
        .await;

    let user = client(&server, "ghp_test_token").whoami().await.unwrap();

    assert_eq!(user.login, "octocat");
    assert_eq!(user.id, 1);
    assert_eq!(user.name.as_deref(), Some("The Octocat"));
    assert_eq!(user.email.as_deref(), Some("octocat@example.com"));
    assert_eq!(user.html_url, "https://github.com/octocat");
}

#[tokio::test]
async fn whoami_tolerates_missing_optional_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "login": "ghost",
            "id": 10137,
            "html_url": "https://github.com/ghost"
        })))
        .mount(&server)
        .await;

    let user = client(&server, "x").whoami().await.unwrap();
    assert_eq!(user.login, "ghost");
    assert!(user.name.is_none());
    assert!(user.email.is_none());
}

#[tokio::test]
async fn whoami_maps_401_to_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Bad credentials"))
        .mount(&server)
        .await;

    let err = client(&server, "bad").whoami().await.unwrap_err();
    assert!(matches!(err, GithubError::Unauthorized), "got {err:?}");
}

#[tokio::test]
async fn whoami_distinguishes_rate_limited_403() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "1234567890")
                .set_body_string("API rate limit exceeded"),
        )
        .mount(&server)
        .await;

    let err = client(&server, "x").whoami().await.unwrap_err();
    match err {
        GithubError::RateLimited { reset_unix } => assert_eq!(reset_unix, Some(1234567890)),
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn whoami_plain_403_is_forbidden_not_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "4999")
                .set_body_string("Resource not accessible by integration"),
        )
        .mount(&server)
        .await;

    let err = client(&server, "x").whoami().await.unwrap_err();
    match err {
        GithubError::Forbidden(body) => assert!(body.contains("not accessible")),
        other => panic!("expected Forbidden, got {other:?}"),
    }
}

#[tokio::test]
async fn whoami_maps_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    let err = client(&server, "x").whoami().await.unwrap_err();
    assert!(matches!(err, GithubError::NotFound), "got {err:?}");
}

#[tokio::test]
async fn whoami_surfaces_unexpected_status_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
        .mount(&server)
        .await;

    let err = client(&server, "x").whoami().await.unwrap_err();
    match err {
        GithubError::Unexpected { status, body } => {
            assert_eq!(status, 500);
            assert!(body.contains("boom"), "body lost: {body}");
        }
        other => panic!("expected Unexpected, got {other:?}"),
    }
}

#[tokio::test]
async fn base_url_trailing_slash_is_normalised() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "login": "x",
            "id": 1,
            "html_url": "https://github.com/x"
        })))
        .mount(&server)
        .await;

    // Pass base with trailing slash; should still hit /user, not //user.
    let base = format!("{}/", server.uri());
    let client = GithubClient::with_base(Token::new("x"), base).unwrap();
    let user = client.whoami().await.unwrap();
    assert_eq!(user.login, "x");
}
