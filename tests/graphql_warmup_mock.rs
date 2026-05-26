use github_fs::config::{Owners, token::Token};
use github_fs::github::{GithubClient, GithubError, RepoFilter};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
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

fn repo_node(id: u64, name: &str, head_tree: Option<(&str, &str, &str)>) -> serde_json::Value {
    let default_branch_ref = match head_tree {
        Some((branch, commit, tree)) => json!({
            "name": branch,
            "target": { "oid": commit, "tree": { "oid": tree } }
        }),
        None => serde_json::Value::Null,
    };
    let refs_nodes = match head_tree {
        Some((branch, commit, tree)) => json!([ref_node(branch, commit, tree)]),
        None => json!([]),
    };
    json!({
        "databaseId": id,
        "name": name,
        "nameWithOwner": format!("me/{name}"),
        "isPrivate": false,
        "isFork": false,
        "diskUsage": 42,
        "description": null,
        "owner": { "login": "me", "databaseId": 9 },
        "defaultBranchRef": default_branch_ref,
        "refs": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": refs_nodes
        }
    })
}

#[tokio::test]
async fn warmup_single_page_returns_repos_with_default_branch_heads() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", "Bearer ghp_test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "viewer": {
                    "repositories": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [
                            repo_node(1, "alpha", Some(("main", "c1", "t1"))),
                            repo_node(2, "beta", None),
                        ]
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let warmup = client(&server).warmup_user_repos(&RepoFilter::default()).await.unwrap();
    assert_eq!(warmup.len(), 2);

    let alpha = &warmup[0];
    assert_eq!(alpha.repo.id, 1);
    assert_eq!(alpha.repo.name, "alpha");
    assert_eq!(alpha.repo.full_name, "me/alpha");
    assert_eq!(alpha.repo.owner.login, "me");
    assert_eq!(alpha.repo.owner.id, 9);
    assert_eq!(alpha.repo.size, 42);
    assert_eq!(alpha.repo.default_branch.as_deref(), Some("main"));
    let head = alpha.default_branch_head.as_ref().unwrap();
    assert_eq!(head.branch, "main");
    assert_eq!(head.commit_sha, "c1");
    assert_eq!(head.tree_sha, "t1");

    // Empty repo (no defaultBranchRef): seeded as repo only, no head.
    let beta = &warmup[1];
    assert_eq!(beta.repo.name, "beta");
    assert!(beta.repo.default_branch.is_none());
    assert!(beta.default_branch_head.is_none());
    assert!(beta.branches.is_empty());
    assert!(beta.branches_complete);

    // Alpha's branches include the default branch.
    assert_eq!(alpha.branches.len(), 1);
    assert_eq!(alpha.branches[0].branch, "main");
    assert_eq!(alpha.branches[0].tree_sha, "t1");
    assert!(alpha.branches_complete);
}

#[tokio::test]
async fn warmup_returns_all_branches_per_repo_in_one_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "viewer": {
                    "repositories": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "databaseId": 1,
                            "name": "alpha",
                            "nameWithOwner": "me/alpha",
                            "isPrivate": false,
                            "isFork": false,
                            "diskUsage": 0,
                            "description": null,
                            "owner": { "login": "me", "databaseId": 9 },
                            "defaultBranchRef": {
                                "name": "main",
                                "target": { "oid": "c1", "tree": { "oid": "t1" } }
                            },
                            "refs": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": [
                                    ref_node("main", "c1", "t1"),
                                    ref_node("dev", "c2", "t2"),
                                    ref_node("feature/x", "c3", "t3"),
                                ]
                            }
                        }]
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let warmup = client(&server).warmup_user_repos(&RepoFilter::default()).await.unwrap();
    assert_eq!(warmup.len(), 1);
    let alpha = &warmup[0];
    assert!(alpha.branches_complete);
    let names: Vec<&str> = alpha.branches.iter().map(|b| b.branch.as_str()).collect();
    assert_eq!(names, vec!["main", "dev", "feature/x"]);
    assert_eq!(alpha.branches[2].tree_sha, "t3");
}

#[tokio::test]
async fn warmup_marks_branches_incomplete_when_inner_pagination_overflows() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "viewer": {
                    "repositories": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "databaseId": 1,
                            "name": "huge",
                            "nameWithOwner": "me/huge",
                            "isPrivate": false,
                            "isFork": false,
                            "diskUsage": 0,
                            "description": null,
                            "owner": { "login": "me", "databaseId": 9 },
                            "defaultBranchRef": {
                                "name": "main",
                                "target": { "oid": "c1", "tree": { "oid": "t1" } }
                            },
                            "refs": {
                                "pageInfo": { "hasNextPage": true, "endCursor": "more" },
                                "nodes": [ref_node("main", "c1", "t1")]
                            }
                        }]
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let warmup = client(&server).warmup_user_repos(&RepoFilter::default()).await.unwrap();
    assert!(!warmup[0].branches_complete);
    assert_eq!(warmup[0].branches.len(), 1);
}

