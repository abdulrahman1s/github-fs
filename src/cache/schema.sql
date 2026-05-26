-- ETags for query-level (non-row) resources, e.g. the full /user/repos list.
CREATE TABLE IF NOT EXISTS etags (
    cache_key   TEXT PRIMARY KEY,
    etag        TEXT NOT NULL,
    updated_at  INTEGER NOT NULL  -- unix seconds
);

-- One row per repository. Refreshed in bulk via a single transaction so the
-- list is always consistent (no torn read during a refresh).
CREATE TABLE IF NOT EXISTS repos (
    id              INTEGER PRIMARY KEY,
    owner_login     TEXT NOT NULL,
    owner_id        INTEGER NOT NULL,
    name            TEXT NOT NULL,
    default_branch  TEXT,
    description     TEXT,
    private         INTEGER NOT NULL,  -- bool: 0/1
    fork            INTEGER NOT NULL,  -- bool: 0/1
    size_kb         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_repos_owner_name ON repos (owner_login, name);

-- HEAD pointer for (repo, branch). The ETag lives here so a refresh is a
-- single-row conditional fetch.
CREATE TABLE IF NOT EXISTS branch_heads (
    repo_id     INTEGER NOT NULL,
    branch      TEXT NOT NULL,
    commit_sha  TEXT NOT NULL,
    tree_sha    TEXT NOT NULL,
    etag        TEXT,
    fetched_at  INTEGER NOT NULL,
    PRIMARY KEY (repo_id, branch)
);

-- Trees are content-addressed by their SHA. The bytes for a given SHA can
-- never change (the SHA *is* the hash of the bytes), so we INSERT OR IGNORE
-- and never need to invalidate.
CREATE TABLE IF NOT EXISTS trees (
    sha         TEXT PRIMARY KEY,
    json_body   TEXT NOT NULL,
    fetched_at  INTEGER NOT NULL
);

-- Per-repo override that swaps which branch the mount surfaces under
-- `<mount>/<repo>/`. Absent → use the repo's GitHub-default branch. Set by
-- `ghfs branch <repo> <branch>`; consulted on every mount-time Repo inode
-- allocation, so the override takes effect at the next mount.
CREATE TABLE IF NOT EXISTS branch_overrides (
    repo_id  INTEGER PRIMARY KEY,
    branch   TEXT NOT NULL,
    set_at   INTEGER NOT NULL
);
