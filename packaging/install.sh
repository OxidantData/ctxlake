#!/bin/sh
# Installs the `ctxlake` and `ctxlake-hook` binaries from a GitHub Release
# archive built by .github/workflows/release.yml.
#
# POSIX `sh`, not bash: this is meant to run as `curl ... | sh` on whatever
# shell a user's system ships as /bin/sh (dash on Debian/Ubuntu, for
# instance), so bash-only syntax (arrays, `[[`, `local`) is off the table.
#
# Usage:
#   curl --proto '=https' --tlsv1.2 -sSf \
#     https://raw.githubusercontent.com/OxidantData/ctxlake/main/packaging/install.sh | sh
#
# Override the version (default: the latest GitHub Release) or install
# directory with env vars:
#   CTXLAKE_VERSION=v0.1.0 CTXLAKE_INSTALL_DIR="$HOME/.local/bin" sh install.sh
set -eu

REPO="OxidantData/ctxlake"
INSTALL_DIR="${CTXLAKE_INSTALL_DIR:-$HOME/.local/bin}"

log() { printf '%s\n' "$*" >&2; }
die() {
  log "error: $*"
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not found on PATH"
}

need curl
need tar
need mktemp

# --- 1. target triple, the same four this repo's release workflow builds ---
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin) os_part="apple-darwin" ;;
  Linux) os_part="unknown-linux-gnu" ;;
  *) die "unsupported OS '$os' — ctxlake ships prebuilt archives for macOS and Linux only; see docs/getting-started.md for cargo install as a fallback" ;;
esac

case "$arch" in
  arm64 | aarch64) arch_part="aarch64" ;;
  x86_64 | amd64) arch_part="x86_64" ;;
  *) die "unsupported architecture '$arch' — see docs/getting-started.md for cargo install as a fallback" ;;
esac

target="${arch_part}-${os_part}"

# --- 2. resolve the version ---
version="${CTXLAKE_VERSION:-}"
if [ -z "$version" ]; then
  # No `jq` dependency: pull just the "tag_name" field out of the release API
  # response with a small, tolerant grep/sed pass rather than a full JSON parse.
  api_response="$(curl --proto '=https' --tlsv1.2 -sSf "https://api.github.com/repos/${REPO}/releases/latest")" ||
    die "could not reach the GitHub releases API for ${REPO}. If this repo has no release yet, pass CTXLAKE_VERSION=vX.Y.Z explicitly, or use 'cargo install' (see docs/getting-started.md)."
  version="$(printf '%s' "$api_response" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$version" ] || die "could not determine the latest release tag from the GitHub API response"
fi

archive="ctxlake-${target}.tar.xz"
base_url="https://github.com/${REPO}/releases/download/${version}"

log "ctxlake installer: version ${version}, target ${target}"

# --- 3. download, verify, extract ---
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

curl --proto '=https' --tlsv1.2 -sSfL -o "${workdir}/${archive}" "${base_url}/${archive}" ||
  die "failed to download ${base_url}/${archive} — does release ${version} exist for this target?"
curl --proto '=https' --tlsv1.2 -sSfL -o "${workdir}/SHA256SUMS" "${base_url}/SHA256SUMS" ||
  die "failed to download ${base_url}/SHA256SUMS"

# Verify against the one line in SHA256SUMS naming this archive, not the whole
# file (SHA256SUMS covers all four targets' archives, only one of which was
# downloaded here) — same shasum(1)/sha256sum(1) line format on macOS and Linux.
checksum_line="$(grep " ${archive}\$" "${workdir}/SHA256SUMS" || true)"
[ -n "$checksum_line" ] || die "no checksum entry for ${archive} in SHA256SUMS"
expected="$(printf '%s' "$checksum_line" | awk '{print $1}')"

if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${workdir}/${archive}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${workdir}/${archive}" | awk '{print $1}')"
else
  die "neither sha256sum nor shasum is available to verify the download"
fi

[ "$expected" = "$actual" ] || die "checksum mismatch for ${archive}: expected ${expected}, got ${actual}"

mkdir -p "$INSTALL_DIR"

# Extract everything, then find the binaries — rather than naming members on the
# tar command line.
#
# GNU tar matches member names literally, so `tar -x ... ctxlake` does NOT match an
# entry stored as `./ctxlake`, and fails with "Not found in archive". bsdtar (macOS)
# normalizes the prefix away and matches happily. v0.1.0's archives were built with
# `tar -C stage .`, which stores the `./` form — so naming members worked on macOS
# and failed on every Linux box. Extracting wholesale depends on neither tar's
# matching rules nor on how a given release happened to be packed.
tar -xJf "${workdir}/${archive}" -C "$workdir"

for bin in ctxlake ctxlake-hook; do
  found="$(find "$workdir" -type f -name "$bin" -print | head -n 1)"
  [ -n "$found" ] || die "${archive} does not contain ${bin}"
  mv "$found" "${INSTALL_DIR}/${bin}"
  chmod +x "${INSTALL_DIR}/${bin}"
done

log "installed ctxlake and ctxlake-hook ${version} to ${INSTALL_DIR}"
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *) log "note: ${INSTALL_DIR} is not on your PATH — add it, e.g. export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac
