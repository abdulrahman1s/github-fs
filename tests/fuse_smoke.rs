//! End-to-end FUSE smoke test.
//!
//! Mounts a `Ghfs` instance pointed at a wiremock GitHub on a temp dir and
//! verifies the basic read path through the kernel: listing repos / branches,
//! listing files in a tree, reading file contents.
//!
//! Marked `#[ignore]` because it needs an environment where unprivileged FUSE
//! mounts are permitted (`/etc/fuse.conf` with `user_allow_other` is *not*
//! required, but the kernel must allow FUSE for the running user, and the
//! `fusermount3` helper must be on PATH). On most Linux dev machines this just
//! works; on locked-down CI runners it does not — hence opt-in via
//! `cargo test -- --ignored`.

use std::sync::Arc;
use std::time::Duration;

use fuser::MountOption;
use github_fs::cache::{BlobStore, MetaCache};
use github_fs::config::token::Token;
use github_fs::fs::Ghfs;
use github_fs::github::{GithubClient, RepoFilter};
use serde_json::json;
use tokio::runtime::Builder;
use wiremock::matchers::{method, path as wpath};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup_wiremock_github() -> MockServer {
    let server = MockServer::start().await;

    // GraphQL warmup + branches: respond 500 so the FS falls back to REST. We
    // could mock the GraphQL response too, but REST coverage is the bigger
    // value for a smoke test.
    Mock::given(method("POST"))
        .and(wpath("/graphql"))
        .respond_with(ResponseTemplate::new(500).set_body_string("graphql disabled in test"))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(wpath("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "id": 1,
                "name": "alpha",
                "full_name": "abdul/alpha",
                "owner": {"login": "abdul", "id": 42},
                "private": false,
                "default_branch": "main",
                "fork": false,
                "size": 1,
                "description": null,
            }
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(wpath("/repos/abdul/alpha/branches/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "main",
            "commit": {
                "sha": "c1",
                "commit": {"tree": {"sha": "t1"}},
            },
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(wpath("/repos/abdul/alpha/git/trees/t1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sha": "t1",
            "url": "irrelevant",
            "truncated": false,
            "tree": [
                {"path": "README.md", "mode": "100644", "type": "blob", "sha": "s1", "size": 6}
            ],
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(wpath("/repos/abdul/alpha/git/blobs/s1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes("hello\n".as_bytes().to_vec()))
        .mount(&server)
        .await;

    server
}

#[test]
#[ignore = "requires permitted unprivileged FUSE mount; run with `cargo test -- --ignored`"]
fn end_to_end_mount_lists_repos_and_reads_a_file() {
    let rt = Builder::new_multi_thread().enable_all().build().unwrap();
    let server = rt.block_on(setup_wiremock_github());

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let mountpoint = tempfile::tempdir().expect("mount tempdir");

    let client = Arc::new(
        GithubClient::with_base(Token::new("test"), server.uri()).expect("GithubClient::with_base"),
    );
    let meta =
        Arc::new(MetaCache::open(cache_dir.path().join("meta.db")).expect("MetaCache::open"));
    let blobs = Arc::new(BlobStore::open(cache_dir.path().join("blobs")).expect("BlobStore::open"));

    let fs = Ghfs::new(
        rt.handle().clone(),
        client,
        meta,
        blobs,
        RepoFilter::default(),
        None,
        github_fs::CloneTrigger::Never,
        github_fs::cache::default_remote_base(),
    );

    let options = vec![
        MountOption::RO,
        MountOption::FSName("ghfs-smoke".to_string()),
        MountOption::AutoUnmount,
    ];

    let session = match fuser::spawn_mount2(fs, mountpoint.path(), &options) {
        Ok(s) => s,
        Err(e) => {
            // Surface a clear skip-style message rather than panicking on
            // environments where FUSE setup is genuinely unavailable.
            eprintln!("skipping: fuser::spawn_mount2 failed: {e}");
            return;
        }
    };

    // The kernel needs a beat to wire the mount before our reads land. Without
    // this, the first readdir can race the mount and observe an empty dir.
    std::thread::sleep(Duration::from_millis(200));

    let repos = read_dir_names(mountpoint.path());
    assert!(
        repos.contains(&"alpha".to_string()),
        "expected `alpha` in repos: {repos:?}"
    );

    // Layout change: `<mount>/<repo>/` now directly lists the default
    // branch's tree — no intermediate `<branch>/` dir.
    let files = read_dir_names(&mountpoint.path().join("alpha"));
    assert!(
        files.contains(&"README.md".to_string()),
        "expected README.md in tree: {files:?}"
    );

    let content = std::fs::read(mountpoint.path().join("alpha/README.md")).expect("read README.md");
    assert_eq!(content, b"hello\n");

    drop(session);
}

fn read_dir_names(p: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(p)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", p.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
