use github_fs::cache::{MetaCache, OWNED_USER_REPOS_ETAG_KEY};
use github_fs::cli::refresh::refresh_repos;
use github_fs::config::token::Token;
use github_fs::github::{GithubClient, RepoFilter};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> GithubClient {
    GithubClient::with_base(Token::new("ghp_test"), server.uri()).unwrap()
}

fn repo_json(id: u64, name: &str) -> serde_json::Value {
    json!({
        "id": id,
        "name": name,
        "full_name": format!("abdul/{name}"),
        "owner": { "login": "abdul", "id": 42 },
        "private": false,
        "default_branch": "main",
        "fork": false,
        "size": 1,
        "description": null,
    })
}

#[tokio::test]
async fn refresh_overwrites_cache_with_fresh_repos_and_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"fresh\"")
                .set_body_json(json!([repo_json(1, "alpha"), repo_json(2, "beta")])),
        )
        .mount(&server)
        .await;

    let meta = MetaCache::open_in_memory().unwrap();
    // Seed the cache with stale data so we can verify refresh replaces it.
    meta.put_repos(&[]).unwrap();
    meta.put_etag(OWNED_USER_REPOS_ETAG_KEY, "\"stale\"")
        .unwrap();

    let count = refresh_repos(&client(&server), &meta, &RepoFilter::default())
        .await
        .unwrap();
    assert_eq!(count, 2);

    let repos = meta.get_repos().unwrap();
    assert_eq!(repos.len(), 2);
    let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));

    let etag = meta.get_etag(OWNED_USER_REPOS_ETAG_KEY).unwrap();
    assert_eq!(etag.as_deref(), Some("\"fresh\""));
}

#[tokio::test]
async fn refresh_does_not_send_if_none_match() {
    // The point of `ghfs refresh` is to bypass conditional requests so the
    // user gets a guaranteed-fresh result. If we ever start sending
    // If-None-Match here the command becomes useless when the etag is good.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([repo_json(1, "alpha")])))
        .mount(&server)
        .await;

    let meta = MetaCache::open_in_memory().unwrap();
    let _ = refresh_repos(&client(&server), &meta, &RepoFilter::default())
        .await
        .unwrap();

    let recv = server.received_requests().await.unwrap();
    let req = recv
        .iter()
        .find(|r| r.url.path() == "/user/repos")
        .expect("refresh hit /user/repos");
    assert!(
        req.headers.get("if-none-match").is_none(),
        "refresh must not send If-None-Match — that defeats the point"
    );
}

#[tokio::test]
async fn refresh_surfaces_unauthorized_without_touching_cache() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Bad credentials"))
        .mount(&server)
        .await;

    let meta = MetaCache::open_in_memory().unwrap();
    meta.put_repos(&[]).unwrap();
    meta.put_etag(OWNED_USER_REPOS_ETAG_KEY, "\"original\"")
        .unwrap();

    let err = refresh_repos(&client(&server), &meta, &RepoFilter::default())
        .await
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("unauthorized") || msg.contains("401"),
        "got: {msg}"
    );

    // Cache must be untouched on failure.
    let etag = meta.get_etag(OWNED_USER_REPOS_ETAG_KEY).unwrap();
    assert_eq!(etag.as_deref(), Some("\"original\""));
}

#[tokio::test]
async fn refresh_rejects_unconditional_304() {
    // GitHub shouldn't 304 a request that didn't send If-None-Match. If it
    // ever does, refusing to write avoids silently zeroing the cache.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;

    let meta = MetaCache::open_in_memory().unwrap();
    let err = refresh_repos(&client(&server), &meta, &RepoFilter::default())
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("304"), "got: {msg}");
}

// Ensure the test we wrote actually requires the header to be absent rather
// than just happening to be absent because we passed `None`. Belt-and-braces.
#[tokio::test]
async fn refresh_succeeds_when_server_sends_no_etag_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([repo_json(1, "alpha")])))
        .mount(&server)
        .await;

    let meta = MetaCache::open_in_memory().unwrap();
    let count = refresh_repos(&client(&server), &meta, &RepoFilter::default())
        .await
        .unwrap();
    assert_eq!(count, 1);

    // No etag came back; cache should not have one written either.
    let etag = meta.get_etag(OWNED_USER_REPOS_ETAG_KEY).unwrap();
    assert!(etag.is_none(), "got: {etag:?}");
}

#[tokio::test]
async fn refresh_authorization_header_is_set() {
    // Smoke-check that the token wiring works — easy to break.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(header("authorization", "Bearer ghp_test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let meta = MetaCache::open_in_memory().unwrap();
    let count = refresh_repos(&client(&server), &meta, &RepoFilter::default())
        .await
        .unwrap();
    assert_eq!(count, 0);
}
