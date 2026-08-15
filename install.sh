#!/bin/sh
set -eu

# packctl installer — downloads a prebuilt release binary from GitHub.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh -s v0.1.0
#   curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh -s rolling
#   curl -fsSL ... | sh -s rolling /usr/local/bin
#
# Overridable via environment (or positionally, via `sh -s`):
#   VERSION       release tag to install (default: latest; "rolling" installs
#                 the latest continuous build from main)
#   INSTALL_DIR   directory to install into (default: /usr/local/bin as root,
#                 else $HOME/.local/bin)

REPO="${PACKCTL_REPO:-sennecools/packctl}"
VERSION="${1:-${VERSION:-latest}}"
shift 2>/dev/null || true
INSTALL_DIR="${1:-${INSTALL_DIR:-}}"

say() { printf 'packctl: %s\n' "$*"; }
die() { say "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# --- OS / architecture detection ---
os="$(uname -s)"
case "$os" in
  Linux) ;;
  *) die "unsupported OS '$os' (packctl targets Linux only)" ;;
esac

arch="$(uname -m)"
case "$arch" in
  x86_64 | amd64)  target="x86_64-unknown-linux-musl" ;;
  aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
  *) die "unsupported architecture '$arch'" ;;
esac

# --- install directory ---
if [ -z "$INSTALL_DIR" ]; then
  if [ "$(id -u)" -eq 0 ]; then
    INSTALL_DIR="/usr/local/bin"
  else
    INSTALL_DIR="$HOME/.local/bin"
  fi
fi

# --- resolve version ---
if [ "$VERSION" = "latest" ]; then
  have curl || die "curl is required for installation"
  say "resolving latest release..."
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$VERSION" ] || die "could not determine the latest release"
fi

# --- download ---
have curl || die "curl is required for installation"
archive="packctl-$target.tar.gz"
base_url="https://github.com/$REPO/releases/download/$VERSION"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

say "downloading $VERSION ($target)..."
curl -fsSL "$base_url/$archive" -o "$tmpdir/$archive"

# --- verify checksum ---
have sha256sum || die "sha256sum is required to verify the download"
curl -fsSL "$base_url/SHA256SUMS" -o "$tmpdir/SHA256SUMS" \
  || die "could not download release checksums"
expected="$(sed -n "s/^\\([0-9a-f]\\{64\\}\\)  $archive$/\\1/p" "$tmpdir/SHA256SUMS")"
[ -n "$expected" ] && [ "$(printf '%s\\n' "$expected" | wc -l)" -eq 1 ] \
  || die "no exact SHA-256 checksum found for $archive"
actual="$(sha256sum "$tmpdir/$archive")"
actual="${actual%% *}"
[ "$actual" = "$expected" ] || die "checksum verification failed for $archive"
say "checksum verified"

tar -xzf "$tmpdir/$archive" -C "$tmpdir"

# --- install ---
mkdir -p "$INSTALL_DIR" 2>/dev/null || true
if [ -w "$INSTALL_DIR" ]; then
  mv -f "$tmpdir/packctl-$target" "$INSTALL_DIR/packctl"
else
  if [ "$(id -u)" -ne 0 ] && have sudo; then
    sudo mkdir -p "$INSTALL_DIR"
    sudo mv -f "$tmpdir/packctl-$target" "$INSTALL_DIR/packctl"
  else
    die "cannot write to '$INSTALL_DIR'; set INSTALL_DIR to a writable directory"
  fi
fi
chmod 0755 "$INSTALL_DIR/packctl"

say "installed $VERSION to $INSTALL_DIR/packctl"
"$INSTALL_DIR/packctl" --version
say "next: run 'packctl create' to set up a server profile"
