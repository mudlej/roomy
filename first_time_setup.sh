#!/usr/bin/env bash
# One-time bootstrap for roomy. Run from Bash, ideally inside the distrobox
# (see distrobox.ini). Installs checksum-verified Just when it is missing or
# too old, checks what building and trying roomy needs, then runs
# `just setup`. Safe to re-run.
set -euo pipefail

JUST_VERSION="1.57.0"
RUST_VERSION="1.92.0"
GTK_VERSION="4.14"
ADWAITA_VERSION="1.5"
ROOT_DIR="$(cd -P -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${ROOMY_BIN_DIR:-${HOME:-}/.local/bin}"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

info() {
  printf '%s\n' "$*"
}

# version_at_least <have> <want>
version_at_least() {
  [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" == "$2" ]]
}

has_command() {
  command -v "$1" >/dev/null 2>&1
}

require_command() {
  has_command "$1" || fail "Required command '$1' is missing."
}

path_contains() {
  case ":$PATH:" in
    *":$1:"*) return 0 ;;
    *) return 1 ;;
  esac
}

ensure_writable_directory() {
  local directory="$1"
  local probe
  mkdir -p "$directory" || fail "Cannot create $directory."
  probe="$directory/.roomy-write-test-$$"
  (umask 077 && : >"$probe") || fail "$directory is not writable."
  rm -f "$probe"
}

detect_environment() {
  local key
  local value

  [[ "$(uname -s)" == "Linux" ]] || fail "roomy runs on Linux only (it needs KVM)."
  MACHINE="$(uname -m)"
  DISTRO_ID="linux"
  HOST_KIND="host"
  if [[ -r /etc/os-release ]]; then
    while IFS='=' read -r key value; do
      value="${value#\"}"
      value="${value%\"}"
      case "$key" in
        ID) DISTRO_ID="${value:-linux}" ;;
      esac
    done </etc/os-release
  fi
  if [[ -f /run/.containerenv || -f /.dockerenv ]]; then
    HOST_KIND="container"
  fi
}

select_assets() {
  local just_target=""

  case "$MACHINE" in
    x86_64|amd64)
      just_target="x86_64-unknown-linux-musl"
      JUST_SHA256="45b548094283cb9739af8f13273b8cddeee869f5b4ef2bb631b1f311cb566155"
      ;;
    *)
      fail "Unsupported architecture: $MACHINE. roomy runs x86-64 guests on x86-64 computers."
      ;;
  esac

  JUST_ARCHIVE="just-$JUST_VERSION-$just_target.tar.gz"
  JUST_URL="https://github.com/casey/just/releases/download/$JUST_VERSION/$JUST_ARCHIVE"
}

select_downloader() {
  if has_command curl; then
    DOWNLOADER="curl"
  elif has_command wget; then
    DOWNLOADER="wget"
  else
    fail "Either curl or wget is required to download tools."
  fi
}

download_to() {
  local url="$1"
  local destination="$2"
  if [[ "$DOWNLOADER" == "curl" ]]; then
    curl -fsSL --retry 3 --connect-timeout 10 --max-time 600 -o "$destination" "$url"
  else
    wget --timeout=30 --tries=3 -q -O "$destination" "$url"
  fi
}

sha256_file() {
  local output
  if has_command sha256sum; then
    output="$(sha256sum "$1")"
    printf '%s\n' "${output%% *}"
  elif has_command shasum; then
    output="$(shasum -a 256 "$1")"
    printf '%s\n' "${output%% *}"
  elif has_command openssl; then
    output="$(openssl dgst -sha256 "$1")"
    printf '%s\n' "${output##* }"
  else
    fail "A SHA-256 tool is required: sha256sum, shasum, or openssl."
  fi
}

verify_download() {
  local file="$1"
  local expected="$2"
  local actual
  actual="$(sha256_file "$file")"
  [[ "$actual" == "$expected" ]] || fail "Checksum verification failed for $(basename "$file")."
}

