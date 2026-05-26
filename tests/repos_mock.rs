use github_fs::config::{Owners, Visibility, token::Token};
use github_fs::github::{Conditional, GithubClient, RepoFilter};
use serde_json::json;
use wiremock::matchers::{header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> GithubClient {
    GithubClient::with_base(Token::new("ghp_test"), server.uri()).unwrap()
}

fn repo_json(id: u64, owner: &str, name: &str, default_branch: &str) -> serde_json::Value {
    json!({
        "id": id,
        "name": name,
        "full_name": format!("{owner}/{name}"),
        "owner": { "login": owner, "id": 42 },
        "private": false,
        "default_branch": default_branch,
        "fork": false,
        "size": 100,
        "description": null,
    })
}

fn repo_json_with_fork(
    id: u64,
    owner: &str,
    name: &str,
    default_branch: &str,
    fork: bool,
) -> serde_json::Value {
    let mut repo = repo_json(id, owner, name, default_branch);
    repo["fork"] = json!(fork);
    repo
}

fn repo_json_with_private(
    id: u64,
    owner: &str,
    name: &str,
    default_branch: &str,
    private: bool,
) -> serde_json::Value {
    let mut repo = repo_json(id, owner, name, default_branch);
    repo["private"] = json!(private);
    repo
}

#[tokio::test]
async fn list_user_repos_single_page_captures_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("per_page", "100"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"first-etag\"")
                .set_body_json(json!([repo_json(1, "abdul", "github-fs", "main")])),
        )
        .mount(&server)
        .await;

    let result = client(&server)
        .list_user_repos(None, &RepoFilter::default())
        .await
        .unwrap();
    let (etag, repos) = result.into_modified().expect("expected Modified");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].name, "github-fs");
    assert_eq!(repos[0].owner.login, "abdul");
    assert_eq!(repos[0].default_branch.as_deref(), Some("main"));
    assert_eq!(etag.as_deref(), Some("\"first-etag\""));
}

#[tokio::test]
async fn list_user_repos_requests_owned_repos_and_filters_forks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("affiliation", "owner"))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            repo_json_with_fork(1, "abdul", "owned", "main", false),
            repo_json_with_fork(2, "abdul", "forked", "main", true),
        ])))
        .mount(&server)
        .await;

    let (_, repos) = client(&server)
        .list_user_repos(None, &RepoFilter::default())
        .await
        .unwrap()
        .into_modified()
        .unwrap();

    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].name, "owned");
    assert!(!repos[0].fork);
}

#[tokio::test]
async fn list_user_repos_paginates_via_link_header() {
    let server = MockServer::start().await;
    // Page 1 — distinct path so wiremock matchers are unambiguous; the Link
    // header tells the client where page 2 lives.
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("per_page", "100"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"page1\"")
                .insert_header(
                    "link",
                    format!(r#"<{}/page2>; rel="next""#, server.uri()).as_str(),
                )
                .set_body_json(json!([repo_json(1, "u", "r1", "main")])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/page2"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"page2\"") // should be ignored
                .set_body_json(json!([repo_json(2, "u", "r2", "trunk")])),
        )
        .mount(&server)
        .await;

    let result = client(&server)
        .list_user_repos(None, &RepoFilter::default())
        .await
        .unwrap();
    let (etag, repos) = result.into_modified().unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].name, "r1");
    assert_eq!(repos[1].name, "r2");
    assert_eq!(
        etag.as_deref(),
        Some("\"page1\""),
        "should capture ETag of page 1, not subsequent pages"
    );
}

#[tokio::test]
async fn list_user_repos_short_circuits_on_304() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(header("if-none-match", "\"prev-etag\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;

    let result = client(&server)
        .list_user_repos(Some("\"prev-etag\""), &RepoFilter::default())
        .await
        .unwrap();
    assert!(matches!(result, Conditional::NotModified), "got {result:?}");
}

#[tokio::test]
async fn list_user_repos_does_not_send_if_none_match_on_page_2() {
    let server = MockServer::start().await;
    // Page 1: requires If-None-Match to be present (we pass an etag in).
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(header_exists("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"new\"")
                .insert_header(
                    "link",
                    format!(r#"<{}/page2>; rel="next""#, server.uri()).as_str(),
                )
                .set_body_json(json!([repo_json(1, "u", "r1", "main")])),
        )
        .mount(&server)
        .await;
    // Page 2: must NOT carry If-None-Match (per-resource ETag semantics).
    // We register a mock that ONLY matches when If-None-Match is absent — if
    // the client sends it, the request will not match and wiremock returns 404
    // by default, failing the test.
    Mock::given(method("GET"))
        .and(path("/page2"))
        // A request matching this mock means If-None-Match was absent. To
        // assert absence we use a manual matcher.
        .and(wiremock::matchers::any())
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([repo_json(2, "u", "r2", "main")])),
        )
        .mount(&server)
        .await;

    let result = client(&server)
        .list_user_repos(Some("\"old\""), &RepoFilter::default())
        .await
        .expect("client should succeed");

    let (_, repos) = result.into_modified().unwrap();
    assert_eq!(repos.len(), 2);

    // Inspect the requests wiremock actually saw to assert that page 2 had no
    // If-None-Match header.
    let recv = server.received_requests().await.unwrap();
    let page2 = recv
        .iter()
        .find(|r| r.url.path() == "/page2")
        .expect("page 2 was requested");
    assert!(
        page2.headers.get("if-none-match").is_none(),
        "page 2 must not carry If-None-Match"
    );
}

