#!/usr/bin/env bash
# Fills packaging/ctxlake.rb.tmpl's @@PLACEHOLDER@@ markers from a version and
# a directory of already-built `<target>.tar.xz.sha256` files (one per line,
# `sha256sum`/`shasum -a 256` format: "<hex>  <filename>"), and writes the
# rendered formula to stdout.
#
# Kept as its own script (rather than inlined into release.yml) so it has
# something to run under `bash packaging/render-formula.sh` outside CI —
# the "formula template renders and is valid Ruby" test in this branch runs
# exactly that, with no GitHub Actions runner required.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: render-formula.sh <version-without-v-prefix> <dir-of-target.tar.xz.sha256-files>" >&2
  exit 1
fi

VERSION="$1"
SHA_DIR="$2"
TEMPLATE="$(dirname "$0")/ctxlake.rb.tmpl"

# `sha256sum`'s own output format ("<hex>  <path>") is what both the release
# workflow and a developer's local `shasum -a 256` produce, so pull the hex out
# of that rather than assuming a bare-hex file — the .sha256 files this reads
# are the ones a `tar.xz` build step deposited, not something reformatted
# specifically for this script.
sha_for() {
  local target="$1"
  local file="$SHA_DIR/ctxlake-${target}.tar.xz.sha256"
  if [ ! -f "$file" ]; then
    echo "::error::missing checksum file for target ${target}: ${file}" >&2
    exit 1
  fi
  awk '{print $1}' "$file"
}

SHA_AARCH64_APPLE_DARWIN="$(sha_for aarch64-apple-darwin)"
SHA_X86_64_APPLE_DARWIN="$(sha_for x86_64-apple-darwin)"
SHA_AARCH64_UNKNOWN_LINUX_GNU="$(sha_for aarch64-unknown-linux-gnu)"
SHA_X86_64_UNKNOWN_LINUX_GNU="$(sha_for x86_64-unknown-linux-gnu)"

sed \
  -e "s/@@VERSION@@/${VERSION}/g" \
  -e "s/@@SHA256_AARCH64_APPLE_DARWIN@@/${SHA_AARCH64_APPLE_DARWIN}/g" \
  -e "s/@@SHA256_X86_64_APPLE_DARWIN@@/${SHA_X86_64_APPLE_DARWIN}/g" \
  -e "s/@@SHA256_AARCH64_UNKNOWN_LINUX_GNU@@/${SHA_AARCH64_UNKNOWN_LINUX_GNU}/g" \
  -e "s/@@SHA256_X86_64_UNKNOWN_LINUX_GNU@@/${SHA_X86_64_UNKNOWN_LINUX_GNU}/g" \
  "$TEMPLATE"
