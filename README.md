# Github FS

**Your entire GitHub, as a folder.**

`ghfs` mounts every repository your token can see as a single
filesystem on Linux. Browse it with `ls`, open files in your editor,
read what you need on demand. Every tool that takes a path just works,
across every repo you can reach. When you actually want to change something,
`ghfs promote` flips one repo into a real on-disk git clone in-place,
so `vim`, `git commit`, and `git push` flow straight through the mount.

```text
~/ghfs/
  abdulrahman1s/
    github-fs/
      Cargo.toml
      README.md
      src/main.rs
  rust-lang/
    rust/
      ...
  torvalds/
    linux/
      ...
```

## See for yourself

```sh
# Find every service in your org with a Dockerfile, in one command.
fd Dockerfile ~/ghfs/myorg

# Read a file from any repo, without cloning it.
cat ~/ghfs/torvalds/linux/MAINTAINERS

# Open a repo in your editor straight from the mount.
code ~/ghfs/rust-lang/rust

# Spot a bug? Promote in place, edit, commit, push.
ghfs promote ~/ghfs/myorg/api
$EDITOR ~/ghfs/myorg/api/src/server.rs
cd ~/ghfs/myorg/api && git commit -am 'fix it' && git push
```

The last block is the trick that sets `ghfs` apart.
`~/ghfs/myorg/api` is the **same path** before and after `ghfs
promote`: same shell `cwd`, same open editor buffers, same inode. It
just becomes writable, backed by a real git checkout.

## Who it's for

- **You work across a lot of repos.** An org with dozens of services,
  a personal account with years of side projects, or just open source
  you keep cloning into `~/code` and forgetting about.
- **You live in a terminal.** `fd`, `fzf`, `vim`/`nvim`, `bat`,
  anything that consumes paths is now a multi-repo tool. (Avoid tools
  that bulk-read file contents across the mount — every uncached file
  is a GitHub API round-trip; reach for `ghfs promote` first if you
  want to grep a whole repo.)
- **You want to read code without ceremony.** Skim a dependency's
  source, look up how an upstream project handles something, share a
  path with a colleague. No "let me clone it first."
- **You want one path for the whole workflow.** Read, realize you
  need to fix it, edit and commit, without ever changing directories
  or re-cloning.

## What you get

- **One mount, every repo.** No per-repo `git clone`, no remembering
  which checkout lives where. Repos land under `<mount>/<owner>/<repo>/`.
- **First read fetches; the rest is local.** Files are cached on disk
  after first access and re-validated with ETags, so re-reads don't
  burn your GitHub rate limit. Wipe `~/.cache/ghfs/` any time to start
  fresh; nothing is lost.
- **Read-only by default; writable where it matters.** Edits return
  `EROFS` everywhere except inside a repo you've `ghfs promote`'d.
  Inside that repo, ops pass through to a real working tree, so `vim`,
  `git status`, `git commit`, and `git push` all work through the
  mount.
- **One branch per repo dir** (the GitHub default by default). Swap
  per-repo with `ghfs branch <path> <other>`. Promoted repos clone
  every branch, configure `origin`, and track upstream branches — `cd`
  in and `git checkout <other>` to switch what the mount serves.
