use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::cache::CacheError;
use crate::github::{Owner, Repo, Tree};

const SCHEMA: &str = include_str!("schema.sql");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchHead {
    pub commit_sha: String,
    pub tree_sha: String,
    pub etag: Option<String>,
    pub fetched_at: i64,
}

pub struct MetaCache {
    conn: Mutex<Connection>,
}

impl MetaCache {
    /// Open (creating parent dirs as needed) the SQLite metadata cache at
    /// `path`. The schema is applied idempotently on every open.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, CacheError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory cache, intended for tests.
    pub fn open_in_memory() -> Result<Self, CacheError> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init(conn: &Connection) -> Result<(), CacheError> {
        // WAL + synchronous=NORMAL is the recommended tuning for a cache
        // where some unsynced writes after a crash are acceptable (we can
        // always re-fetch from GitHub). On :memory: this is a silent no-op.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(())
    }

    // --- ETag K/V ---

    pub fn get_etag(&self, key: &str) -> Result<Option<String>, CacheError> {
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        conn.query_row(
            "SELECT etag FROM etags WHERE cache_key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn put_etag(&self, key: &str, etag: &str) -> Result<(), CacheError> {
        let now = unix_now();
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        conn.execute(
            "INSERT INTO etags (cache_key, etag, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(cache_key) DO UPDATE SET \
                etag = excluded.etag, \
                updated_at = excluded.updated_at",
            params![key, etag, now],
        )?;
        Ok(())
    }

    // --- Repo list ---

    /// Atomically replace the entire repo list. Wrapped in a transaction so
    /// concurrent readers never see a partial mid-refresh state.
    pub fn put_repos(&self, repos: &[Repo]) -> Result<(), CacheError> {
        let mut conn = self.conn.lock().expect("MetaCache mutex poisoned");
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM repos", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO repos \
                 (id, owner_login, owner_id, name, default_branch, description, private, fork, size_kb) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for r in repos {
                stmt.execute(params![
                    r.id as i64,
                    r.owner.login,
                    r.owner.id as i64,
                    r.name,
                    r.default_branch,
                    r.description,
                    r.private as i64,
                    r.fork as i64,
                    r.size as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_repos(&self) -> Result<Vec<Repo>, CacheError> {
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, owner_login, owner_id, name, default_branch, description, private, fork, size_kb \
             FROM repos ORDER BY owner_login, name",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let owner_login: String = row.get(1)?;
            let owner_id: i64 = row.get(2)?;
            let name: String = row.get(3)?;
            let default_branch: Option<String> = row.get(4)?;
            let description: Option<String> = row.get(5)?;
            let private: i64 = row.get(6)?;
            let fork: i64 = row.get(7)?;
            let size_kb: i64 = row.get(8)?;
            let full_name = format!("{owner_login}/{name}");
            Ok(Repo {
                id: id as u64,
                owner: Owner {
                    login: owner_login,
                    id: owner_id as u64,
                },
                name,
                full_name,
                default_branch,
                description,
                private: private != 0,
                fork: fork != 0,
                size: size_kb as u64,
            })
        })?;
        let out: Result<Vec<Repo>, rusqlite::Error> = rows.collect();
        Ok(out?)
    }

    // --- Branch heads ---

    pub fn put_branch_head(
        &self,
        repo_id: u64,
        branch: &str,
        commit_sha: &str,
        tree_sha: &str,
        etag: Option<&str>,
    ) -> Result<(), CacheError> {
        let now = unix_now();
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        conn.execute(
            "INSERT INTO branch_heads (repo_id, branch, commit_sha, tree_sha, etag, fetched_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(repo_id, branch) DO UPDATE SET \
                commit_sha = excluded.commit_sha, \
                tree_sha   = excluded.tree_sha, \
                etag       = excluded.etag, \
                fetched_at = excluded.fetched_at",
            params![repo_id as i64, branch, commit_sha, tree_sha, etag, now],
        )?;
        Ok(())
    }

    pub fn get_branch_head(
        &self,
        repo_id: u64,
        branch: &str,
    ) -> Result<Option<BranchHead>, CacheError> {
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        conn.query_row(
            "SELECT commit_sha, tree_sha, etag, fetched_at \
             FROM branch_heads WHERE repo_id = ?1 AND branch = ?2",
            params![repo_id as i64, branch],
            |row| {
                Ok(BranchHead {
                    commit_sha: row.get(0)?,
                    tree_sha: row.get(1)?,
                    etag: row.get(2)?,
                    fetched_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    // --- Trees (immutable by SHA) ---

    /// Store a tree. Idempotent: if the same SHA is already present the
    /// existing row is kept (the bytes for a given git SHA can never differ).
    pub fn put_tree(&self, tree: &Tree) -> Result<(), CacheError> {
        let json = serde_json::to_string(tree)?;
        let now = unix_now();
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        conn.execute(
            "INSERT OR IGNORE INTO trees (sha, json_body, fetched_at) VALUES (?1, ?2, ?3)",
            params![tree.sha, json, now],
        )?;
        Ok(())
    }

    pub fn get_tree(&self, sha: &str) -> Result<Option<Tree>, CacheError> {
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        let json: Option<String> = conn
            .query_row(
                "SELECT json_body FROM trees WHERE sha = ?1",
                params![sha],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    // --- Branch overrides ---

    /// Set the branch the mount should surface under `<mount>/<repo>/` for
    /// `repo_id`. Upserts.
    pub fn put_branch_override(&self, repo_id: u64, branch: &str) -> Result<(), CacheError> {
        let now = unix_now();
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        conn.execute(
            "INSERT INTO branch_overrides (repo_id, branch, set_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(repo_id) DO UPDATE SET \
                branch = excluded.branch, \
                set_at = excluded.set_at",
            params![repo_id as i64, branch, now],
        )?;
        Ok(())
    }

    pub fn get_branch_override(&self, repo_id: u64) -> Result<Option<String>, CacheError> {
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        conn.query_row(
            "SELECT branch FROM branch_overrides WHERE repo_id = ?1",
            params![repo_id as i64],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Remove a previously-set override; mount falls back to the repo's
    /// GitHub-default branch.
    pub fn delete_branch_override(&self, repo_id: u64) -> Result<(), CacheError> {
        let conn = self.conn.lock().expect("MetaCache mutex poisoned");
        conn.execute(
            "DELETE FROM branch_overrides WHERE repo_id = ?1",
            params![repo_id as i64],
        )?;
        Ok(())
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Owner, Repo, Tree, TreeEntry, TreeEntryKind};

    fn repo(id: u64, owner: &str, name: &str) -> Repo {
        Repo {
            id,
            name: name.into(),
            full_name: format!("{owner}/{name}"),
            owner: Owner {
                login: owner.into(),
                id: 1,
            },
            private: false,
            default_branch: Some("main".into()),
            description: None,
            size: 0,
            fork: false,
        }
    }

    fn cache() -> MetaCache {
        MetaCache::open_in_memory().unwrap()
    }

    #[test]
    fn init_is_idempotent() {
        let c = cache();
        let conn = c.conn.lock().unwrap();
        // Re-running init on the same connection must not fail or duplicate
        // schema objects.
        MetaCache::init(&conn).unwrap();
        MetaCache::init(&conn).unwrap();
    }

    #[test]
    fn etag_put_get_overwrite() {
        let c = cache();
        assert_eq!(c.get_etag("k").unwrap(), None);
        c.put_etag("k", "\"v1\"").unwrap();
        assert_eq!(c.get_etag("k").unwrap().as_deref(), Some("\"v1\""));
        c.put_etag("k", "\"v2\"").unwrap();
        assert_eq!(c.get_etag("k").unwrap().as_deref(), Some("\"v2\""));
    }

    #[test]
    fn get_repos_empty_on_fresh_cache() {
        let c = cache();
        assert!(c.get_repos().unwrap().is_empty());
    }

    #[test]
    fn put_repos_replaces_atomically() {
        let c = cache();
        c.put_repos(&[repo(1, "u", "a"), repo(2, "u", "b"), repo(3, "u", "c")])
            .unwrap();
        assert_eq!(c.get_repos().unwrap().len(), 3);

        // Replace with a smaller set — old rows must be gone.
        c.put_repos(&[repo(4, "u", "d")]).unwrap();
        let got = c.get_repos().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 4);
        assert_eq!(got[0].name, "d");
    }

    #[test]
    fn repos_roundtrip_preserves_all_fields() {
        let c = cache();
        let r = Repo {
            id: 12345,
            name: "rocket".into(),
            full_name: "abdul/rocket".into(),
            owner: Owner {
                login: "abdul".into(),
                id: 99,
            },
            private: true,
            default_branch: Some("trunk".into()),
            description: Some("a rocket".into()),
            size: 4096,
            fork: true,
        };
        c.put_repos(std::slice::from_ref(&r)).unwrap();
        let got = c.get_repos().unwrap();
        assert_eq!(got.len(), 1);
        let g = &got[0];
        assert_eq!(g.id, r.id);
        assert_eq!(g.name, r.name);
        assert_eq!(g.full_name, "abdul/rocket"); // synthesised on read
        assert_eq!(g.owner.login, "abdul");
        assert_eq!(g.owner.id, 99);
        assert!(g.private);
        assert_eq!(g.default_branch.as_deref(), Some("trunk"));
        assert_eq!(g.description.as_deref(), Some("a rocket"));
        assert_eq!(g.size, 4096);
        assert!(g.fork);
    }

    #[test]
    fn repos_ordered_by_owner_then_name() {
        let c = cache();
        c.put_repos(&[repo(1, "z", "a"), repo(2, "a", "z"), repo(3, "a", "a")])
            .unwrap();
        let got = c.get_repos().unwrap();
        assert_eq!(
            got.iter()
                .map(|r| (r.owner.login.as_str(), r.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("a", "a"), ("a", "z"), ("z", "a")]
        );
    }

    #[test]
    fn repos_handles_null_default_branch_and_description() {
        let c = cache();
        let r = Repo {
            default_branch: None,
            description: None,
            ..repo(7, "u", "minimal")
        };
        c.put_repos(&[r]).unwrap();
        let got = &c.get_repos().unwrap()[0];
        assert!(got.default_branch.is_none());
        assert!(got.description.is_none());
    }

    #[test]
    fn branch_head_put_get_and_upsert() {
        let c = cache();
        assert!(c.get_branch_head(1, "main").unwrap().is_none());

        c.put_branch_head(1, "main", "c1", "t1", Some("\"e1\""))
            .unwrap();
        let h = c.get_branch_head(1, "main").unwrap().unwrap();
        assert_eq!(h.commit_sha, "c1");
        assert_eq!(h.tree_sha, "t1");
        assert_eq!(h.etag.as_deref(), Some("\"e1\""));

        // Upsert with no etag — etag must be cleared.
        c.put_branch_head(1, "main", "c2", "t2", None).unwrap();
        let h = c.get_branch_head(1, "main").unwrap().unwrap();
        assert_eq!(h.commit_sha, "c2");
        assert_eq!(h.tree_sha, "t2");
        assert!(h.etag.is_none());
    }

    #[test]
    fn branch_heads_keyed_by_repo_and_branch() {
        let c = cache();
        c.put_branch_head(1, "main", "c1", "t1", None).unwrap();
        c.put_branch_head(1, "dev", "c2", "t2", None).unwrap();
        c.put_branch_head(2, "main", "c3", "t3", None).unwrap();

        assert_eq!(
            c.get_branch_head(1, "main").unwrap().unwrap().tree_sha,
            "t1"
        );
        assert_eq!(c.get_branch_head(1, "dev").unwrap().unwrap().tree_sha, "t2");
        assert_eq!(
            c.get_branch_head(2, "main").unwrap().unwrap().tree_sha,
            "t3"
        );
    }

    #[test]
    fn tree_round_trip_preserves_entries() {
        let c = cache();
        let t = Tree {
            sha: "treeSha".into(),
            url: "https://example/t".into(),
            truncated: true,
            tree: vec![
                TreeEntry {
                    path: "README".into(),
                    mode: "100644".into(),
                    kind: TreeEntryKind::Blob,
                    sha: "blobSha".into(),
                    size: Some(42),
                },
                TreeEntry {
                    path: "src".into(),
                    mode: "040000".into(),
                    kind: TreeEntryKind::Tree,
                    sha: "subtree".into(),
                    size: None,
                },
            ],
        };
        c.put_tree(&t).unwrap();
        let got = c.get_tree("treeSha").unwrap().unwrap();
        assert_eq!(got.sha, "treeSha");
        assert!(got.truncated);
        assert_eq!(got.tree.len(), 2);
        assert_eq!(got.tree[0].kind, TreeEntryKind::Blob);
        assert_eq!(got.tree[0].size, Some(42));
        assert_eq!(got.tree[1].kind, TreeEntryKind::Tree);
        assert!(got.tree[1].size.is_none());
    }

    #[test]
    fn tree_missing_returns_none() {
        let c = cache();
        assert!(c.get_tree("does-not-exist").unwrap().is_none());
    }

    #[test]
    fn tree_put_is_insert_or_ignore() {
        // Contract: trees are content-addressed by SHA. Once stored, the row
        // is never rewritten — git SHAs are content hashes, so the bytes for
        // any given SHA are immutable.
        let c = cache();
        let t1 = Tree {
            sha: "abc".into(),
            url: "u".into(),
            truncated: false,
            tree: vec![],
        };
        c.put_tree(&t1).unwrap();
        let t2 = Tree {
            sha: "abc".into(),
            url: "u-changed".into(),
            truncated: true,
            tree: vec![],
        };
        c.put_tree(&t2).unwrap();
        let got = c.get_tree("abc").unwrap().unwrap();
        // First write wins.
        assert_eq!(got.url, "u");
        assert!(!got.truncated);
    }

    #[test]
    fn branch_override_round_trip_and_delete() {
        let c = cache();
        assert!(c.get_branch_override(7).unwrap().is_none());

        c.put_branch_override(7, "dev").unwrap();
        assert_eq!(c.get_branch_override(7).unwrap().as_deref(), Some("dev"));

        // Upsert replaces, doesn't accumulate rows.
        c.put_branch_override(7, "release").unwrap();
        assert_eq!(
            c.get_branch_override(7).unwrap().as_deref(),
            Some("release")
        );

        c.delete_branch_override(7).unwrap();
        assert!(c.get_branch_override(7).unwrap().is_none());

        // delete is idempotent when nothing is set.
        c.delete_branch_override(7).unwrap();
    }

    #[test]
    fn file_backed_cache_persists_across_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.db");

        {
            let c = MetaCache::open(&path).unwrap();
            c.put_repos(&[repo(1, "u", "a")]).unwrap();
            c.put_etag("k", "\"v\"").unwrap();
        }
        // Re-open and confirm data is still there.
        let c = MetaCache::open(&path).unwrap();
        assert_eq!(c.get_repos().unwrap().len(), 1);
        assert_eq!(c.get_etag("k").unwrap().as_deref(), Some("\"v\""));
    }
}
