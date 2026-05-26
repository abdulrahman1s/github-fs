use github_fs::config::token::Token;
use github_fs::github::{GithubClient, GithubError};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> GithubClient {
    GithubClient::with_base(Token::new("ghp_test"), server.uri()).unwrap()
}

#[tokio::test]
async fn get_blob_raw_returns_bytes_with_raw_accept() {
    let server = MockServer::start().await;
    let payload = b"fn main() { println!(\"hi\"); }";
    Mock::given(method("GET"))
        .and(path("/repos/u/r/git/blobs/blob-sha"))
        .and(header("accept", "application/vnd.github.raw"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.as_ref()))
        .mount(&server)
        .await;

    let body = client(&server)
        .get_blob_raw("u", "r", "blob-sha")
        .await
        .unwrap();
    assert_eq!(body.as_ref(), payload.as_ref());
}

#[tokio::test]
async fn get_blob_raw_handles_binary_payload() {
    let server = MockServer::start().await;
    let payload: Vec<u8> = (0u8..=255).collect();
    Mock::given(method("GET"))
        .and(path("/repos/u/r/git/blobs/binsha"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
        .mount(&server)
        .await;

    let body = client(&server)
        .get_blob_raw("u", "r", "binsha")
        .await
        .unwrap();
    assert_eq!(body.as_ref(), payload.as_slice());
}

#[tokio::test]
async fn get_blob_raw_404_maps_to_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/git/blobs/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = client(&server)
        .get_blob_raw("u", "r", "missing")
        .await
        .unwrap_err();
    assert!(matches!(err, GithubError::NotFound), "got {err:?}");
}

#[tokio::test]
async fn get_blob_raw_does_not_send_if_none_match() {
    // Blobs are content-addressed; conditional caching makes no sense and
    // would waste a header. Verify we don't send it.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/u/r/git/blobs/abc"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"x".as_ref()))
        .mount(&server)
        .await;

    let _ = client(&server).get_blob_raw("u", "r", "abc").await.unwrap();

    let recv = server.received_requests().await.unwrap();
    let req = recv.first().unwrap();
    assert!(
        req.headers.get("if-none-match").is_none(),
        "blob requests must not include If-None-Match"
    );
}
