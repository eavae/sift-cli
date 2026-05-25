#!/usr/bin/env bash
#
# sift installer — downloads a prebuilt binary from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/eavae/sift-cli/main/scripts/install.sh | bash
#
# Env overrides:
#   SIFT_VERSION       pin a specific tag (e.g. v0.1.0); default = latest
#   SIFT_INSTALL_DIR   install location;                 default = $HOME/.local/bin
#   SIFT_REPO          GitHub repo (owner/name);         default = eavae/sift-cli

set -euo pipefail

REPO="${SIFT_REPO:-eavae/sift-cli}"
INSTALL_DIR="${SIFT_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${SIFT_VERSION:-}"

log()  { printf '==> %s\n' "$*"; }
warn() { printf 'WARN: %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# ---- detect target ----------------------------------------------------------
uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "$uname_s" in
  Linux)  os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *)      die "Unsupported OS: $uname_s (try building from source: see README)" ;;
esac

case "$uname_m" in
  x86_64|amd64) arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *) die "Unsupported architecture: $uname_m" ;;
esac

target="${arch}-${os}"
log "Detected target: $target"

# ---- resolve version --------------------------------------------------------
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }
need curl
need tar

if [ -z "$VERSION" ]; then
  log "Resolving latest release from $REPO ..."
  # Follow the redirect from /releases/latest to grab the tag without jq.
  VERSION="$(
    curl -fsSLI -o /dev/null -w '%{url_effective}' \
      "https://github.com/${REPO}/releases/latest" \
      | sed 's#.*/tag/##'
  )"
  [ -n "$VERSION" ] || die "could not resolve latest version (set SIFT_VERSION manually)"
fi
log "Version: $VERSION"

# ---- download + verify ------------------------------------------------------
archive="sift-${VERSION}-${target}.tar.gz"
url="https://github.com/${REPO}/releases/download/${VERSION}/${archive}"
sum_url="${url}.sha256"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

log "Downloading $url"
curl -fsSL "$url"     -o "$tmpdir/$archive"

log "Downloading checksum"
if curl -fsSL "$sum_url" -o "$tmpdir/$archive.sha256"; then
  pushd "$tmpdir" >/dev/null
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$archive.sha256" || die "checksum mismatch"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$archive.sha256" || die "checksum mismatch"
  else
    warn "no sha256 verifier found (sha256sum/shasum); skipping verification"
  fi
  popd >/dev/null
else
  warn "no checksum file at $sum_url; skipping verification"
fi

# ---- extract + install ------------------------------------------------------
log "Extracting"
tar -C "$tmpdir" -xzf "$tmpdir/$archive"

extracted_bin="$tmpdir/sift-${VERSION}-${target}/sift"
[ -f "$extracted_bin" ] || die "archive layout unexpected: missing $extracted_bin"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$extracted_bin" "$INSTALL_DIR/sift"

log "Installed: $INSTALL_DIR/sift"

# ---- PATH hint --------------------------------------------------------------
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    cat <<EOF

NOTE: $INSTALL_DIR is not on your PATH. Add it to your shell profile:

    export PATH="\$HOME/.local/bin:\$PATH"

EOF
    ;;
esac

"$INSTALL_DIR/sift" --version 2>/dev/null || true
log "Done. Try:  sift --help"