#[tokio::test]
async fn warmup_paginates_via_end_cursor() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(r#""cursor":null"#))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "viewer": {
                    "repositories": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "abc123" },
                        "nodes": [repo_node(1, "alpha", Some(("main", "c1", "t1")))]
                    }
                }
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(r#""cursor":"abc123""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "viewer": {
                    "repositories": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [repo_node(2, "beta", Some(("trunk", "c2", "t2")))]
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let warmup = client(&server).warmup_user_repos(&RepoFilter::default()).await.unwrap();
    let names: Vec<&str> = warmup.iter().map(|w| w.repo.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta"]);
    assert_eq!(warmup[1].default_branch_head.as_ref().unwrap().tree_sha, "t2");
}

#[tokio::test]
async fn warmup_filters_forks_client_side() {
    // Even if isFork=false slips past the server filter (e.g. an enterprise
    // schema quirk), we don't want forks landing in the cache.
    let server = MockServer::start().await;
    let mut fork = repo_node(7, "forked", Some(("main", "c", "t")));
    fork["isFork"] = json!(true);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "viewer": {
                    "repositories": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [repo_node(1, "kept", Some(("main", "c", "t"))), fork]
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let warmup = client(&server).warmup_user_repos(&RepoFilter::default()).await.unwrap();
    assert_eq!(warmup.len(), 1);
    assert_eq!(warmup[0].repo.name, "kept");
}

#[tokio::test]
async fn warmup_surfaces_graphql_errors_array_as_unexpected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [{ "message": "Field 'isFork' doesn't exist on type 'X'" }]
        })))
        .mount(&server)
        .await;

    let err = client(&server).warmup_user_repos(&RepoFilter::default()).await.unwrap_err();
    assert!(matches!(err, GithubError::Unexpected { status: 200, .. }));
}

#[tokio::test]
async fn warmup_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let err = client(&server).warmup_user_repos(&RepoFilter::default()).await.unwrap_err();
    assert!(matches!(err, GithubError::Unauthorized));
}

#[tokio::test]
async fn warmup_self_only_uses_owner_affiliation_in_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "viewer": {
                    "repositories": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": []
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let _ = client(&server)
        .warmup_user_repos(&RepoFilter::new(Owners::SelfOnly, false))
        .await
        .unwrap();

    let recv = server.received_requests().await.unwrap();
    let body = std::str::from_utf8(&recv[0].body).unwrap();
    assert!(
        body.contains("ownerAffiliations: [OWNER]"),
        "SelfOnly must narrow server-side; got body: {body}"
    );
    assert!(
        body.contains("isFork: false"),
        "include_forks=false must add isFork filter; got body: {body}"
    );
}

#[tokio::test]
async fn warmup_all_omits_owner_affiliation_and_fork_filter_when_forks_included() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "viewer": {
                    "repositories": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": []
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let _ = client(&server)
        .warmup_user_repos(&RepoFilter::new(Owners::All, true))
        .await
        .unwrap();

    let recv = server.received_requests().await.unwrap();
    let body = std::str::from_utf8(&recv[0].body).unwrap();
    assert!(
        !body.contains("ownerAffiliations"),
        "All must widen the fetch; got body: {body}"
    );
    // `isFork` (the field selection on nodes) is always present — we read it
    // to enforce the client-side filter. The thing we mustn't see is the
    // *argument* `isFork: false` on `repositories(...)`.
    assert!(
        !body.contains("isFork: false"),
        "include_forks=true must not add the isFork filter arg; got body: {body}"
    );
}

#[tokio::test]
async fn warmup_list_filter_drops_owners_client_side() {
    let server = MockServer::start().await;
    let mut other_owner = repo_node(2, "external", Some(("main", "c2", "t2")));
    other_owner["owner"] = json!({ "login": "someone-else", "databaseId": 11 });
    other_owner["nameWithOwner"] = json!("someone-else/external");
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "viewer": {
                    "repositories": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [
                            repo_node(1, "kept", Some(("main", "c", "t"))),
                            other_owner
                        ]
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    // The default repo_node uses owner.login = "me". Allow only that owner.
    let filter = RepoFilter::new(Owners::List(vec!["ME".into()]), false);
    let warmup = client(&server)
        .warmup_user_repos(&filter)
        .await
        .unwrap();
    assert_eq!(warmup.len(), 1);
    assert_eq!(warmup[0].repo.name, "kept");
}

#[tokio::test]
async fn warmup_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "555")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;
    let err = client(&server).warmup_user_repos(&RepoFilter::default()).await.unwrap_err();
    assert!(matches!(
        err,
        GithubError::RateLimited {
            reset_unix: Some(555)
        }
    ));
}
