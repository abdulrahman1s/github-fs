# Agent guide — github-fs

Guidance for AI coding agents working in this repo. Humans should read
`README.md` instead.

## What this project is

A Linux CLI that mounts GitHub repos as a FUSE filesystem. The layout
groups repos by owner (`<mount>/<owner>/<repo>/...`), surfacing the
repo's *effective branch* — the override set via `ghfs branch <path> <b>`
(where `<path>` lives under the mount) if present, else the
GitHub-default branch. Read-only by default;
materialized repos (via `ghfs promote` or the configured clone trigger,
when the materialized branch is the repo's effective branch) become
writable passthroughs to an on-disk worktree, so editors and `git` work
directly through the mount. Single binary `ghfs`. Library crate
`github_fs` is the public test surface.

## Environment

**NixOS.** All build/test commands must run inside the project's Nix dev
shell (provides `pkg-config` + `fuse3` headers that `fuser` needs to
compile). Outside the shell, builds fail with missing headers — the user
is not on Arch, despite what stale tooling context may claim.

The dev shell is defined in `flake.nix` (devShells.default). Run commands as:

```bash
nix develop --command cargo test
nix develop --command cargo check --all-targets
nix develop --command cargo clippy --all-targets -- -D warnings
```

Do not invoke `cargo` directly without the wrapper.

## Project layout

```
src/
├── main.rs              # thin binary entrypoint; no #[tokio::main]
├── lib.rs               # builds the runtime, dispatches subcommands
├── cli/
│   ├── mod.rs           # clap Args + init_tracing
│   ├── whoami.rs        # `ghfs whoami`
│   ├── mount.rs         # `ghfs mount <path>` — blocks in fuser::Session::run
│   ├── unmount.rs       # `ghfs unmount <path>` (wraps `fusermount3 -u`)
│   ├── status.rs        # `ghfs status` (parses /proc/mounts)
│   ├── refresh.rs       # `ghfs refresh` (re-fetches repo list)
│   ├── promote.rs       # `ghfs promote` (eager worktree materialization)
│   └── branch.rs        # `ghfs branch` (set/clear branch override in sqlite)
├── config/
│   ├── mod.rs           # Config + pure `resolve()` (testable)
│   └── token.rs         # Token newtype with redacting Debug/Display
├── github/
│   ├── mod.rs           # GithubClient + Conditional<T>
│   ├── types.rs         # User, Repo, Owner, Branch, Tree, TreeEntry, ...
│   ├── pagination.rs    # parse_next_link (Link: rel="next")
│   └── errors.rs        # GithubError
├── cache/
│   ├── mod.rs           # re-exports
│   ├── meta.rs          # SQLite-backed MetaCache (repos/etags/branch_heads/trees/branch_overrides)
│   ├── blobs.rs         # content-addressed BlobStore on disk
│   ├── schema.sql       # included via include_str!
│   └── errors.rs        # CacheError
└── fs/
    ├── mod.rs           # Ghfs + impl Filesystem + direct_children filter
    ├── inode.rs         # InodeTable + InodeKind
    ├── attr.rs          # FileAttr builders + mode helpers
    └── open_files.rs    # fh -> Arc<File> table (uses pread, no per-file mutex)

tests/                   # wiremock-backed integration tests per endpoint
```

## Invariants — do not break

1. **FUSE callbacks are sync; the GitHub client is async.** Bridge via
   `self.handle.block_on(async move { ... })`. `block_on` is only safe
   because fuser callback threads are **not** tokio workers. Never call
   `block_on` from inside an async function or a `#[tokio::test]`.

2. **Trees are immutable by SHA.** `MetaCache::put_tree` is `INSERT OR
   IGNORE`. Blobs in `BlobStore` are keyed by their git SHA. Never write
   logic that "refreshes" a tree or blob in place.

3. **ETags are per-resource.** `list_user_repos` sends `If-None-Match` only
   on page 1 — not on subsequent pages. The repo-list ETag lives under key
   `user_repos_v1` in `etags`; branch ETags live in the `branch_heads` row.

4. **Inode kinds are immutable and inode identity is stable.**
   `InodeTable::lookup_or_create` is keyed by `(parent_ino, name)`. Once
   an ino is allocated, its `InodeKind` is never mutated — do not add
   mutability to the `Arc`. The FS never evicts a `(parent, name)`
   mapping either: a path that has been resolved keeps the same ino for
   the life of the mount. The virtual ↔ passthrough switch (when a
   clone materializes) is achieved by *dynamic dispatch inside each FUSE
   op*, not by reallocating the inode — that's what lets a shell `cwd`
   survive `ghfs promote` without the path going stale. See
   `worktree_root_for` / `passthrough_disk_path` in `src/fs/mod.rs`. If
   you need behavior that varies per-call, add the check at the op
   level; never reach for eviction.

   **Corollary — the `branch` field on `InodeKind::Repo` is fixed at
   allocation time.** A `ghfs branch` override written mid-session does
   NOT propagate into already-allocated `Repo` inodes (their `branch`
   string is captured when the inode is first created). This is the
   price of inode-identity stability; the CLI prints a "remount to
   apply" hint when a live mount is detected. Do not introduce a code
   path that mutates the `branch` of an existing `Repo` inode.

5. **Mount is foreground.** `cli::mount::run` is synchronous and drives a
   `fuser::Session` to completion. Do not wrap it in `tokio::spawn` or
   convert it to async.

6. **Token never leaves `Token`.** `Token::Debug`/`Display` redact. Never log
   `token.expose()`; always pass the `Token` value, not its `String` form.
   The `token.expose()` accessor exists for the HTTP header builder only.

## Conventions

- **Errors.** Module-local `thiserror` enums; top-level uses `anyhow` only in
  `main.rs` / CLI handlers. Per-module errors implement `From` into the
  caller's error type. `GhfsError::to_errno()` is the single source of truth
  for FUSE error mapping.
- **Logging.** `tracing` everywhere. CLI subcommands log to stderr via
  `tracing_subscriber::fmt` so command stdout (e.g. `whoami` output) stays
  clean for piping. Default filter: `ghfs=info,warn`. Debug with
  `RUST_LOG=ghfs=debug` or `--log-level`.
- **Comments.** Write WHY, not WHAT. Inline comments are reserved for hidden
  invariants, workarounds, or surprising design decisions. Don't restate the
  code.
- **Tests.** Inline `#[cfg(test)]` for pure-function tests; `tests/*.rs` for
  integration tests using `wiremock` (one file per endpoint family). Always
  run via `nix develop --command cargo test`.

## Documentation is part of "done"

Any change that touches user-visible surface area must update both
`DOCS.md` and `README.md` in the same change. Treat this as a hard
checklist item alongside `cargo test` and `cargo clippy`. A feature
whose docs lag is considered incomplete.

User-visible surface includes:

- **CLI** — new subcommand, flag, or change to an existing flag's
  meaning / default. Update the relevant subsection under
  `DOCS.md#subcommands` *and* the `README.md` "Subcommands" table /
  Quickstart.
- **Configuration** — new TOML key, env var, or change to defaults /
  precedence. Update `DOCS.md#configuration` (TOML example, env-var
  table, any subsection that explains semantics), the root
  `example.config.toml` (add/update the commented block for the key,
  including its env var, default, and accepted values), and add at
  least a pointer from `README.md` if the option is non-obvious.
- **Filesystem layout / mount semantics** — anything that changes what
  `ls`, `cat`, `stat`, or `readlink` will observe. Update
  `DOCS.md#filesystem-layout` and `DOCS.md#mount-semantics`; if it
  changes the at-a-glance behavior, update the README mount-semantics
  bullets too.
- **Errno mapping** — any new branch in `GhfsError::to_errno` or a
  changed mapping. Update both error tables (`DOCS.md#errors-and-errnos`
  and `README.md#errors`).
- **Cache layout / on-disk paths** — new file, table, or path under the
  cache dir. Update `DOCS.md#cache-layout`.
- **Install / systemd** — new install flag, env var, or unit-file
  field. Update `DOCS.md#install` / `DOCS.md#systemd-auto-mount`.

Rules of thumb:

- `DOCS.md` is the **authoritative** reference. `README.md` is a
  marketing-flavored summary that links into `DOCS.md`. If the change is
  small (a new env var on an existing option), it may only need a
  `DOCS.md` line — but always check whether the README still reads
  truthfully after the change.
- If you rename or remove user-visible surface, scrub both files for
  the old name. Dangling references to a removed flag are worse than
  no documentation.
- Examples in docs must compile / run / be valid TOML. If you change
  the TOML schema, update every example block that uses it.

When the docs change is the *only* change, no test run is required, but
clippy + tests must still pass for any code change that ships alongside.

## Release Please

Release Please reads Conventional Commits. When the user asks you to commit,
write commit messages in this shape:

```text
<type>(optional-scope): <short imperative summary>

optional body

optional footer
```

Use these types consistently:

- `feat`: user-visible feature or behavior addition. Triggers a minor release.
- `fix`: user-visible bug fix. Triggers a patch release.
- `perf`: user-visible performance improvement. Triggers a patch release.
- `docs`: documentation-only change.
- `refactor`: code restructuring with no intended behavior change.
- `test`: test-only change.
- `build`: build system, dependency, packaging, or release config change.
- `ci`: GitHub Actions or other CI-only change.
- `chore`: maintenance that does not fit another type.
- `revert`: revert a previous commit.

Prefer a concise scope when it adds clarity: `cli`, `fs`, `cache`, `github`,
`config`, `docs`, `ci`, or `release`. Examples:

- `feat(fs): support writable materialized worktrees`
- `fix(cli): preserve stdout for whoami`
- `docs: document branch override behavior`
- `build(release): align release-please changelog sections`

For breaking changes, add `!` after the type/scope and explain the break in
the body or footer:

```text
fix(cli)!: make unmount lazy by default

BREAKING CHANGE: ghfs unmount now uses lazy unmount unless --no-lazy is set.
```

Do not manually bump `Cargo.toml`, `Cargo.lock`,
`.release-please-manifest.json`, or `CHANGELOG.md` for normal feature/fix
work. Release Please owns version and changelog edits in its release PR.

## When extending the GitHub client

1. Add the response type to `src/github/types.rs` (derive both `Serialize`
   and `Deserialize` if it will be persisted in the cache).
2. Add the method on `GithubClient` in `src/github/mod.rs`. For cacheable
   endpoints, return `Conditional<T>` and use `get_json_conditional`. For
   immutable resources (content-addressed by SHA), skip ETag.
3. Add a wiremock test file under `tests/` covering: 200 + ETag capture,
   304, 401, 403 with/without rate-limit headers, 404, unexpected status.
4. If you introduce a new HTTP header on requests, update the existing
   `whoami` test that asserts `header_exists("user-agent")` lest it drift.

## When extending the cache

1. Add the table to `src/cache/schema.sql` and add `CREATE TABLE IF NOT
   EXISTS` so re-opens are idempotent. Bump no version — Phase C has no
   migration framework yet, and the cache is rebuildable.
2. Add typed methods on `MetaCache`. Use `ON CONFLICT DO UPDATE` for upserts
   and an explicit transaction for atomic-replace patterns (see
   `put_repos`).
3. For content-addressed data (blob bodies, trees), use `INSERT OR IGNORE`
   to enforce write-once semantics.

## When extending the filesystem

1. Add or extend `InodeKind` variants — these are the "rows" of the
   filesystem state. Keep them `Clone` and immutable.
2. New FUSE ops go in `src/fs/mod.rs`. Always:
   - Resolve `ino -> InodeKind` via `self.inodes.get(ino)` (return `ENOENT`
     if missing).
   - Match on `&*kind`; return `ENOTDIR` / `EISDIR` / `EINVAL` for type
     mismatches.
   - Bridge async via `self.handle.block_on(...)`.
   - Map errors through `GhfsError::to_errno`.
3. **Default is read-only; materialized repos are writable.** Every
   write op must return `EROFS` by default. The single carve-out is
   paths under a repo whose effective branch has been materialized as
   a local worktree (via `ghfs promote` or any other call into
   `try_clone_branch`): inside such a repo, write ops forward to
   `std::fs` against `<cache>/clones/<owner>/<repo>/<fs_name>/<rel>`
   so the repo dir behaves like a normal writable folder (edit, `git
   status`, commit, etc.). `MountOption::RO` is **not** used — the
   FUSE layer enforces the policy itself, because the kernel flag
   can't express the carve-out. The check is whether
   `passthrough_disk_path(&kind)` is `Some`. New write ops must follow
   this same gate; do not add an unconditional write path.

## Build / verify checklist

Before declaring work done:

```bash
nix develop --command cargo test
nix develop --command cargo clippy --all-targets -- -D warnings
```

Both must be green. Clippy is `-D warnings`; new lints must be fixed at the
source, not silenced.

## Known caveats

- FUSE end-to-end smoke test lives at `tests/fuse_smoke.rs` but is marked
  `#[ignore]` because it needs a host where unprivileged FUSE mounts are
  permitted (`cargo test -- --ignored fuse_smoke`).
- Truncated-tree handling — for monorepos over GitHub's recursive-tree
  limit (~100k entries / ~7 MB) the virtual view logs a warning and
  omits entries beyond the cap. Promoting the repo serves it in full
  via libgit2.
