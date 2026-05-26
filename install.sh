#!/bin/sh
# Any-distro installer for ghfs.
#
# Downloads a prebuilt GitHub Release binary and installs it under ~/.local by
# default. The installer never uses sudo/doas; custom install dirs must already
# be writable by the current user.

set -eu

REPO="${GHFS_GITHUB_REPO:-abdulrahman1s/github-fs}"
VERSION="${GHFS_VERSION:-latest}"
TARGET="${GHFS_TARGET:-}"
ARCHIVE_URL="${GHFS_ARCHIVE_URL:-}"
PREFIX="${PREFIX:-}"
BIN_DIR="${BIN_DIR:-}"
TARGET_SHELL="${GHFS_SHELL:-}"

DO_UNINSTALL=0
EDIT_ZSHRC=0
EDIT_BASHRC=0
EDIT_FISHRC=0
ASSUME_YES=${GHFS_YES:-0}
SKIP_RC=${GHFS_NO_MODIFY_RC:-0}
SKIP_COMPLETIONS=${GHFS_NO_COMPLETIONS:-0}
SHELL_SETUP_DONE=0
COMPLETIONS_INSTALLED=
TMP_DIR=
GHFS_SRC=

INSTALL_SERVICE=${GHFS_SERVICE:-0}
SKIP_SERVICE=${GHFS_NO_SERVICE:-0}
ENABLE_SERVICE=${GHFS_SERVICE_ENABLE:-0}
SERVICE_MOUNT_PATH="${GHFS_SERVICE_MOUNT_PATH:-}"
SERVICE_TOKEN_FILE="${GHFS_SERVICE_TOKEN_FILE:-}"
SERVICE_UNIT_NAME="${GHFS_SERVICE_UNIT:-ghfs.service}"

usage() {
  cat <<'EOF'
Usage: sh install.sh [OPTIONS]

Download a prebuilt ghfs release from GitHub and install it for the current user.

Options:
  --version VERSION  Release version to install, with or without v (default: latest)
  --target TARGET    Release target triple (default: detect host)
  --repo OWNER/REPO  GitHub repo that hosts release assets
  --archive-url URL  Full release tarball URL; skips repo/version URL building
  --prefix DIR       Install under DIR/bin (default: $HOME/.local)
  --bin-dir DIR      Install directly into DIR
  --user             Install under $HOME/.local (the default)
  --shell SHELL      Shell whose rc file gets a PATH export: zsh, bash, or fish
  --zshrc            Append PATH export to ~/.zshrc (skips the prompt)
  --bashrc           Append PATH export to ~/.bashrc (skips the prompt)
  --fishrc           Append PATH export to ~/.config/fish/config.fish (skips the prompt)
  -y, --yes          Auto-accept the PATH-export prompt
  --no-modify-rc     Never modify a shell rc file; don't prompt
  --no-completions   Don't write shell-completion files
  --service          Install ~/.config/systemd/user/ghfs.service (skips the prompt)
  --service-enable   Also `systemctl --user enable --now` the unit
  --no-service       Never install the systemd unit; don't prompt
  --service-mount-path PATH
                     Path the service mounts at (default: $HOME/ghfs)
  --service-token-file PATH
                     Set EnvironmentFile= on the unit to PATH (contains GHFS_TOKEN=...)
  --uninstall        Remove the installed ghfs binary from the selected prefix
                     and the systemd user unit if present
  -h, --help         Show this help

By default the installer detects your shell ($SHELL) and only prompts to
append a PATH export when the install directory isn't already on PATH. Use
--yes for non-interactive installs or --no-modify-rc to opt out entirely.

Environment:
  GHFS_VERSION        Same as --version
  GHFS_TARGET         Same as --target
  GHFS_GITHUB_REPO    Same as --repo
  GHFS_ARCHIVE_URL    Same as --archive-url
  PREFIX              Same as --prefix; defaults to $HOME/.local
  BIN_DIR             Same as --bin-dir; defaults to $PREFIX/bin
  GHFS_SHELL          Default shell for the prompt
  GHFS_YES            Same as --yes
  GHFS_NO_MODIFY_RC   Same as --no-modify-rc
  GHFS_NO_COMPLETIONS Same as --no-completions when truthy (1)
  GHFS_SERVICE        Same as --service when truthy (1)
  GHFS_NO_SERVICE     Same as --no-service when truthy (1)
  GHFS_SERVICE_ENABLE Same as --service-enable when truthy (1)
  GHFS_SERVICE_MOUNT_PATH  Same as --service-mount-path
  GHFS_SERVICE_TOKEN_FILE  Same as --service-token-file
  GHFS_SERVICE_UNIT   Override the systemd unit filename (default: ghfs.service)
EOF
}

