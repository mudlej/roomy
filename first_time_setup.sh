#!/usr/bin/env bash
# One-time bootstrap for roomy. Run from Bash, ideally inside the distrobox
# (see distrobox.ini). Installs checksum-verified Just when it is missing or
# too old, checks what building and trying roomy needs, then runs
# `just setup`. Safe to re-run.
set -euo pipefail

JUST_VERSION="1.57.0"
JUST_ARCHIVE="just-$JUST_VERSION-x86_64-unknown-linux-musl.tar.gz"
JUST_SHA256="45b548094283cb9739af8f13273b8cddeee869f5b4ef2bb631b1f311cb566155"
JUST_URL="https://github.com/casey/just/releases/download/$JUST_VERSION/$JUST_ARCHIVE"
RUST_VERSION="1.92.0"
GTK_VERSION="4.14"
ADWAITA_VERSION="1.5"
ROOT_DIR="$(cd -P -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${ROOMY_BIN_DIR:-${HOME:-}/.local/bin}"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

has_command() {
  command -v "$1" >/dev/null 2>&1
}

# version_at_least <have> <want>
version_at_least() {
  [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" == "$2" ]]
}

just_is_recent() {
  local have
  have="$(just --version 2>/dev/null)" && version_at_least "${have#just }" "$JUST_VERSION"
}

install_just() {
  TEMP_DIR="$(mktemp -d)"
  trap 'rm -rf -- "$TEMP_DIR"' EXIT

  echo "Installing Just $JUST_VERSION into $BIN_DIR."
  has_command curl || fail "curl is needed to download Just."
  curl -fsSL --retry 3 --connect-timeout 10 --max-time 600 -o "$TEMP_DIR/$JUST_ARCHIVE" "$JUST_URL" \
    || fail "Could not download $JUST_URL."
  echo "$JUST_SHA256  $TEMP_DIR/$JUST_ARCHIVE" | sha256sum -c --quiet \
    || fail "Checksum verification failed for $JUST_ARCHIVE."
  mkdir -p "$TEMP_DIR/unpacked" "$BIN_DIR"
  tar -xzf "$TEMP_DIR/$JUST_ARCHIVE" -C "$TEMP_DIR/unpacked"
  [[ -f "$TEMP_DIR/unpacked/just" ]] || fail "The Just archive did not contain the expected binary."
  install -m755 "$TEMP_DIR/unpacked/just" "$BIN_DIR/just"
  hash -r
}

# Only needed when Just went into a directory the shell does not search yet.
add_bin_dir_to_profile() {
  local profile="$HOME/.bashrc"
  local entry="export PATH=\"$BIN_DIR:\$PATH\""
  [[ "${SHELL:-}" == */zsh ]] && profile="$HOME/.zshrc"
  if [[ -f "$profile" ]] && grep -qxF "$entry" "$profile"; then
    return
  fi
  printf '\n%s\n' "$entry" >>"$profile" || fail "Cannot add $BIN_DIR to the PATH in $profile."
  echo "Added $BIN_DIR to the PATH in $profile (open a new shell, or run: $entry)."
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
    echo "WARNING: missing for some recipes: ${missing[*]}"
    echo "         The distrobox (distrobox.ini) provides all of them."
    echo "         Without the window toolkit, 'just check-headless' still works."
  fi
  [[ -e /dev/kvm ]] || echo "NOTE: /dev/kvm is not visible here; the lab recipes and 'just run' need it."
}

main() {
  local bin_dir_on_path="true"

  [[ -n "${HOME:-}" ]] || fail "HOME is not set."
  [[ "$(uname -s)" == "Linux" ]] || fail "roomy runs on Linux only (it needs KVM)."
  [[ "$(uname -m)" == "x86_64" ]] || fail "Unsupported architecture: $(uname -m). roomy runs x86-64 guests on x86-64 computers."

  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) bin_dir_on_path="false" ;;
  esac
  export PATH="$BIN_DIR:$PATH"

  if just_is_recent; then
    echo "$(just --version) is already installed (>= $JUST_VERSION)."
  else
    install_just
    [[ "$bin_dir_on_path" == "true" ]] || add_bin_dir_to_profile
  fi

  check_prerequisites
  if has_command cargo; then
    echo "Running project setup."
    (cd "$ROOT_DIR" && just setup)
    echo "roomy is ready. Start with: just run   (or: just --list)"
  else
    echo "Rust is missing, so the project was not set up. Enter the distrobox and run this again."
  fi
}

main "$@"