install_just() {
  local archive="$TEMP_DIR/$JUST_ARCHIVE"
  local extracted="$TEMP_DIR/just"
  local destination="$BIN_DIR/just"
  local have

  have="$(just --version 2>/dev/null || true)"
  if has_command just && version_at_least "${have#just }" "$JUST_VERSION"; then
    info "Just ${have#just } is already installed (>= $JUST_VERSION)."
    return
  fi

  info "Installing Just $JUST_VERSION into $BIN_DIR."
  select_downloader
  require_command tar
  ensure_writable_directory "$BIN_DIR"
  download_to "$JUST_URL" "$archive" || fail "Could not download $JUST_URL."
  verify_download "$archive" "$JUST_SHA256"
  mkdir -p "$extracted"
  tar -xzf "$archive" -C "$extracted"
  [[ -f "$extracted/just" ]] || fail "The Just archive did not contain the expected binary."
  cp "$extracted/just" "$destination.tmp"
  chmod +x "$destination.tmp"
  mv "$destination.tmp" "$destination"
  JUST_WAS_INSTALLED="true"
  hash -r
}

# Only when Just was put into a directory the shell does not search yet.
persist_shell_environment() {
  local profile
  local entry="export PATH=\"$BIN_DIR:\$PATH\""
  local line

  [[ "$JUST_WAS_INSTALLED" == "true" && "$BIN_PATH_NEEDS_PERSISTENCE" == "true" ]] || return 0
  case "${SHELL:-}" in
    */zsh) profile="$HOME/.zshrc" ;;
    *) profile="$HOME/.bashrc" ;;
  esac
  if [[ -f "$profile" ]]; then
    while IFS= read -r line; do
      [[ "$line" == "$entry" ]] && return 0
    done <"$profile"
  fi
  printf '\n%s\n' "$entry" >>"$profile" || fail "Cannot add $BIN_DIR to the PATH in $profile."
  info "Added $BIN_DIR to the PATH in $profile (open a new shell, or run: $entry)."
}

# Warn (don't fail) about what the recipes need; the distrobox has all of it.
check_prerequisites() {
  local missing=()
  local have
  local tool

  if has_command cargo && has_command rustc; then
    have="$(rustc --version | sed 's/^rustc \([^ ]*\).*/\1/')"
    version_at_least "$have" "$RUST_VERSION" || missing+=("rust>=$RUST_VERSION (found $have)")
  else
    missing+=("rust>=$RUST_VERSION (cargo, rustc)")
  fi
  for tool in cc pkg-config python3 curl; do
    has_command "$tool" || missing+=("$tool")
  done
  if has_command pkg-config; then
    pkg-config --atleast-version="$GTK_VERSION" gtk4 2>/dev/null || missing+=("gtk4-dev>=$GTK_VERSION")
    pkg-config --atleast-version="$ADWAITA_VERSION" libadwaita-1 2>/dev/null || missing+=("libadwaita-dev>=$ADWAITA_VERSION")
  fi
  for tool in qemu-system-x86_64 qemu-img ssh xorriso; do
    has_command "$tool" || missing+=("$tool (lab recipes)")
  done
  if [[ ${#missing[@]} -gt 0 ]]; then
    info "WARNING: missing for some recipes: ${missing[*]}"
    info "         The distrobox (distrobox.ini) provides all of them."
    info "         Without the window toolkit, 'just check-headless' still works."
  fi
  [[ -e /dev/kvm ]] || info "NOTE: /dev/kvm is not visible here; the lab recipes and 'just run' need it."
}

main() {
  [[ -n "${HOME:-}" ]] || fail "HOME is not set."
  require_command uname
  require_command mkdir
  require_command mktemp
  require_command cp
  require_command chmod
  require_command mv
  require_command rm
  require_command sort

  detect_environment
  select_assets
  if ! has_command sha256sum && ! has_command shasum && ! has_command openssl; then
    fail "A SHA-256 tool is required: sha256sum, shasum, or openssl."
  fi

  if path_contains "$BIN_DIR"; then
    BIN_PATH_NEEDS_PERSISTENCE="false"
  else
    BIN_PATH_NEEDS_PERSISTENCE="true"
  fi
  JUST_WAS_INSTALLED="false"
  export PATH="$BIN_DIR:$PATH"

  TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/roomy-setup.XXXXXX")" || fail "Cannot create a temporary directory."
  trap 'rm -rf "$TEMP_DIR"' EXIT

  info "Environment: $DISTRO_ID, $MACHINE, $HOST_KIND."
  install_just
  persist_shell_environment
  version_at_least "$(just --version | sed 's/^just //')" "$JUST_VERSION" || fail "Just version verification failed."

  check_prerequisites
  if has_command cargo; then
    info "Running project setup."
    (cd "$ROOT_DIR" && just setup)
    info "roomy is ready. Start with: just run   (or: just --list)"
  else
    info "Rust is missing, so the project was not set up. Enter the distrobox and run this again."
  fi
}

main "$@"