if [ -t 2 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != dumb ]; then
  BOLD=$(printf '\033[1m')
  DIM=$(printf '\033[2m')
  RED=$(printf '\033[31m')
  GREEN=$(printf '\033[32m')
  YELLOW=$(printf '\033[33m')
  CYAN=$(printf '\033[36m')
  RESET=$(printf '\033[0m')
else
  BOLD=
  DIM=
  RED=
  GREEN=
  YELLOW=
  CYAN=
  RESET=
fi

say() {
  printf '%s%s==>%s %s\n' "$BOLD" "$CYAN" "$RESET" "$*" >&2
}

ok() {
  printf ' %s✓%s %s\n' "$GREEN" "$RESET" "$*" >&2
}

warn() {
  printf ' %s!%s %s\n' "$YELLOW" "$RESET" "$*" >&2
}

hint() {
  printf '   %s%s%s\n' "$DIM" "$*" "$RESET" >&2
}

die() {
  printf '%s%serror:%s %s\n' "$BOLD" "$RED" "$RESET" "$*" >&2
  exit 1
}

cleanup() {
  if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
    rm -rf "$TMP_DIR"
  fi
}
trap cleanup EXIT HUP INT TERM

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

make_tmp_dir() {
  if command -v mktemp >/dev/null 2>&1; then
    TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ghfs-install.XXXXXX")
  else
    TMP_DIR="${TMPDIR:-/tmp}/ghfs-install.$$"
    mkdir -p "$TMP_DIR"
  fi
}

download_file() {
  url=$1
  output=$2

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$output"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$output" "$url"
  else
    die "missing required command: curl or wget"
  fi
}

download_required() {
  url=$1
  output=$2

  download_file "$url" "$output" || die "failed to download $url"
}

detect_target() {
  os=$(uname -s 2>/dev/null || true)
  arch=$(uname -m 2>/dev/null || true)

  case "$os:$arch" in
    Linux:x86_64|Linux:amd64)
      printf '%s\n' x86_64-unknown-linux-gnu
      ;;
    Linux:aarch64|Linux:arm64)
      printf '%s\n' aarch64-unknown-linux-gnu
      ;;
    *)
      die "no prebuilt release target for $os/$arch; set GHFS_TARGET or use --target"
      ;;
  esac
}

normalise_tag() {
  case "$1" in
    v*) printf '%s\n' "$1" ;;
    *) printf 'v%s\n' "$1" ;;
  esac
}

resolve_tag() {
  if [ "$VERSION" != latest ]; then
    normalise_tag "$VERSION"
    return
  fi

  latest_json="$TMP_DIR/latest.json"
  say "resolving latest GitHub release for $REPO"
  download_required "https://api.github.com/repos/$REPO/releases/latest" "$latest_json"

  tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$latest_json" | sed -n '1p')
  [ -n "$tag" ] || die "could not find tag_name in GitHub latest-release response"
  printf '%s\n' "$tag"
}

sha256_of() {
  file=$1

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | sed 's/[[:space:]].*//'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | sed 's/[[:space:]].*//'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$file" | sed 's/^.*= //'
  else
    return 1
  fi
}