#[tokio::test]
async fn list_user_repos_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = client(&server)
        .list_user_repos(None, &RepoFilter::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, github_fs::github::GithubError::Unauthorized),
        "got {err:?}"
    );
}

#[tokio::test]
async fn list_user_repos_tolerates_missing_optional_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            // No description, no default_branch — must still parse.
            {
                "id": 7,
                "name": "minimal",
                "full_name": "u/minimal",
                "owner": {"login":"u","id":1},
                "private": false,
                "fork": false,
                "size": 0
            }
        ])))
        .mount(&server)
        .await;

    let (_, repos) = client(&server)
        .list_user_repos(None, &RepoFilter::default())
        .await
        .unwrap()
        .into_modified()
        .unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].name, "minimal");
    assert!(repos[0].default_branch.is_none());
    assert!(repos[0].description.is_none());
}

#[tokio::test]
async fn list_user_repos_all_drops_affiliation_param_and_keeps_collab_repos() {
    let server = MockServer::start().await;
    // Owners=All must NOT send `affiliation=owner` — the mock matcher fails
    // the request if the param shows up.
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            repo_json(1, "abdul", "owned", "main"),
            repo_json(2, "rust-lang", "rust", "master"),
        ])))
        .mount(&server)
        .await;

    let filter = RepoFilter::new(Owners::All, false);
    let (_, repos) = client(&server)
        .list_user_repos(None, &filter)
        .await
        .unwrap()
        .into_modified()
        .unwrap();

    assert_eq!(repos.len(), 2, "both repos should pass the All filter");

    // Make the `no affiliation` assertion explicit: peek at the recorded
    // request URL.
    let recv = server.received_requests().await.unwrap();
    let req = recv.iter().find(|r| r.url.path() == "/user/repos").unwrap();
    assert!(
        req.url.query_pairs().all(|(k, _)| k != "affiliation"),
        "All filter must not send affiliation; got query {:?}",
        req.url.query()
    );
}

#[tokio::test]
async fn list_user_repos_list_owner_filter_drops_other_owners() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            repo_json(1, "abdul", "keep-mine", "main"),
            repo_json(2, "rust-lang", "rust", "master"),
            repo_json(3, "someone-else", "skip-me", "main"),
        ])))
        .mount(&server)
        .await;

    let filter = RepoFilter::new(
        Owners::List(vec!["AbduL".into(), "rust-lang".into()]),
        false,
    );
    let (_, repos) = client(&server)
        .list_user_repos(None, &filter)
        .await
        .unwrap()
        .into_modified()
        .unwrap();

    let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["keep-mine", "rust"]);
}

#[tokio::test]
async fn list_user_repos_include_forks_keeps_forks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("affiliation", "owner"))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            repo_json_with_fork(1, "abdul", "owned", "main", false),
            repo_json_with_fork(2, "abdul", "forked", "main", true),
        ])))
        .mount(&server)
        .await;

    let filter = RepoFilter::new(Owners::SelfOnly, true);
    let (_, repos) = client(&server)
        .list_user_repos(None, &filter)
        .await
        .unwrap()
        .into_modified()
        .unwrap();

    assert_eq!(repos.len(), 2, "forks must pass when include_forks=true");
}

#[tokio::test]
async fn list_user_repos_visibility_public_only_drops_private() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("affiliation", "owner"))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            repo_json_with_private(1, "abdul", "open-src", "main", false),
            repo_json_with_private(2, "abdul", "secret", "main", true),
        ])))
        .mount(&server)
        .await;

    let filter = RepoFilter::new(Owners::SelfOnly, false).with_visibility(Visibility::PublicOnly);
    let (_, repos) = client(&server)
        .list_user_repos(None, &filter)
        .await
        .unwrap()
        .into_modified()
        .unwrap();

    let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["open-src"]);
}

#[tokio::test]
async fn list_user_repos_visibility_private_only_drops_public() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("affiliation", "owner"))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            repo_json_with_private(1, "abdul", "open-src", "main", false),
            repo_json_with_private(2, "abdul", "secret", "main", true),
        ])))
        .mount(&server)
        .await;

    let filter = RepoFilter::new(Owners::SelfOnly, false).with_visibility(Visibility::PrivateOnly);
    let (_, repos) = client(&server)
        .list_user_repos(None, &filter)
        .await
        .unwrap()
        .into_modified()
        .unwrap();

    let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["secret"]);
}