- **Filter what shows up.** Yourself, everything visible, or an owner
  allowlist. Hide forks; show only private or only public. See
  [DOCS.md](DOCS.md#filtering-which-repos-appear).
- **Clone-on-demand.** Let `ghfs` auto-promote repos the first time
  you touch them, no manual step. See [DOCS.md](DOCS.md#clone-on-demand).

## Install

Install the latest release to `~/.local/bin/ghfs`:

```sh
curl -fsSL https://raw.githubusercontent.com/abdulrahman1s/github-fs/master/install.sh | sh
```

The installer also drops bash/zsh/fish completions into the standard
XDG paths (opt out with `--no-completions`).

See [DOCS.md](DOCS.md#install) for installer flags (`--yes`,
`--no-modify-rc`, `--no-completions`, per-shell PATH-export overrides)
and other install methods.

Build from source:

```sh
git clone https://github.com/abdulrahman1s/github-fs.git
cd github-fs
cargo build --release
install -m 0755 target/release/ghfs ~/.local/bin/ghfs
```

You will also need `fusermount3` and the kernel FUSE module. Debian/Ubuntu:
`sudo apt install fuse3`. Fedora/Arch/Alpine: package `fuse3`.

<details>
<summary>NixOS users</summary>

Use the flake instead of a manual install:

```sh
nix run github:abdulrahman1s/github-fs#ghfs -- --help
nix profile install github:abdulrahman1s/github-fs#ghfs
```

For the NixOS module (with optional systemd user-service for auto-mount)
and the prebuilt-release option, see [DOCS.md](DOCS.md#nix-flakes).

</details>

## Quickstart

```sh
# point ghfs at a GitHub personal access token
export GHFS_TOKEN=ghp_xxx
# or
mkdir -p ~/.config/ghfs
echo 'token = "ghp_xxx"' > ~/.config/ghfs/config.toml

# smoke-test auth
ghfs whoami

# mount
mkdir -p ~/ghfs
ghfs mount ~/ghfs

# in another shell:
ls ~/ghfs
ls ~/ghfs/<owner>
ls ~/ghfs/<owner>/<some-repo>
cat ~/ghfs/<owner>/<some-repo>/README.md

# switch which branch <some-repo> shows (takes effect on next mount)
ghfs branch ~/ghfs/<owner>/<some-repo> dev

# Ctrl-C in the mount terminal to unmount, or from another shell:
ghfs unmount ~/ghfs

# list active ghfs mounts
ghfs status

# force-refresh the cached repo list and show added/removed repos
# — also signals every running mount via SIGUSR1 to pick up the change in place
ghfs refresh
```

Token scopes: `repo` for private repos, none for public ones.

## Subcommands

| Command | What it does |
| ------- | ------------ |
| `ghfs whoami` | Print the authenticated GitHub user. Smoke-tests auth. |
| `ghfs mount <path>` | Mount the GitHub filesystem at `<path>` (foreground). |
| `ghfs unmount <path> [--strict]` | Unmount via `fusermount3 -uz` (lazy by default — detaches a busy mount and frees it once the last reference drops). Pass `--strict` to refuse on busy and surface the holder PIDs instead. |
| `ghfs status` | List active ghfs mounts (scans `/proc/mounts`). |
| `ghfs refresh` | Re-fetch the cached repo list and show added/removed repos. |
| `ghfs info <path>` | Print repo metadata (URL, description, visibility, fork flag, default/effective branch) for the repo at `<path>` inside an active mount. |
| `ghfs promote <path> [--branch B]` | Manually clone a repo into a local working copy (`origin` configured, every branch fetched, `--branch` initially checked out). Works regardless of `[clone] trigger`. `<path>` is a path inside an active mount, e.g. `~/ghfs/<owner>/<repo>`. |
| `ghfs branch <path> <B>` | Set which branch the mount surfaces under `<mount>/<owner>/<repo>/`. `<path>` is a path inside an active mount. Persistent; applies on next mount. Pass `--default` to clear. |
| `ghfs completions <shell>` | Print a shell-completion script (`bash`, `zsh`, `fish`, `elvish`, `powershell`) to stdout. Redirect into the location your shell expects. |

See [DOCS.md](DOCS.md) for the full layout, mount semantics, configuration,
errno mapping, systemd auto-mount, caching internals, and development workflows.

## Mount semantics

* **Read-only by default; writable under materialized repos.** Writes
  outside a materialized repo return `EROFS`. Inside one, ops pass
  through to the on-disk working tree.
* **Two-level layout.** Repos live under `<mount>/<owner>/<repo>/`.
* **One branch per repo dir.** `~/ghfs/<owner>/<repo>/` is the repo's
  effective branch (override from `ghfs branch`, falling back to the
  GitHub default). Override changes take effect at the next mount.
* **Symlinks** (`mode 120000`) are surfaced as real symlinks.
* **Hard links.** `link(2)` works inside a single materialized repo+branch;
  crossing worktrees returns `EXDEV`, linking into or out of a virtual
  path returns `EROFS`. Each name gets its own FUSE inode number, so
  `st_nlink` is accurate but `st_ino`-based dedup (`du`, `tar -l`,
  `rsync -H`) doesn't recognize the link.
* **Submodules** (`mode 160000`) show as empty directories; gitlinks
  aren't followed.
* **Truncated trees** (>~100k entries or >7 MB) log a warning and may
  omit some entries; promote the repo to read it in full.

## Errors

GitHub errors are translated to errnos at the FUSE boundary:

| Cause | errno |
|---|---|
| 401 Unauthorized / 403 Forbidden (no rate-limit) | `EACCES` |
| 403 with `X-RateLimit-Remaining: 0` | `EAGAIN` |
| 404 Not Found | `ENOENT` |
| Network / 5xx / decode failure | `EIO` |

Run with `RUST_LOG=ghfs=debug` for verbose op tracing.

## Documentation

See [DOCS.md](DOCS.md) for installation variants, configuration, mount
semantics, caching internals, systemd auto-mount, privacy/security notes,
and development workflows.

## License

MIT.