verify_checksum() {
  archive=$1
  checksum_file=$2

  expected=$(sed 's/[[:space:]].*//' "$checksum_file" | sed -n '1p')
  [ -n "$expected" ] || die "checksum file is empty"

  actual=$(sha256_of "$archive" || true)
  if [ -z "$actual" ]; then
    warn "no sha256 tool found; skipping checksum verification"
    return
  fi

  [ "$actual" = "$expected" ] || die "checksum mismatch for downloaded archive"
  ok "verified sha256 checksum"
}

check_fuse_runtime() {
  if [ ! -e /dev/fuse ]; then
    warn "/dev/fuse is missing; the kernel FUSE module isn't loaded"
    hint "load it with: sudo modprobe fuse"
  fi

  if ! command -v fusermount3 >/dev/null 2>&1 \
     && ! command -v fusermount >/dev/null 2>&1; then
    warn "neither fusermount3 nor fusermount is on PATH"
    hint "install libfuse3 (Debian/Ubuntu: fuse3, Fedora: fuse3, Arch: fuse3, Alpine: fuse3)"
  fi
}

download_ghfs() {
  need_cmd tar
  make_tmp_dir

  if [ -n "$ARCHIVE_URL" ]; then
    url=$ARCHIVE_URL
    archive_name=ghfs.tar.gz
  else
    [ -n "$TARGET" ] || TARGET=$(detect_target)
    tag=$(resolve_tag)
    archive_name="ghfs-$tag-$TARGET.tar.gz"
    url="https://github.com/$REPO/releases/download/$tag/$archive_name"
  fi

  archive="$TMP_DIR/$archive_name"
  checksum_file="$archive.sha256"

  say "downloading $url"
  download_required "$url" "$archive"

  if download_file "$url.sha256" "$checksum_file"; then
    verify_checksum "$archive" "$checksum_file"
  else
    warn "checksum asset not found; skipping checksum verification"
  fi

  tar -xzf "$archive" -C "$TMP_DIR"

  for candidate in "$TMP_DIR/ghfs" "$TMP_DIR"/*/ghfs; do
    if [ -f "$candidate" ]; then
      GHFS_SRC=$candidate
      break
    fi
  done

  [ -n "$GHFS_SRC" ] || die "archive did not contain a ghfs binary"
}

detect_shell() {
  shell_name=$(basename "${SHELL:-}" 2>/dev/null || true)
  case "$shell_name" in
    zsh|bash|fish) printf '%s\n' "$shell_name" ;;
    *) printf '%s\n' zsh ;;
  esac
}

append_once() {
  rc_file=$1
  marker=$2
  line=$3
  label=$4
  rc_dir=$(dirname "$rc_file")

  [ -d "$rc_dir" ] || mkdir -p "$rc_dir"
  touch "$rc_file"

  if grep -qxF "$line" "$rc_file"; then
    say "$label already present in $rc_file"
  else
    {
      printf '\n%s\n' "$marker"
      printf '%s\n' "$line"
    } >> "$rc_file"
    ok "added $label to $rc_file"
  fi
}

bin_dir_in_rc() {
  rc_file=$1
  [ -f "$rc_file" ] || return 1

  grep -qF "$BIN_DIR" "$rc_file" && return 0

  if [ -n "${HOME:-}" ]; then
    case "$BIN_DIR" in
      "$HOME"|"$HOME/"*)
        rest=${BIN_DIR#$HOME}
        grep -qF "\$HOME$rest" "$rc_file" && return 0
        grep -qF "~$rest" "$rc_file" && return 0
        ;;
    esac
  fi

  return 1
}

append_path_once() {
  rc_file=$1
  marker=$2
  line=$3
  label=$4

  if bin_dir_in_rc "$rc_file"; then
    say "$label already references $BIN_DIR in $rc_file"
    return
  fi
  append_once "$rc_file" "$marker" "$line" "$label"
}

prompt_yes_no() {
  question=$1
  default=$2

  case "$default" in
    y) suffix=" [Y/n] " ;;
    *) suffix=" [y/N] " ;;
  esac

  answer=
  printf '%s%s==>%s %s%s' "$BOLD" "$CYAN" "$RESET" "$question" "$suffix" > /dev/tty
  if ! IFS= read -r answer < /dev/tty; then
    printf '\n' > /dev/tty
    return 1
  fi

  case "$answer" in
    [Yy]|[Yy][Ee][Ss]) return 0 ;;
    [Nn]|[Nn][Oo]) return 1 ;;
    '')
      case "$default" in y) return 0 ;; *) return 1 ;; esac
      ;;
    *) return 1 ;;
  esac
}

setup_shell_rc() {
  shell=$1
  [ -n "${HOME:-}" ] || die "PATH-setup needs HOME to be set"

  case "$shell" in
    zsh)
      append_path_once "$HOME/.zshrc" '# ghfs PATH' "export PATH=\"$BIN_DIR:\$PATH\"" "zsh PATH"
      hint "restart your shell or run: exec zsh"
      ;;
    bash)
      append_path_once "$HOME/.bashrc" '# ghfs PATH' "export PATH=\"$BIN_DIR:\$PATH\"" "bash PATH"
      hint "restart your shell or run: exec bash"
      ;;
    fish)
      append_path_once "$HOME/.config/fish/config.fish" '# ghfs PATH' "fish_add_path $BIN_DIR" "fish PATH"
      hint "restart your shell or run: exec fish"
      ;;
    *)
      die "unsupported shell: $shell"
      ;;
  esac
  SHELL_SETUP_DONE=1
}

xdg_data_home() {
  printf '%s\n' "${XDG_DATA_HOME:-$HOME/.local/share}"
}

xdg_config_home() {
  printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}"
}

# Generate a completion script via `ghfs completions <shell>` and write it
# atomically to the per-user location that the shell will auto-discover.
# Returns 0 on success (sets $target / $target_dir for callers), 1 otherwise.
install_completion_file() {
  shell=$1

  bin="$BIN_DIR/ghfs"
  if [ ! -x "$bin" ]; then
    warn "ghfs binary not executable at $bin; skipping $shell completion"
    return 1
  fi

  case "$shell" in
    bash)
      target="$(xdg_data_home)/bash-completion/completions/ghfs"
      ;;
    zsh)
      target="$(xdg_data_home)/zsh/site-functions/_ghfs"
      ;;
    fish)
      target="$(xdg_config_home)/fish/completions/ghfs.fish"
      ;;
    *)
      return 1
      ;;
  esac

  target_dir=$(dirname "$target")
  mkdir -p "$target_dir"

  if ! "$bin" completions "$shell" > "$target.tmp" 2>/dev/null; then
    rm -f "$target.tmp"
    warn "could not generate $shell completion (ghfs completions $shell failed)"
    return 1
  fi
  mv -f "$target.tmp" "$target"
  ok "installed $shell completion at $target"
  COMPLETIONS_INSTALLED="$COMPLETIONS_INSTALLED $shell"
  return 0
}

# Bash + fish auto-discover from XDG paths and need no rc edits. Zsh needs
# the completion dir on $fpath and a compinit call — wire those into the
# user's zshrc only when rc edits are allowed, otherwise print a hint.
auto_completion_setup() {
  if [ "$SKIP_COMPLETIONS" = 1 ]; then
    return
  fi
  [ -n "${HOME:-}" ] || return

  shell=$TARGET_SHELL
  [ -n "$shell" ] || shell=$(detect_shell)

  case "$shell" in
    bash|zsh|fish) ;;
    *) return ;;
  esac

  install_completion_file "$shell" || return

  if [ "$shell" = zsh ]; then
    if [ "$SKIP_RC" = 1 ]; then
      hint "add to ~/.zshrc to enable zsh completions:"
      hint "  fpath=(\"$target_dir\" \$fpath)"
      hint "  autoload -Uz compinit && compinit"
    else
      # compinit re-runs are cheap and idempotent under most frameworks
      # (oh-my-zsh, zinit, prezto already call it themselves).
      append_once "$HOME/.zshrc" '# ghfs completions' \
        "fpath=(\"$target_dir\" \$fpath)" "zsh completion fpath"
      append_once "$HOME/.zshrc" '# ghfs completions (compinit)' \
        "autoload -Uz compinit && compinit" "zsh compinit"
    fi
  fi
}

auto_shell_setup() {
  if [ "$SKIP_RC" = 1 ]; then
    return
  fi
  if [ "$EDIT_ZSHRC" = 1 ] || [ "$EDIT_BASHRC" = 1 ] || [ "$EDIT_FISHRC" = 1 ]; then
    return
  fi

  case ":$PATH:" in
    *":$BIN_DIR:"*) return ;;
  esac

  shell=$TARGET_SHELL
  [ -n "$shell" ] || shell=$(detect_shell)

  case "$shell" in
    zsh) rc_path="$HOME/.zshrc" ;;
    bash) rc_path="$HOME/.bashrc" ;;
    fish) rc_path="$HOME/.config/fish/config.fish" ;;
    *) return ;;
  esac

  [ -n "${HOME:-}" ] || return

  if [ "$ASSUME_YES" = 1 ]; then
    setup_shell_rc "$shell"
    return
  fi

  if [ ! -r /dev/tty ] || [ ! -w /dev/tty ]; then
    warn "$BIN_DIR is not on PATH and stdin isn't a tty; skipping rc edit (use --yes to auto-accept)"
    return
  fi

  if prompt_yes_no "add $BIN_DIR to PATH in $rc_path?" y; then
    setup_shell_rc "$shell"
  else
    say "skipping rc edit; either add $BIN_DIR to PATH yourself or invoke $BIN_DIR/ghfs by full path"
  fi
}

systemd_user_dir() {
  printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
}

systemd_user_unit_path() {
  printf '%s/%s\n' "$(systemd_user_dir)" "$SERVICE_UNIT_NAME"
}

write_service_unit() {
  unit_dir=$(systemd_user_dir)
  unit_path=$(systemd_user_unit_path)
  mkdir -p "$unit_dir"

  mount_path=$SERVICE_MOUNT_PATH
  [ -n "$mount_path" ] || mount_path="$HOME/ghfs"

  fusermount_bin=$(command -v fusermount3 2>/dev/null || command -v fusermount 2>/dev/null || printf '/usr/bin/fusermount3\n')
  mkdir_bin=$(command -v mkdir 2>/dev/null || printf '/bin/mkdir\n')

  env_file_line=
  if [ -n "$SERVICE_TOKEN_FILE" ]; then
    env_file_line="EnvironmentFile=$SERVICE_TOKEN_FILE"
  fi

  {
    printf '[Unit]\n'
    printf 'Description=Mount GitHub repositories as a FUSE filesystem (ghfs)\n'
    printf 'After=network-online.target\n'
    printf 'Wants=network-online.target\n'
    printf '\n'
    printf '[Service]\n'
    printf 'Type=simple\n'
    printf 'ExecStartPre=%s -p "%s"\n' "$mkdir_bin" "$mount_path"
    printf 'ExecStart=%s/ghfs mount "%s"\n' "$BIN_DIR" "$mount_path"
    printf 'ExecStop=%s -u "%s"\n' "$fusermount_bin" "$mount_path"
    printf 'KillSignal=SIGINT\n'
    printf 'TimeoutStopSec=20\n'
    printf 'Restart=on-failure\n'
    printf 'RestartSec=5\n'
    if [ -n "$env_file_line" ]; then
      printf '%s\n' "$env_file_line"
    fi
    printf '\n'
    printf '[Install]\n'
    printf 'WantedBy=default.target\n'
  } > "$unit_path"

  chmod 0644 "$unit_path"
  ok "wrote systemd unit $unit_path"
  hint "mount path: $mount_path"
  if [ -z "$env_file_line" ]; then
    hint "the unit inherits the user-manager environment; set GHFS_TOKEN there or rerun with --service-token-file"
  else
    hint "EnvironmentFile=$SERVICE_TOKEN_FILE — file must define GHFS_TOKEN=... (mode 0600)"
  fi
}

enable_service_unit() {
  if ! command -v systemctl >/dev/null 2>&1; then
    warn "systemctl not on PATH; skipping enable. Run later: systemctl --user enable --now $SERVICE_UNIT_NAME"
    return
  fi

  systemctl --user daemon-reload || warn "systemctl --user daemon-reload failed"
  if systemctl --user enable --now "$SERVICE_UNIT_NAME" >/dev/null 2>&1; then
    ok "enabled and started $SERVICE_UNIT_NAME"
    hint "check: systemctl --user status $SERVICE_UNIT_NAME"
  else
    warn "could not enable $SERVICE_UNIT_NAME (no user session?). Try later: systemctl --user enable --now $SERVICE_UNIT_NAME"
  fi
}

install_service() {
  [ -n "${HOME:-}" ] || die "systemd-unit setup needs HOME to be set"

  if [ ! -d /run/systemd/system ]; then
    warn "no /run/systemd/system found; writing the unit anyway in case you move it"
  fi

  write_service_unit
  if [ "$ENABLE_SERVICE" = 1 ]; then
    enable_service_unit
  else
    say "enable on next login or run:"
    hint "systemctl --user daemon-reload && systemctl --user enable --now $SERVICE_UNIT_NAME"
  fi
}

uninstall_service() {
  unit_path=$(systemd_user_unit_path)
  if [ ! -e "$unit_path" ]; then
    return
  fi

  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable --now "$SERVICE_UNIT_NAME" >/dev/null 2>&1 || true
  fi

  rm -f "$unit_path"
  ok "removed $unit_path"

  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload >/dev/null 2>&1 || true
  fi
}

auto_service_setup() {
  if [ "$SKIP_SERVICE" = 1 ]; then
    return
  fi
  if [ "$INSTALL_SERVICE" = 1 ]; then
    install_service
    return
  fi

  case "$(uname -s 2>/dev/null || true)" in
    Linux) ;;
    *) return ;;
  esac
  [ -d /run/systemd/system ] || return
  command -v systemctl >/dev/null 2>&1 || return
  [ -n "${HOME:-}" ] || return

  if [ "$ASSUME_YES" = 1 ]; then
    install_service
    return
  fi

  if [ ! -r /dev/tty ] || [ ! -w /dev/tty ]; then
    return
  fi

  prompt_path=$SERVICE_MOUNT_PATH
  [ -n "$prompt_path" ] || prompt_path="$HOME/ghfs"

  if prompt_yes_no "install a systemd user service to auto-mount at $prompt_path on login?" n; then
    install_service
  else
    say "skipping systemd unit; install later with: sh install.sh --service"
  fi
}

install_ghfs() {
  dest="$BIN_DIR/ghfs"
  tmp="$BIN_DIR/.ghfs.$$"

  need_cmd install
  install -d -m 0755 "$BIN_DIR"
  install -m 0755 "$GHFS_SRC" "$tmp"
  mv -f "$tmp" "$dest"

  version=$("$GHFS_SRC" --version 2>/dev/null || printf 'ghfs unknown')
  ok "installed $version to $dest"

  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR is not on PATH" ;;
  esac
}

uninstall_ghfs() {
  dest="$BIN_DIR/ghfs"
  if [ ! -e "$dest" ]; then
    say "no ghfs binary at $dest"
    return
  fi

  rm -f "$dest"
  ok "removed $dest"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      shift
      [ "$#" -gt 0 ] || die "--version needs a value"
      VERSION="$1"
      ;;
    --version=*)
      VERSION="${1#--version=}"
      ;;
    --target)
      shift
      [ "$#" -gt 0 ] || die "--target needs a target triple"
      TARGET="$1"
      ;;
    --target=*)
      TARGET="${1#--target=}"
      ;;
    --repo)
      shift
      [ "$#" -gt 0 ] || die "--repo needs OWNER/REPO"
      REPO="$1"
      ;;
    --repo=*)
      REPO="${1#--repo=}"
      ;;
    --archive-url)
      shift
      [ "$#" -gt 0 ] || die "--archive-url needs a URL"
      ARCHIVE_URL="$1"
      ;;
    --archive-url=*)
      ARCHIVE_URL="${1#--archive-url=}"
      ;;
    --prefix)
      shift
      [ "$#" -gt 0 ] || die "--prefix needs a directory"
      PREFIX="$1"
      ;;
    --prefix=*)
      PREFIX="${1#--prefix=}"
      ;;
    --bin-dir)
      shift
      [ "$#" -gt 0 ] || die "--bin-dir needs a directory"
      BIN_DIR="$1"
      ;;
    --bin-dir=*)
      BIN_DIR="${1#--bin-dir=}"
      ;;
    --user)
      [ -n "${HOME:-}" ] || die "--user needs HOME to be set"
      PREFIX="$HOME/.local"
      ;;
    --shell)
      shift
      [ "$#" -gt 0 ] || die "--shell needs zsh, bash, or fish"
      TARGET_SHELL="$1"
      ;;
    --shell=*)
      TARGET_SHELL="${1#--shell=}"
      ;;
    --zshrc)
      EDIT_ZSHRC=1
      ;;
    --bashrc)
      EDIT_BASHRC=1
      ;;
    --fishrc)
      EDIT_FISHRC=1
      ;;
    -y|--yes)
      ASSUME_YES=1
      ;;
    --no-modify-rc)
      SKIP_RC=1
      ;;
    --no-completions)
      SKIP_COMPLETIONS=1
      ;;
    --service)
      INSTALL_SERVICE=1
      ;;
    --no-service)
      SKIP_SERVICE=1
      ;;
    --service-enable)
      INSTALL_SERVICE=1
      ENABLE_SERVICE=1
      ;;
    --service-mount-path)
      shift
      [ "$#" -gt 0 ] || die "--service-mount-path needs a directory"
      SERVICE_MOUNT_PATH="$1"
      ;;
    --service-mount-path=*)
      SERVICE_MOUNT_PATH="${1#--service-mount-path=}"
      ;;
    --service-token-file)
      shift
      [ "$#" -gt 0 ] || die "--service-token-file needs a file path"
      SERVICE_TOKEN_FILE="$1"
      ;;
    --service-token-file=*)
      SERVICE_TOKEN_FILE="${1#--service-token-file=}"
      ;;
    --uninstall)
      DO_UNINSTALL=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
  shift
done

if [ -z "$BIN_DIR" ]; then
  if [ -z "$PREFIX" ]; then
    [ -n "${HOME:-}" ] || die "HOME is not set; use --prefix or --bin-dir"
    PREFIX="$HOME/.local"
  fi
  BIN_DIR="$PREFIX/bin"
fi

case "$TARGET_SHELL" in
  ''|zsh|bash|fish) ;;
  *) die "unsupported shell: $TARGET_SHELL" ;;
esac

if [ "$(id -u)" -eq 0 ]; then
  die "do not run this installer as root; run it as your normal user"
fi

if [ "$DO_UNINSTALL" = 1 ]; then
  uninstall_service
  uninstall_ghfs
  exit 0
fi

download_ghfs
install_ghfs
check_fuse_runtime

if [ "$EDIT_ZSHRC" = 1 ]; then setup_shell_rc zsh; fi
if [ "$EDIT_BASHRC" = 1 ]; then setup_shell_rc bash; fi
if [ "$EDIT_FISHRC" = 1 ]; then setup_shell_rc fish; fi

auto_shell_setup
auto_completion_setup
auto_service_setup

if [ "$SHELL_SETUP_DONE" != 1 ]; then
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
      say "next: add $BIN_DIR to your PATH, then run:"
      hint "ghfs --help"
      ;;
  esac
fi
