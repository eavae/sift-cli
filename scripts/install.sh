#!/usr/bin/env bash
#
# sift installer — downloads a prebuilt binary from GitHub Releases.
#
# Works on Linux, macOS, and Windows (run it under Git Bash / MSYS2).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/eavae/sift-cli/main/scripts/install.sh | bash
#
# Env overrides:
#   SIFT_VERSION       pin a specific tag (e.g. v0.2.0);  default = latest
#   SIFT_INSTALL_DIR   install location;                  default = first writable
#                      directory on PATH (falling back to $HOME/.local/bin)
#   SIFT_REPO          GitHub repo (owner/name);          default = eavae/sift-cli
#   SIFT_MIRROR        GitHub mirror for regions where github.com is blocked:
#                        auto (default) → use GitHub directly if reachable,
#                                         else fall back to a public gh-proxy
#                        off            → force GitHub directly
#                        <url>          → force this mirror prefix
#                                         (e.g. https://cdn.gh-proxy.org)

set -euo pipefail

REPO="${SIFT_REPO:-eavae/sift-cli}"
VERSION="${SIFT_VERSION:-}"

log()  { printf '==> %s\n' "$*"; }
warn() { printf 'WARN: %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# ---- detect target ----------------------------------------------------------
uname_s="$(uname -s)"
uname_m="$(uname -m)"

# Windows ships as a `.zip` holding `sift.exe`; every other target is an
# extensionless binary in a `.tar.gz`.
archive_ext="tar.gz"
bin_name="sift"
case "$uname_s" in
  Linux)                os="unknown-linux-gnu" ;;
  Darwin)               os="apple-darwin" ;;
  MINGW*|MSYS*|CYGWIN*) os="pc-windows-msvc"; archive_ext="zip"; bin_name="sift.exe" ;;
  *)                    die "Unsupported OS: $uname_s (try building from source: see README)" ;;
esac

case "$uname_m" in
  x86_64|amd64)  arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)             die "Unsupported architecture: $uname_m" ;;
esac

# Windows is published for x86_64 only (see the release matrix).
if [ "$os" = "pc-windows-msvc" ] && [ "$arch" != "x86_64" ]; then
  die "No Windows build for $arch (only x86_64-pc-windows-msvc is published)"
fi

target="${arch}-${os}"
log "Detected target: $target"

# ---- pick install dir: first writable directory on PATH ---------------------
# Installing into a directory already on PATH means `sift` runs immediately,
# with no profile edit. `SIFT_INSTALL_DIR` overrides the search entirely.
resolve_install_dir() {
  if [ -n "${SIFT_INSTALL_DIR:-}" ]; then
    printf '%s' "$SIFT_INSTALL_DIR"
    return
  fi
  local IFS=: d
  for d in $PATH; do
    # Absolute paths only — never install into a relative PATH entry (`.`).
    case "$d" in
      /*) if [ -d "$d" ] && [ -w "$d" ]; then printf '%s' "$d"; return; fi ;;
    esac
  done
  # Nothing writable on PATH → user-local fallback (created below).
  printf '%s' "$HOME/.local/bin"
}
INSTALL_DIR="$(resolve_install_dir)"

# ---- pick GitHub mirror (fallback for blocked regions, e.g. mainland China) -
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }
need curl

# MIRROR is a prefix prepended to every github.com / api.github.com URL. It is
# either empty (direct) or e.g. "https://cdn.gh-proxy.org/" so that the final
# URL is "https://cdn.gh-proxy.org/https://github.com/...".
DEFAULT_MIRROR="https://cdn.gh-proxy.org"
case "${SIFT_MIRROR:-auto}" in
  auto)
    if curl -fsS --connect-timeout 5 --max-time 10 -o /dev/null "https://github.com" 2>/dev/null; then
      MIRROR=""
    else
      MIRROR="${DEFAULT_MIRROR%/}/"
      warn "github.com unreachable — falling back to mirror ${DEFAULT_MIRROR}"
    fi
    ;;
  off|"")
    MIRROR=""
    ;;
  *)
    MIRROR="${SIFT_MIRROR%/}/"
    log "Using mirror ${SIFT_MIRROR%/}"
    ;;
esac

# ---- resolve version --------------------------------------------------------
if [ -z "$VERSION" ]; then
  log "Resolving latest release from $REPO ..."
  if [ -z "$MIRROR" ]; then
    # Direct: follow the /releases/latest redirect (no API rate limit, no jq).
    VERSION="$(
      curl -fsSLI -o /dev/null -w '%{url_effective}' \
        "https://github.com/${REPO}/releases/latest" \
        | sed 's#.*/tag/##'
    )"
  else
    # Mirrored: the redirect does not survive the proxy, so read `tag_name`
    # from the GitHub API through the same mirror.
    VERSION="$(
      curl -fsSL "${MIRROR}https://api.github.com/repos/${REPO}/releases/latest" \
        | grep -oE '"tag_name"[[:space:]]*:[[:space:]]*"[^"]+"' | head -1 \
        | sed -E 's/.*"([^"]+)"$/\1/'
    )"
  fi
  [ -n "$VERSION" ] || die "could not resolve latest version (set SIFT_VERSION manually)"
fi
log "Version: $VERSION"

# ---- download + verify ------------------------------------------------------
name="sift-${VERSION}-${target}"
archive="${name}.${archive_ext}"
url="${MIRROR}https://github.com/${REPO}/releases/download/${VERSION}/${archive}"
sum_url="${url}.sha256"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

log "Downloading $url"
curl -fsSL "$url" -o "$tmpdir/$archive"

log "Downloading checksum"
if curl -fsSL "$sum_url" -o "$tmpdir/$archive.sha256"; then
  ( cd "$tmpdir"
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum -c "$archive.sha256" || die "checksum mismatch"
    elif command -v shasum >/dev/null 2>&1; then
      shasum -a 256 -c "$archive.sha256" || die "checksum mismatch"
    else
      warn "no sha256 verifier found (sha256sum/shasum); skipping verification"
    fi )
else
  warn "no checksum file at $sum_url; skipping verification"
fi

# ---- extract ----------------------------------------------------------------
log "Extracting"
case "$archive_ext" in
  tar.gz)
    tar -C "$tmpdir" -xzf "$tmpdir/$archive"
    ;;
  zip)
    if command -v unzip >/dev/null 2>&1; then
      unzip -q "$tmpdir/$archive" -d "$tmpdir"
    elif tar -C "$tmpdir" -xf "$tmpdir/$archive" 2>/dev/null; then
      : # bsdtar (Windows 10+, macOS) reads zip natively
    else
      die "need 'unzip' (or a zip-capable tar) to extract $archive"
    fi
    ;;
esac

extracted_bin="$tmpdir/$name/$bin_name"
[ -f "$extracted_bin" ] || die "archive layout unexpected: missing $extracted_bin"

# ---- install ----------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
cp "$extracted_bin" "$INSTALL_DIR/$bin_name"
chmod +x "$INSTALL_DIR/$bin_name" 2>/dev/null || true
log "Installed: $INSTALL_DIR/$bin_name"

# ---- PATH hint --------------------------------------------------------------
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    cat <<EOF

NOTE: $INSTALL_DIR is not on your PATH. Add it to your shell profile:

    export PATH="$INSTALL_DIR:\$PATH"

EOF
    ;;
esac

"$INSTALL_DIR/$bin_name" --version 2>/dev/null || true
log "Done. Try:  sift --help"
