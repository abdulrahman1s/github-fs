# github-fs Documentation

`ghfs` mounts your GitHub repositories as a FUSE filesystem on Linux.
Repos are grouped by owner (`<mount>/<owner>/<repo>/...`) and each repo
directory surfaces the contents of one branch — the repo's GitHub-default
branch by default, overridable per-repo via [`ghfs branch`](#ghfs-branch).
The mount is read-only by default; once you `ghfs promote` a repo (or the
configured clone trigger fires), that repo's directory becomes a writable
passthrough to an on-disk worktree — editors and `git` work through it
transparently. Everything else stays read-only. It runs as a single binary
`ghfs`; a small library crate `github_fs` is the public test surface.

## Contents

- [Install](#install)
- [Authentication](#authentication)
- [Subcommands](#subcommands)
- [Filesystem layout](#filesystem-layout)
- [Mount semantics](#mount-semantics)
- [Configuration](#configuration)
- [Cache layout](#cache-layout)
- [systemd auto-mount](#systemd-auto-mount)
- [Errors and errnos](#errors-and-errnos)
- [How it works](#how-it-works)
- [Safety and privacy](#safety-and-privacy)
- [Development](#development)

## Install

### Install script

Download the latest prebuilt GitHub Release binary and install it to
`~/.local/bin/ghfs`:

```sh
curl -fsSL https://raw.githubusercontent.com/abdulrahman1s/github-fs/master/install.sh | sh
```

The script detects the host release target, downloads
`ghfs-<version>-<target>.tar.gz`, and verifies the `.sha256` asset when a
local SHA-256 tool is available. It is user-only by default and does not
use `sudo`, `doas`, or root permissions.

After install, the script checks whether `~/.local/bin` (or your chosen
`--bin-dir`) is on `PATH`. If it isn't, it detects your shell from
`$SHELL` and asks via `/dev/tty` (so it still works under `curl … | sh`):

```text
==> add /home/you/.local/bin to PATH in /home/you/.zshrc? [Y/n]
```

Pressing Enter accepts the default and appends a PATH export. The append
is idempotent — re-running the installer won't duplicate entries. If
`PATH` already contains the bin dir, the prompt is skipped entirely.

If `/dev/tty` is unavailable (CI, non-interactive shell), the rc file is
left alone and the installer prints a hint instead. Pass `--yes` to make
it append automatically in that case.

Skip the prompt with explicit flags:

```sh
# auto-accept the prompt
curl -fsSL https://raw.githubusercontent.com/abdulrahman1s/github-fs/master/install.sh | sh -s -- --yes

# never touch any rc file
curl -fsSL https://raw.githubusercontent.com/abdulrahman1s/github-fs/master/install.sh | sh -s -- --no-modify-rc

# pick a specific shell instead of auto-detecting
curl -fsSL https://raw.githubusercontent.com/abdulrahman1s/github-fs/master/install.sh | sh -s -- --zshrc
curl -fsSL https://raw.githubusercontent.com/abdulrahman1s/github-fs/master/install.sh | sh -s -- --bashrc
curl -fsSL https://raw.githubusercontent.com/abdulrahman1s/github-fs/master/install.sh | sh -s -- --fishrc
```

`GHFS_YES=1` and `GHFS_NO_MODIFY_RC=1` work as environment-variable
equivalents of `--yes` and `--no-modify-rc`.

Shell completions are installed alongside the binary into XDG-standard
per-user locations (auto-detected from `$SHELL` or `--shell`):

| Shell | Path |
| ----- | ---- |
| bash  | `${XDG_DATA_HOME:-~/.local/share}/bash-completion/completions/ghfs` |
| zsh   | `${XDG_DATA_HOME:-~/.local/share}/zsh/site-functions/_ghfs` |
| fish  | `${XDG_CONFIG_HOME:-~/.config}/fish/completions/ghfs.fish` |

Bash and fish auto-discover these paths and need no shell-rc changes.
Zsh needs the completion dir on `$fpath`; when rc edits are allowed the
installer appends an `fpath=(...)` line and an `autoload -Uz compinit &&
compinit` call to `~/.zshrc` (re-running `compinit` is idempotent and
safe under oh-my-zsh / zinit / prezto). With `--no-modify-rc` set the
installer prints those lines instead of writing them.

Opt out with `--no-completions` or `GHFS_NO_COMPLETIONS=1` to leave
the completion paths untouched.

The installer also warns when `/dev/fuse` is missing or when neither
`fusermount3` nor `fusermount` is on `PATH`, with hints for loading the
kernel module or installing libfuse3.

### Uninstall

```sh
sh install.sh --uninstall
```

This removes the binary from the resolved `BIN_DIR`. It does **not**
touch `~/.config/ghfs/` or `~/.cache/ghfs/` — wipe those by hand if you
want a clean slate.

### From source

```sh
git clone https://github.com/abdulrahman1s/github-fs.git
cd github-fs
cargo build --release
mkdir -p ~/.local/bin
install -m 0755 target/release/ghfs ~/.local/bin/ghfs
```

You need the `fusermount3` helper and the kernel FUSE module on the
host. `ghfs` no longer links against libfuse at build time — it drives
the kernel FUSE protocol directly and shells out to `fusermount3` for
the mount / unmount handshake — so the `*-dev` headers are not needed:

| Distro | Package |
| ------ | ------- |
| Debian / Ubuntu | `fuse3` |
| Fedora / RHEL | `fuse3` |
| Arch | `fuse3` |
| Alpine | `fuse3` |
| NixOS | use the bundled `shell.nix` / flake |

If the kernel module isn't loaded:

```sh
sudo modprobe fuse
```

### Nix flakes

From this checkout:

```sh
nix run .#ghfs -- --help
nix profile install .#ghfs
```

Both the `github-fs` package and the `prebuilt` derivation ship bash,
zsh, and fish completions into the standard nix paths
(`$out/share/bash-completion/completions/ghfs`,
`$out/share/zsh/site-functions/_ghfs`,
`$out/share/fish/vendor_completions.d/ghfs.fish`). Home-Manager and
NixOS pick these up automatically.

In a NixOS flake, add this repo as an input and enable the module:

```nix
{
  inputs.github-fs.url = "github:abdulrahman1s/github-fs";
  inputs.github-fs.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { nixpkgs, github-fs, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      modules = [
        github-fs.nixosModules.default
        {
          programs.github-fs.enable = true;
        }
      ];
    };
  };
}
```

To install a GitHub Release binary instead of building from source,
enable `prebuilt` and pin the release tarball hash:

```nix
programs.github-fs = {
  enable = true;
  prebuilt = {
    enable = true;
    version = "0.1.0";
    hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
  };
};
```

Get the hash with:

```sh
nix store prefetch-file --json \
  https://github.com/abdulrahman1s/github-fs/releases/download/v0.1.0/ghfs-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
```

The module can also install a systemd **user** service that mounts the
filesystem on login — see [systemd auto-mount](#systemd-auto-mount).

## Authentication

`ghfs` needs a GitHub personal access token. Resolution order is:

1. `--token` CLI flag (wins)
2. `GHFS_TOKEN` environment variable
3. `GITHUB_TOKEN` environment variable (compat fallback)
4. `token = "..."` in the config file

Scopes:

| What you want to read | Token scope |
| --------------------- | ----------- |
| Public repos only | none |
| Your private repos | `repo` |
| Org repos | `repo` plus org SSO authorization |

Create a fine-grained token at
<https://github.com/settings/personal-access-tokens/new>. Pick "Read-only"
for **Contents** and **Metadata** on the repos you care about. `ghfs`
never writes back to GitHub.

The token is wrapped in a `Token` newtype whose `Debug`/`Display` impls
redact, so it never surfaces in logs or panics. Anyone reading the
config file or cache on disk can still see what your token unlocked — see
[Safety and privacy](#safety-and-privacy).

Smoke-test that the wiring works:

```sh
ghfs whoami
# -> abdulrahman1s
```

## Subcommands

### `ghfs whoami`

Hits `GET /user` and prints the authenticated login on stdout. Errors go
to stderr. Exits non-zero on any auth failure. Useful in CI and from
config-management tools to assert that the token is live.

### `ghfs mount <path>`

Mounts the filesystem at `<path>` and **blocks** in `fuser::mount2`
until the kernel detaches. The mount is foreground by
design: Ctrl-C in this terminal sends SIGINT/SIGTERM, which the process
handles by calling `fusermount3 -u` for a clean unmount.

Flags:

```text
--foreground          Reserved; mount is foreground in all current builds.
--cache-dir DIR       Override the cache directory for this mount.
```

The mount is read-only by default; writes return `EROFS` unless the
target sits under a materialized repo (see [Mount
semantics](#mount-semantics)). The FUSE layer enforces the policy
itself — `MountOption::RO` is **not** used, because the kernel flag
can't express the per-repo writable carve-out.

Immediately after the mount succeeds, `ghfs` spawns a background task
that pre-fetches everything `ls` would touch on first use:

1. **Repo list** — GraphQL warmup (with REST fallback), so
   `ls <mount>` is instant.
2. **Effective-branch trees** — recursive tree at HEAD of each repo's
   effective branch (override or default), fanned out with a small
   concurrency cap. Makes `ls <mount>/<owner>/<repo>` instant. Reads
   sqlite first and only hits the API for trees not already cached.

If the kernel races ahead of the repo-list prefetch, the FUSE-driven
`list_repos` serializes on the same in-flight fetch instead of
duplicating the GitHub call. The tree prefetch is best-effort: per-repo
errors are logged, and the slow on-demand path remains the
always-available fallback.

### `ghfs unmount <path>`

Wraps `fusermount3 -uz <path>` (lazy unmount) so you don't have to
remember the flag. The mount disappears from the namespace immediately;
the kernel frees it once the last open handle or `cwd` reference goes
away. Use it from a second shell when the mount process is held open by
a shell currently `cd`'d into the mountpoint — the lazy default handles
that case transparently.

Flags:

```text
--strict  Refuse to detach when the mountpoint is busy. Maps to plain
          `fusermount3 -u`. Use this when you specifically want a
          busy-error and the holder PIDs surfaced (e.g. to debug a
          stray process holding the mount) rather than the silent
          lazy-detach the default offers.
```

When `--strict` is set and the kernel returns *Device or resource busy*,
`ghfs unmount` prints the PIDs to investigate (`fuser -vm <path>` /
`lsof +f -- <path>`) and points at dropping `--strict` as the escape
hatch.

### `ghfs status`

Parses `/proc/mounts`, filters for `fuse.ghfs` entries, and prints them.
Useful when you've lost track of where a mount lives.

### `ghfs info`

Print repository metadata for the repo identified by a path inside an
active ghfs mount.

```text
ghfs info ~/ghfs/<owner>/<repo>
ghfs info ~/ghfs/<owner>/<repo>/src/foo.rs   # deeper path; the rest is ignored
ghfs info ~/ghfs/<owner>/<repo> --cache-dir /tmp/ghfs-alt
```

Output includes the repo's full name, GitHub URL, description,
visibility (public/private), fork flag, and both the GitHub-default
branch and the *effective* branch — marked `(override)` when an
override has been set via [`ghfs branch`](#ghfs-branch).

The lookup reads the cached repo list first; on a cache miss it issues
one `GET /user/repos` (subject to the active `owners` / `include_forks`
/ `visibility` filters), writes the result back to the cache, and then
picks the matching row. Errors out if the repo isn't visible to your
token.

### `ghfs promote`

Manually clone a repo into the local clone store, without going through
the FUSE mount. Equivalent to what `[clone] trigger = "on_access"`
would do on first access, but invoked explicitly. Works regardless of
the configured trigger — useful for pre-staging a repo (e.g. so you can
`cd` into it offline later) or for forcing a fresh clone after wiping
the cache.

The clone is a regular non-bare git repository. Every branch is
fetched into `refs/heads/*`; the `--branch` argument (or the repo's
effective default) is the one initially checked out. Once the clone
exists, ghfs treats the working tree as the user's — switching
branches (`git checkout dev`), committing, or making local edits is
yours to manage. A repeat `ghfs promote` against an existing clone is
a no-op and will **not** touch the working tree.

`ghfs promote` requires a path inside an active ghfs FUSE mount. The
owner and repo are read from the first two path components after the
mount root:

```text
ghfs promote ~/ghfs/<owner>/<repo>                       # effective branch
ghfs promote ~/ghfs/<owner>/<repo> --branch dev
ghfs promote ~/ghfs/<owner>/<repo> --branch feature/x --cache-dir /tmp/ghfs-alt
ghfs promote ~/ghfs/<owner>/<repo>/src/foo.rs            # deeper path; the rest is ignored
```

Flags:

```text
--branch BRANCH    Branch to check out after cloning. Defaults to the repo's
                   effective branch (override set via `ghfs branch`, falling
                   back to the GitHub-default branch). All branches are
                   fetched regardless — this just selects the initial checkout.
--cache-dir DIR    Override the cache directory for this invocation.
```

The command prints the absolute clone path on success, suitable for
shell substitution:

```sh
cd "$(ghfs promote ~/ghfs/acme/widgets)"   # effective branch
git status
git checkout dev   # any branch the remote has is already a local ref
```

Progress is rendered on **stderr** so stdout stays a single path. On a
TTY the line refreshes in place; with stderr redirected, one line is
written per stage transition (and every ~2s during a long fetch). The
on-access trigger emits the same progress to the mount log (`info`-level
`clone: fetching` / `clone: checking out` events), throttled to roughly
one line per second.

Resolution rules:

- **path** — the argument must be a path that canonicalizes inside an
  active ghfs mount (as listed in `/proc/mounts`). The first path
  component after the mount root is the owner; the second is the repo.
  Components below the repo dir are ignored, so paths to files inside
  the repo are fine. Errors out if the path is not under any active
  ghfs mount, or if it stops before naming a repo dir.
- branch — `--branch` always wins; otherwise the cached `default_branch`
  is used; otherwise a live API lookup. Errors out if none of those
  yields a default (e.g. a freshly-created empty repo).
- depth — `[clone] fetch_depth = N` (or `GHFS_CLONE_FETCH_DEPTH`) caps
  each fetched branch at N commits. Useful for huge monorepos when you
  only need current contents. Unset = full history.

`promote` always creates the clone even when `[clone] trigger =
"never"`. It does *not* mutate the trigger config — but any live mount
that shares the cache directory will *automatically* start serving that
repo's files from the clone (passthrough), within roughly one second
(the kernel TTL for repo entries). No IPC needed; no restart required.

The repo dir in the mount stays a normal directory before *and* after
`promote` — there is no symlink swap. A shell `cwd` inside the repo
dir keeps working across the transition: the inode identity is stable;
only the backing data source changes (GitHub tree API → on-disk
working tree).

### `ghfs branch`

Set or clear the branch the mount surfaces under
`<mount>/<owner>/<repo>/`. Persisted in the metadata cache
(`branch_overrides`), keyed by `repo_id`. Like `promote`, the repo is
identified by a path inside an active ghfs mount.

```text
ghfs branch ~/ghfs/<owner>/<repo> <branch>      # set override
ghfs branch ~/ghfs/<owner>/<repo> --default     # clear override, fall back to GitHub default
ghfs branch ~/ghfs/<owner>/<repo>/src dev       # deeper path — the rest is ignored
```

Flags:

```text
--default          Clear the override; mount falls back to the repo's
                   GitHub-default branch. Mutually exclusive with the
                   positional `branch` argument.
--cache-dir DIR    Override the cache directory for this invocation.
```

Behavior:

- Before writing the override, ghfs hits `GET /repos/:owner/:name/branches/:branch`
  to validate the branch exists — typos are caught up front rather than
  producing an empty repo dir on next mount.
- The override is **persistent** but takes effect at **next mount**.
  Running mounts continue to surface whatever branch they resolved at
  allocation time; this is the same trade-off the FS makes to keep
  inode identity stable across `ghfs promote` (a shell `cwd` inside
  `<mount>/<owner>/<repo>/` survives the transition, at the cost of
  needing a remount to follow a branch switch). Running mounts are
  detected via `/proc/mounts` and an unmount/remount hint is printed to
  stderr.
- Path-resolution rules match `ghfs promote`: the path must canonicalize
  inside an active ghfs mount, and `<owner>/<repo>` are read from the
  first two path components after the mount root.

### `ghfs completions`

Print a shell-completion script to stdout. Supported shells:
`bash`, `zsh`, `fish`, `elvish`, `powershell`.

```sh
# zsh — drop into any directory on $fpath
ghfs completions zsh > ~/.zfunc/_ghfs
# then in ~/.zshrc:
#   fpath=(~/.zfunc $fpath)
#   autoload -U compinit && compinit

# bash — source from .bashrc or drop into completion.d
ghfs completions bash > ~/.local/share/bash-completion/completions/ghfs

# fish
ghfs completions fish > ~/.config/fish/completions/ghfs.fish
```

The output is generated from the live clap definition, so flags and
subcommands stay in sync with the binary version you have installed —
regenerate after an upgrade.

### `ghfs refresh`

Re-fetches your repo list and updates the on-disk cache. Use this after
you create or delete a repo on GitHub if you don't want to wait for the
TTL to expire or for the next remount.

`refresh` doesn't touch branch heads, trees, or blobs — only the
top-level "what repos does this user have" listing. Trees are immutable
by SHA and never need invalidation; branch heads expire via ETag on the
next access.

After writing the on-disk cache, `refresh` also sends `SIGUSR1` to
every running mount that shares this cache directory — each running
mount listens for `SIGUSR1` and re-fetches the repo list in place, so
the change is reflected in `~/ghfs/<owner>/` listings immediately,
without a remount. Live mounts are discovered via pidfiles under
`<cache>/mounts/`; stale pidfiles (process gone) are reaped here.

Mounts also auto-refresh on a timer (see
[`auto_refresh_interval_secs`](#config-file), default 5 minutes), so
even without `ghfs refresh` you'll pick up new GitHub repos within
roughly the configured interval. The two paths are complementary:
`ghfs refresh` is the "I want it now" trigger; the timer is the
background poll.

## Filesystem layout

```text
<mountpoint>/
  <owner>/                # user or org login
    <repo-name>/
      README.md
      src/
        ...
```

- Two-level layout: the mount root lists owners (users / orgs) that your
  token can see at least one repo under; each owner dir lists the repos
  for that owner.
- Each repo directory mirrors a single branch's `HEAD` commit tree.
- That branch is the repo's **effective branch**:
  - the override set by [`ghfs branch`](#ghfs-branch) for this repo, or
  - the repo's GitHub-default branch (`main`, `master`, `trunk`, ...) when
    no override is set.
- Branches other than the effective one are not surfaced via the mount.
  After a `ghfs promote`, every branch is fetched into the local clone
  at `<cache>/clones/<owner>/<repo>/`, so `cd` into that dir and `git
  checkout <other>` to switch what the mount serves.
- Tags are not surfaced via the mount; check out a tag inside the
  materialized clone if you need one.

This means stable URIs like:

```sh
cat ~/ghfs/abdulrahman1s/github-fs/Cargo.toml
rg fn ~/ghfs/rust-lang/rust/src
```

always reflect what GitHub currently has on the effective branch,
modulo the branch-head ETag cache TTL.

## Mount semantics

| Property | Behavior |
| -------- | -------- |
| Layout | Two virtual levels above any repo: `<mount>/<owner>/<repo>/<content>`. Owner dirs are virtual (always read-only); the passthrough carve-out applies one level down, per repo. |
| Mode | **Read-only by default; materialized repos are writable.** Writes outside a repo that has a materialized clone return `EROFS`. Writes inside one pass through to the on-disk working tree (so `git status`, editors, `git commit`, etc. all work). |
| Inodes | Stable within a single mount session. Allocated lazily per `(parent_ino, name)`. Survive the virtual→passthrough transition with no reallocation, so a shell `cwd` keeps working across `ghfs promote`. |
| Effective branch | Resolved once per repo at mount-time. `ghfs branch` writes are picked up at the **next mount**; running mounts continue to surface the branch they resolved at allocation time. |
| File modes | `100644` → 0644, `100755` → 0755, `120000` → symlink, `160000` (submodule) → empty dir, `040000` → 0755 dir. Inside a materialized repo, modes reflect the real on-disk file. |
| Symlinks | Surfaced as real symlinks; `readlink` returns the blob body, or the real on-disk target under passthrough. |
| Submodules | Show as empty directories — gitlinks are not followed. |
| Large trees | GitHub truncates recursive trees over ~100k entries or ~7 MB. ghfs logs a warning; entries beyond the cap are omitted from the virtual view. Promote the repo (`ghfs promote`) to read it in full via libgit2. |
| `stat` times | Virtual mode: all set to the process start time (GitHub's tree API doesn't expose per-entry mtimes). Passthrough mode: real disk times. |
| Sizes | Virtual mode: blob size from the tree response. Passthrough mode: real disk size. |
| Reads | First `open` triggers one `GET /repos/:o/:r/git/blobs/:sha` if the blob isn't cached, then all `read` calls `pread` against the local blob file. With a materialized clone, reads come straight from the on-disk working tree (passthrough) and skip the blob cache entirely. |
| Writes | `EROFS` everywhere by default. Inside a materialized repo, the following ops are forwarded to `std::fs` against the working tree: `create`, `write`, `mkdir`, `unlink`, `rmdir`, `rename`, `setattr` (chmod / truncate / utimes), `symlink`, `fsync`. Cross-repo rename returns `EXDEV`. |

The FUSE layer enforces the read-only policy itself (returning `EROFS`
from every write op unless the target ino sits under a materialized
clone). `MountOption::RO` is **not** used — the kernel flag can't
express the carve-out for materialized repos.

## Configuration

Resolution order (later wins):

1. config file at `~/.config/ghfs/config.toml`
2. environment variables
3. CLI flags

### Config file

```toml
# ~/.config/ghfs/config.toml
token          = "ghp_..."             # GitHub PAT
mount_path     = "/home/you/ghfs"      # default mountpoint for `ghfs mount`
cache_dir      = "/home/you/.cache/ghfs"
cache_ttl_secs = 300                   # for resources that don't carry ETags
log_level      = "ghfs=info"

# Background repo-list refresh interval (seconds). Default 300 (5 min);
# set to 0 to disable. The mount sends a conditional `If-None-Match`
# every tick, so the steady state is a cheap 304.
# auto_refresh_interval_secs = 300

# Restrict which repos appear in the mount.
# "self-only" (default), "all", or an array of owner logins.
owners         = "self-only"
# owners       = "all"
# owners       = ["abdulrahman1s", "rust-lang"]

# Hide fork repositories from the mount (default). Set to true to keep them.
include_forks  = false

# Filter repos by their GitHub visibility flag.
# "all" (default), "public" / "public-only", "private" / "private-only".
visibility     = "all"

# On-demand libgit2 clone (opt-in). When enabled, ghfs maintains a non-bare
# clone of each visited repo under <cache_dir>/clones/<owner>/<repo>/ and
# serves tree listings + file reads from the local object DB once the clone
# exists. The GitHub API path remains the fallback whenever a clone is
# missing or a libgit2 step fails — enabling this never *removes*
# capability, it only adds a local-first source.
[clone]
trigger     = "never"   # "never" (default) | "on_list" | "on_read" | "on_access"
# fetch_depth = 1       # shallow-clone depth; unset / 0 = full history
```

The parser rejects unknown keys to catch typos early. `chmod 0600
~/.config/ghfs/config.toml` because it contains a credential.

#### Filtering which repos appear

| `owners` value | Effect |
| -------------- | ------ |
| `"self-only"` *(default)* | Only repos owned by the authenticated user. |
| `"all"` | Every repo the token can see — owned, collaborator, and organization-member. |
| `["alice", "rust-lang"]` | Only repos whose owner login is in the allowlist (case-insensitive). |
| `"alice"` *(bare string)* | Shorthand for a single-owner allowlist — a typo'd preset becomes a list of one rather than silently falling back to `self-only`. |

`include_forks = false` (default) hides repositories where GitHub's
`fork` flag is true. Set it to `true` to keep forks alongside originals
— useful if you maintain long-lived fork branches.

| `visibility` value | Effect |
| ------------------ | ------ |
| `"all"` *(default)* | No visibility filtering — both public and private repos appear. |
| `"public"` / `"public-only"` | Only repos with GitHub's `private = false` flag. |
| `"private"` / `"private-only"` | Only repos with GitHub's `private = true` flag. |

The visibility check is applied client-side, alongside the fork and
owner filters. It does *not* change the fetch URL or ETag cache key, so
switching `visibility` between sessions retrims the same cached list
rather than triggering a refetch.

#### Clone-on-demand

The `[clone]` section opts the mount into a parallel data source backed
by `libgit2`. When a clone exists for a repo, ghfs's behavior changes
in two ways:

1. **Inside the mount, ops on that repo dir pass through to the on-disk
   working tree** at `<cache>/clones/<owner>/<repo>/`. `ls`, `cat`,
   `stat`, `readlink` all read directly from real disk, not from the
   GitHub API. The repo dir itself is still a regular directory in the
   mount — *not* a symlink — so a shell `cwd` into the repo is
   unaffected by the moment the clone is materialized.
2. **For repos without a clone yet, the FUSE-virtual path still works**
   exactly as before — `ls`, `cat`, etc. continue to be served from the
   GitHub API. The clone-backed shortcut never *removes* capability; it
   only adds a faster, real-filesystem source once it exists.

The clone is a regular non-bare git repository with **every branch
fetched** into `refs/heads/*`. The branch initially checked out is the
effective branch at clone time; `cd` into the dir and `git checkout
<other>` to switch what the mount serves. ghfs never re-checks-out for
you, so your working-tree state (dirty files, in-progress commits) is
yours to manage.

The switch between virtual and passthrough is a per-call dispatch
decision; the FUSE inode for the repo (and every descendant) is
allocated once and keeps its identity for the life of the mount. This
is the property that lets `getcwd` survive `ghfs promote` — there is
no inode flip to invalidate the shell's open `cwd`.

| `trigger` value | When the clone is materialized |
| --------------- | ------------------------------ |
| `"never"` *(default)* | Never automatically. `ghfs promote` is the only path. |
| `"on_list"` | First `readdir` into a repo (e.g. `ls ~/ghfs/foo`). |
| `"on_read"` | First `open` of a file inside a repo. Listing alone won't trigger it. |
| `"on_access"` | First lookup of a repo path (e.g. `cd ~/ghfs/foo`). Eager: the first access blocks until the clone completes, so the very first listing already comes from disk. |

`fetch_depth = N` caps each fetched branch at N commits — useful when
cloning a giant monorepo just to browse current contents. Unset (or
`0`) fetches full history. Env: `GHFS_CLONE_FETCH_DEPTH`.

`on_list` and `on_read` fire *after* the kernel has already walked into
the repo dir, so within the same session the triggering op completes
via the virtual path; the *next* access (within the 1-second repo
TTL) is the first one routed to disk. `on_access` collapses that into
a single step.

The transition also propagates **across processes**: repo entries
have a 1-second kernel TTL. So `ghfs promote` from another shell starts
serving from the on-disk clone within ~1s in the running mount without
any IPC — no symlink swap, no inode reallocation, just a different
backing store on the next per-call dispatch. (`ghfs branch` is **not**
in this category — it requires a remount; see the
[subcommand](#ghfs-branch).)

Each clone is **non-bare** and **fetches every branch**: a single
`git init` + `fetch +refs/heads/*:refs/heads/*` populates all branches
into the same working tree's object DB. The initial checkout is the
branch passed to `ensure_clone` (the repo's effective branch at
trigger / promote time). There is no automatic re-fetch within a mount
session — to advance to new commits, `cd` into the clone and `git
pull` (or `git fetch && git checkout <other>` to switch branches).

The token configured for the mount is passed to libgit2 via HTTP basic
auth (`username = x-access-token`), which works for both classic and
fine-grained PATs.

The disk layout under the cache:

```text
<cache>/clones/<owner>/<repo>/         # one non-bare clone per repo
<cache>/clones/<owner>/<repo>/.git/    # holds every branch in refs/heads/*
```

Notes:

- **Repos always look like directories in the mount.** `ls -l
  ~/ghfs/` shows `dr-xr-xr-x` for every repo, virtual or passthrough;
  the distinction is invisible to userspace by design.
- **Manual deletion is safe.** `rm -rf` the clone dir and the mount's
  next repo lookup falls back to the virtual path (or re-clones on the
  next trigger / `promote`).
- **Editing through the mount works after promote.** Inside a
  materialized repo, write ops (`create`, `write`, `mkdir`, `unlink`,
  `rmdir`, `rename`, `chmod`, `truncate`, `symlink`, `fsync`) are
  forwarded to the on-disk working tree. So `vim ~/ghfs/repo/foo.rs`,
  `git status`, and `git commit` all work from the mount path. Writes
  *outside* a materialized repo still return `EROFS`. The cache path
  at `<cache>/clones/<owner>/<repo>/` remains a normal git checkout
  if you'd rather work there directly.

### Environment variables

| Variable | Meaning |
| -------- | ------- |
| `GHFS_TOKEN` | GitHub PAT. Wins over `GITHUB_TOKEN`. |
| `GITHUB_TOKEN` | Fallback PAT, accepted for compatibility. |
| `GHFS_CACHE_DIR` | Override the cache directory. |
| `GHFS_CACHE_TTL_SECS` | Override `cache_ttl_secs`. |
| `GHFS_AUTO_REFRESH_INTERVAL_SECS` | Override `auto_refresh_interval_secs`. `0` disables. |
| `GHFS_MOUNT_PATH` | Default mount path. |
| `GHFS_LOG_LEVEL` | `tracing` filter string, e.g. `ghfs=debug`. |
| `GHFS_OWNERS` | Override `owners`. Accepts a preset (`self-only`, `all`), a single login, or a comma-separated list (e.g. `alice,rust-lang`). |
| `GHFS_INCLUDE_FORKS` | Override `include_forks`. Accepts `1`/`0`, `true`/`false`, `yes`/`no`, `on`/`off`. |
| `GHFS_VISIBILITY` | Override `visibility`. Accepts `all`, `public` (`public-only`), or `private` (`private-only`). |
| `GHFS_CLONE_TRIGGER` | Override `[clone] trigger`. Accepts `never`/`off`/`disabled`, `on_list`/`on-list`/`list`, `on_read`/`on-read`/`read`, or `on_access`/`on-access`/`access`. |
| `GHFS_CLONE_FETCH_DEPTH` | Override `[clone] fetch_depth`. Positive integer = shallow clone depth; `0` = full history (same as unset). |
| `RUST_LOG` | Standard `tracing` env var; lower precedence than `GHFS_LOG_LEVEL`. |

### CLI flags

Two flags are global:

```text
--token TOKEN          GitHub PAT. Overrides env and config file.
--log-level FILTER     tracing filter, e.g. ghfs=debug,warn.
```

Per-subcommand flags are listed under [Subcommands](#subcommands).

### Default paths

| Path | What it is |
| ---- | ---------- |
| `~/.config/ghfs/config.toml` | Config file (XDG, via the `directories` crate). |
| `~/.cache/ghfs/` | Default cache root (XDG, fallback `/tmp/ghfs-cache`). |
| `~/.cache/ghfs/meta.db` | SQLite metadata DB. |
| `~/.cache/ghfs/blobs/aa/<sha>` | Content-addressed blob store. |
| `~/.cache/ghfs/clones/<owner>/<repo>/` | Non-bare libgit2 clone (one per repo) — only present when `[clone] trigger` is enabled or after `ghfs promote`. Holds every branch in `refs/heads/*` plus a working tree. |
| `~/.cache/ghfs/mounts/<encoded-mountpath>.pid` | Pidfile per live mount, used by `ghfs refresh` to signal `SIGUSR1`. Removed on graceful shutdown; stale entries reaped by `ghfs refresh`. |

## Cache layout

`ghfs` keeps two caches under `cache_dir`:

### SQLite metadata DB (`meta.db`)

| Table | Purpose |
| ----- | ------- |
| `repos` | Owner/name/default-branch list for the authenticated user. |
| `etags` | ETags for cacheable list endpoints (`user_repos_v1`, etc.). |
| `branch_heads` | Per-branch HEAD commit SHA plus its ETag. |
| `trees` | Recursive trees, keyed by their tree SHA. Immutable, written `INSERT OR IGNORE`. |
| `branch_overrides` | Per-repo override (set via `ghfs branch`) that swaps which branch the mount surfaces. Absent → use the repo's GitHub default. |

Trees and blobs are **content-addressed**: they're keyed by their git
SHA. They never need invalidation. The cache is rebuildable — wipe
`~/.cache/ghfs/` and the next access will refill.

### Blob store

Raw file bytes live at `<cache_dir>/blobs/<aa>/<sha>` where `<aa>` is the
first two characters of the SHA (fan-out so a single directory never
holds 50k files). Writes are atomic via `NamedTempFile + persist`.

### Clone store

When `[clone] trigger` is anything other than `"never"` (or when `ghfs
promote` runs), ghfs keeps a non-bare libgit2 clone of each visited
repo at `<cache_dir>/clones/<owner>/<repo>/`. Each clone is a regular
git working tree with **every branch fetched** into `refs/heads/*` and
the repo's effective branch initially checked out. After the first
clone, ghfs never touches the working tree again — `git checkout`,
local edits, in-progress commits are all yours. The clones are
**rebuildable** in the same sense as the rest of the cache — delete
the directory and the next access (with the trigger still on) will
re-clone.

`fetch_depth = N` makes the fetch shallow (libgit2's `--depth N`),
which is useful for huge monorepos when you only need current
contents. Note: libgit2's local (`file://`) transport does **not**
support shallow fetches; this only kicks in for real `https://`
remotes.

Under `trigger = "on_list"` / `"on_read"`, the clone store is a parallel
source: tree listings and blob reads first consult the clone, and fall
back to the GitHub API on miss. The metadata DB and blob store are still
populated as before, so disabling `[clone]` later cleanly returns to the
API-only flow with the existing on-disk caches intact.

Under `trigger = "on_access"`, the clone is materialized synchronously
on first lookup into the repo dir, so the very first listing already
comes from disk. Switching what the mount serves between branches is a
matter of `cd ~/.cache/ghfs/clones/<owner>/<repo> && git checkout
<other>`; the mount picks up the new working-tree contents on the
next stat (kernel attr TTL is forced to zero under passthrough so
external `git` writes are visible immediately).

### Conditional requests

Cacheable endpoints (the repo list, branch HEAD) send `If-None-Match`
with the stored ETag. A 304 is a free read — it doesn't count against
your GitHub rate limit. Pagination is special-cased: `list_user_repos`
only sends `If-None-Match` on page 1, because GitHub's ETag is for the
combined response.

Immutable resources (trees by SHA, blobs by SHA) skip ETag entirely —
they can never change.

## systemd auto-mount

`ghfs` runs as a long-lived foreground process. The natural way to have
it always available is a systemd **user** service that starts on login,
unmounts cleanly on logout, and restarts on failure.

### Via the install script

The shipped `install.sh` can drop a unit at
`~/.config/systemd/user/ghfs.service` for you. By default it asks before
writing — accept the prompt and the unit is created (but **not** enabled
yet). Flags to skip the prompt:

```sh
# write the unit and skip the prompt; enable it manually later
sh install.sh --service

# write + enable + start in one shot
sh install.sh --service-enable

# custom mountpoint (default is $HOME/ghfs)
sh install.sh --service-enable --service-mount-path "$HOME/repos"

# read GHFS_TOKEN from a 0600 file via EnvironmentFile=
sh install.sh --service-enable --service-token-file "$HOME/.config/ghfs/token.env"

# never offer the prompt
sh install.sh --no-service
```

Environment-variable equivalents: `GHFS_SERVICE=1`,
`GHFS_SERVICE_ENABLE=1`, `GHFS_NO_SERVICE=1`,
`GHFS_SERVICE_MOUNT_PATH=...`, `GHFS_SERVICE_TOKEN_FILE=...`,
`GHFS_SERVICE_UNIT=...`.

The token file should look like:

```text
GHFS_TOKEN=ghp_xxx
```

`chmod 0600` it. If you don't pass `--service-token-file`, the unit
inherits whatever environment systemd's user manager has — set
`GHFS_TOKEN` there (e.g. via `systemctl --user import-environment` from
your shell login profile) or rerun the installer with the token file.

`sh install.sh --uninstall` removes both the binary and the systemd
unit (after disabling it).

### Inspect or control

```sh
systemctl --user status ghfs.service
systemctl --user restart ghfs.service
systemctl --user stop ghfs.service       # triggers `fusermount3 -u` cleanly
journalctl --user -u ghfs.service -f
```

The unit sets `KillSignal=SIGINT` to match the foreground Ctrl-C
behavior exactly: SIGINT triggers ghfs's clean-unmount path before the
process exits.

### Via the NixOS module

The NixOS module can install the same systemd user service declaratively.
Enable it alongside `programs.github-fs.enable`:

```nix
{
  programs.github-fs = {
    enable = true;

    autoMount = {
      enable    = true;
      mountPath = "/home/you/ghfs";   # required; will be created on start
      # tokenFile = "/run/secrets/ghfs-token"; # optional; sets GHFS_TOKEN
      # cacheDir  = "/home/you/.cache/ghfs";   # optional override
      # extraArgs = [ "--log-level" "ghfs=info" ];
    };
  };
}
```

The module's service uses the same shape as the one written by
`install.sh`: foreground, `KillSignal=SIGINT`, `Restart=on-failure`,
`ExecStop=fusermount3 -u`. `cacheDir`, `tokenFile`, and `extraArgs` flow
into `--cache-dir`, `EnvironmentFile=`, and trailing `ghfs mount` args.

### Hand-rolled (non-NixOS, non-install.sh)

If you'd rather wire up the unit by hand, the minimum looks like:

```ini
# ~/.config/systemd/user/ghfs.service
[Unit]
Description=Mount GitHub repositories as a FUSE filesystem (ghfs)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStartPre=/usr/bin/mkdir -p %h/ghfs
ExecStart=%h/.local/bin/ghfs mount %h/ghfs
ExecStop=/usr/bin/fusermount3 -u %h/ghfs
Environment=GHFS_TOKEN=ghp_...
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now ghfs.service
```

Prefer an `EnvironmentFile=` pointing at a `0600` file over inlining the
token in the unit.

## Errors and errnos

GitHub errors are translated to errnos at the FUSE boundary by
`GhfsError::to_errno`:

| Cause | errno | When you'll see it |
| ----- | ----- | ------------------ |
| 401 Unauthorized / 403 Forbidden (no rate-limit header) | `EACCES` | Bad/expired token, or token lacks the scope for that repo. |
| 403 with `X-RateLimit-Remaining: 0` | `EAGAIN` | Hit GitHub's rate limit; retry later. |
| 404 Not Found | `ENOENT` | Repo/branch/file doesn't exist (or you can't see it). |
| Network failure, 5xx, decode failure | `EIO` | Transient. Retry. |
| Write attempt outside a materialized repo | `EROFS` | Promote the repo first (`ghfs promote`), or work in the cache path directly. |
| Cross-repo rename | `EXDEV` | Renames must stay inside a single materialized repo — different repos are different worktrees on disk. |
| Passthrough syscall failed (e.g. ENOSPC, EACCES on disk) | passthrough errno | Whatever the underlying `std::fs` op returned; ghfs forwards `raw_os_error()` unchanged. |
| Wrong inode kind for op (e.g. `read` on a dir) | `EISDIR` / `ENOTDIR` / `EINVAL` | Logic error in caller — `ls` won't hit this. |

For verbose op tracing:

```sh
RUST_LOG=ghfs=debug ghfs mount ~/ghfs
```

## How it works

1. `ghfs mount` builds a tokio runtime and a `Ghfs` filesystem object
   that owns the GitHub client, the SQLite cache, the blob store, an
   inode table, and an open-file table.
2. `fuser::mount2` registers the filesystem with the kernel and blocks.
3. FUSE callbacks run on fuser-spawned threads (not tokio workers). They
   bridge into async code with `Handle::block_on` — safe because the
   callback threads are not tokio workers.
4. `lookup` / `readdir` consult the in-memory inode table; on miss, they
   ask the GitHub client. The first `readdir` on a repo fetches one
   recursive tree (`/repos/:o/:r/git/trees/:sha?recursive=1`) at HEAD of
   the repo's effective branch and stores it in SQLite, keyed by tree
   SHA. All subsequent `readdir` calls in that repo walk the in-memory
   tree index.
5. `open` either finds the blob already on disk under
   `<cache_dir>/blobs/<aa>/<sha>` or fetches it with
   `GET /repos/:o/:r/git/blobs/:sha` (raw accept), then opens the local
   file. The fd is stored in an `Arc<File>` table keyed by fuse `fh`.
6. `read` is `pread` on the local file — fast, no per-file mutex, no
   network round-trip after the first open.
7. On SIGINT/SIGTERM the process calls `fusermount3 -u <mountpoint>` so
   the kernel detaches cleanly.

```text
   user:  ls ~/ghfs/foo/src
       |
       v
   FUSE kernel module --readdir--> fuser thread
       |
       v
   Ghfs::readdir
       |  (cache hit)            (cache miss)
       |    \                        \
       |     in-memory tree           block_on(github.get_tree(sha))
       |     entries                    -> insert into SQLite by SHA
       v
   reply with entries
```

## Safety and privacy

`ghfs` never pushes to GitHub. Writes to the mount either return
`EROFS` (virtual repos) or land in the on-disk working tree at
`<cache>/clones/<owner>/<repo>/` (materialized repos) — pushing those
local commits up is your call (`git push` inside the clone).

What it _does_ do is talk to GitHub on your behalf and cache the
responses on local disk:

- **Token handling.** The token is wrapped in a `Token` newtype whose
  `Debug`/`Display` redact. `--token`, `GHFS_TOKEN`, `GITHUB_TOKEN`, and
  the config file are the only ways it enters the process; the
  `expose()` accessor exists only for the HTTP header builder.
- **Where the token sits at rest.** `~/.config/ghfs/config.toml` if you
  put it there — set it to `chmod 0600`. Otherwise it's only in the
  process environment.
- **Cache contents.** `~/.cache/ghfs/blobs/` contains the raw bytes of
  every file you've opened through the mount. Treat the directory as
  sensitive: anything your token can read is going to land there.
- **Mountpoint permissions.** Mount on a directory only you can read.
  `allow_other` is **not** enabled by default, so the FUSE mount is
  scoped to your uid.
- **Logging.** Default filter is `ghfs=info,warn`. Debug-level logs
  include request URLs and inode operations, but never the bearer token
  (the `Token` redacts).

This is not a permission boundary. Anyone who can read your home
directory can read your cache.

## Development

All commands inside `nix-shell` (provides `pkg-config` + `fuse3`
headers):

```sh
nix-shell --run "cargo check --all-targets"
nix-shell --run "cargo test"
nix-shell --run "cargo clippy --all-targets -- -D warnings"
nix-shell --run "cargo fmt --check"
```

Both `cargo test` and `cargo clippy -- -D warnings` must be green before
declaring work done. New lints are fixed at the source, not silenced.

### Project layout

```text
src/
├── main.rs              # thin binary entrypoint
├── lib.rs               # builds the runtime, dispatches subcommands
├── cli/
│   ├── mod.rs           # clap Args + init_tracing
│   ├── whoami.rs        # `ghfs whoami`
│   ├── mount.rs         # `ghfs mount` (blocks in fuser::mount2)
│   ├── unmount.rs       # `ghfs unmount`
│   ├── status.rs        # `ghfs status` (/proc/mounts scan)
│   ├── refresh.rs       # `ghfs refresh`
│   ├── promote.rs       # `ghfs promote` (materialize worktree)
│   └── branch.rs        # `ghfs branch` (set/clear mount-visible branch)
├── config/
│   ├── mod.rs           # Config + pure resolve()
│   └── token.rs         # Token newtype with redacting Debug/Display
├── github/
│   ├── mod.rs           # GithubClient + Conditional<T>
│   ├── types.rs         # User, Repo, Branch, Tree, TreeEntry, ...
│   ├── pagination.rs    # parse_next_link (Link: rel="next")
│   └── errors.rs        # GithubError
├── cache/
│   ├── mod.rs           # re-exports
│   ├── meta.rs          # SQLite-backed MetaCache
│   ├── blobs.rs         # content-addressed BlobStore on disk
│   ├── schema.sql       # included via include_str!
│   └── errors.rs        # CacheError
└── fs/
    ├── mod.rs           # Ghfs + impl Filesystem
    ├── inode.rs         # InodeTable + InodeKind
    ├── attr.rs          # FileAttr builders + mode helpers
    └── open_files.rs    # fh -> Arc<File> table (uses pread)
```

### Tests

130+ cases across unit tests (Token redaction, config resolution, inode
allocation, attr building, tree filtering, blob store atomicity, SQLite
roundtrips, `/proc/mounts` parsing) and `wiremock`-backed integration
tests for every GitHub endpoint and the `refresh` subcommand.

A live-FUSE smoke test exists behind `#[ignore]`:

```sh
nix-shell --run "cargo test -- --ignored fuse_smoke"
```

It needs a host where unprivileged FUSE mounts are permitted.

### Releases

`.github/workflows/build-release.yml` runs on every push and on tag
pushes that match `v*`. Pushes to the default branch with conventional
commits (`feat:`, `fix:`, `perf:`, `feat!:` etc.) drive an automatic
version bump and tag.

For a tagged release, the workflow builds `ghfs` for each matrix target,
packages `ghfs-<tag>-<target>.tar.gz`, attaches a SHA-256 checksum, and
uploads them as GitHub Release assets. `install.sh` consumes those
assets and verifies the checksum on download.

### Extending the GitHub client

1. Add the response type to `src/github/types.rs` (derive both
   `Serialize` and `Deserialize` if it will be cached).
2. Add the method on `GithubClient` in `src/github/mod.rs`. Cacheable
   endpoints return `Conditional<T>` via `get_json_conditional`;
   immutable resources (content-addressed by SHA) skip ETag.
3. Add a wiremock test file under `tests/` covering: 200 + ETag capture,
   304, 401, 403 (with and without rate-limit headers), 404, unexpected
   status.

### Extending the cache

1. Add the table to `src/cache/schema.sql` with `CREATE TABLE IF NOT
   EXISTS` for idempotent re-opens.
2. Add typed methods on `MetaCache`. Use `ON CONFLICT DO UPDATE` for
   upserts and an explicit transaction for atomic-replace patterns (see
   `put_repos`).
3. For content-addressed data, use `INSERT OR IGNORE` to enforce
   write-once semantics.

### Extending the filesystem

1. Add or extend `InodeKind` variants — these are the "rows" of the
   filesystem state. Keep them `Clone` and immutable.
2. New FUSE ops go in `src/fs/mod.rs`. Always:
   - Resolve `ino -> InodeKind` via `self.inodes.get(ino)` (return
     `ENOENT` if missing).
   - Return `ENOTDIR` / `EISDIR` / `EINVAL` for type mismatches.
   - Bridge async via `self.handle.block_on(...)`.
   - Map errors through `GhfsError::to_errno`.
3. Write ops must return `EROFS` by default. The single carve-out is
   paths under a materialized repo: forward to `std::fs` against
   `<cache>/clones/<owner>/<repo>/<rel>` when
   `passthrough_disk_path(&kind)` is `Some`. `MountOption::RO` is **not**
   used — the FUSE layer enforces the policy itself.

See `AGENTS.md` for the full set of invariants and conventions.
