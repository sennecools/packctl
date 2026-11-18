#!/bin/sh
set -eu

# packctl installer: downloads a prebuilt release binary from GitHub,
# verifies its SHA-256 checksum and installs it.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh -s v0.2.0
#   curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh -s rolling
#   curl -fsSL ... | sh -s rolling /usr/local/bin
#
# Overridable via environment (or positionally, via `sh -s`):
#   VERSION       release tag to install (default: latest; "rolling" installs
#                 the latest continuous build from main)
#   INSTALL_DIR   directory to install into (default: /usr/local/bin as root,
#                 else $HOME/.local/bin)
#
# Optional:
#   PACKCTL_VERIFY_ATTESTATION=1  also verify the GitHub build provenance
#                                 attestation with `gh attestation verify`
#                                 (requires the GitHub CLI)
#   PACKCTL_DOWNLOAD_BASE         download from this https:// or file:// base
#                                 instead of the GitHub release (used by CI to
#                                 test the installer against a fresh build)

REPO="${PACKCTL_REPO:-sennecools/packctl}"
VERSION="${1:-${VERSION:-latest}}"
INSTALL_DIR="${2:-${INSTALL_DIR:-}}"

say() { printf 'packctl: %s\n' "$*"; }
warn() { say "warning: $*" >&2; }
die() { say "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# --- required tools ---
have curl || die "curl is required for installation"
have tar || die "tar is required for installation"
if have sha256sum; then
  sha256() { sha256sum "$1"; }
elif have shasum; then
  sha256() { shasum -a 256 "$1"; }
else
  die "sha256sum or shasum is required to verify the download"
fi

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
  say "resolving latest release..."
  # github.com redirects /releases/latest to /releases/tag/<tag>; following
  # the redirect avoids the rate-limited REST API.
  VERSION="$(curl -fsSLI --proto '=https' --proto-redir '=https' -o /dev/null -w '%{url_effective}' \
    "https://github.com/$REPO/releases/latest" | sed 's#.*/tag/##')"
  case "$VERSION" in
    "" | */*) die "could not determine the latest release" ;;
  esac
fi

# --- download ---
archive="packctl-$target.tar.gz"
member="packctl-$target"
base_url="${PACKCTL_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download/$VERSION}"
case "$base_url" in
  https://* | file://*) ;;
  *) die "refusing to download from '$base_url' (https:// or file:// only)" ;;
esac

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

# The rolling release replaces its files in place on every main commit; an
# install that races that update fails closed, and trying again fixes it.
retry=""
[ "$VERSION" = "rolling" ] && retry=" (rolling may be mid-update; retry in a minute)"

say "downloading $VERSION ($target)..."
curl -fsSL --proto '=https,file' --proto-redir '=https' "$base_url/$archive" -o "$tmpdir/$archive" \
  || die "could not download $archive for $VERSION$retry"

# --- verify checksum (fail closed) ---
curl -fsSL --proto '=https,file' --proto-redir '=https' "$base_url/SHA256SUMS" -o "$tmpdir/SHA256SUMS" \
  || die "could not download release checksums$retry"
# "<hash>  <file>" (text mode) or "<hash> *<file>" (binary mode).
expected="$(sed -n "s/^\\([0-9a-f]\\{64\\}\\) [ *]$archive$/\\1/p" "$tmpdir/SHA256SUMS")"
if [ -z "$expected" ] || [ "$(printf '%s\n' "$expected" | wc -l)" -ne 1 ]; then
  die "no exact SHA-256 checksum found for $archive"
fi
actual="$(sha256 "$tmpdir/$archive")"
actual="${actual%% *}"
[ "$actual" = "$expected" ] || die "checksum verification failed for $archive$retry"
say "checksum verified"

# --- verify build provenance (opt-in) ---
if [ "${PACKCTL_VERIFY_ATTESTATION:-0}" = "1" ]; then
  have gh || die "PACKCTL_VERIFY_ATTESTATION=1 needs the GitHub CLI (gh)"
  gh attestation verify "$tmpdir/$archive" -R "$REPO" \
    || die "attestation verification failed for $archive"
  say "attestation verified"
fi

# --- extract only the binary ---
mkdir "$tmpdir/out"
tar -xzf "$tmpdir/$archive" -C "$tmpdir/out" "$member" 2>/dev/null \
  || tar -xzf "$tmpdir/$archive" -C "$tmpdir/out" "./$member" \
  || die "$archive does not contain $member"
binary="$tmpdir/out/$member"
[ -f "$binary" ] || die "$archive does not contain $member"

# --- install ---
# `install -m 0755` copies with the right mode, owned by whoever runs it, so a
# root-owned directory never gets a user-owned binary.
if mkdir -p "$INSTALL_DIR" 2>/dev/null && [ -w "$INSTALL_DIR" ]; then
  install -m 0755 "$binary" "$INSTALL_DIR/packctl"
elif [ "$(id -u)" -ne 0 ] && have sudo; then
  say "$INSTALL_DIR is not writable; using sudo"
  sudo mkdir -p "$INSTALL_DIR"
  sudo install -m 0755 "$binary" "$INSTALL_DIR/packctl"
else
  die "cannot write to '$INSTALL_DIR'; set INSTALL_DIR to a writable directory"
fi

say "installed $VERSION to $INSTALL_DIR/packctl"
case ":${PATH:-}:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "$INSTALL_DIR is not on your PATH; add $INSTALL_DIR to your PATH" ;;
esac
"$INSTALL_DIR/packctl" --version
say "next: run 'packctl create' to set up a server profile"
