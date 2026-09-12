# packaging/

Release-time artifacts that `.github/workflows/release.yml` builds from, plus the
scripts a human runs by hand where automation deliberately stops.

## `install.sh`

The `curl | sh` installer documented in `docs/getting-started.md`. Detects the four
targets `release.yml` builds (`{aarch64,x86_64}-apple-darwin`,
`{aarch64,x86_64}-unknown-linux-gnu`), downloads that target's `.tar.xz` plus the
release's `SHA256SUMS`, verifies the archive against it, and installs `ctxlake` and
`ctxlake-hook` to `$HOME/.local/bin` (override with `CTXLAKE_INSTALL_DIR`).

## `ctxlake.rb.tmpl` + `render-formula.sh`

A Homebrew formula template with `@@PLACEHOLDER@@` markers for the version and each
target's sha256, in the shape of `../../homebrew-tap/Formula/oxidant.rb` (the house
style for this org's taps). `render-formula.sh <version> <dir-of-.sha256-files>`
fills it in and prints the rendered formula to stdout; `release.yml`'s `release` job
runs this once the four archives and checksums exist and attaches the result to the
GitHub Release as `ctxlake.rb`.

**This script does not push to `OxidantData/homebrew-tap` — the tap pulls.** Its own
`sync-formulae` workflow runs hourly, downloads `ctxlake.rb` from this repo's latest
release, checks it parses, and commits it. Nothing here needs a token scoped to
another repo, which was the original objection to automating it.

It can also be triggered immediately:

```sh
gh workflow run sync-formulae.yml --repo OxidantData/homebrew-tap
```

> This was a manual step for the project's first three releases, and it was never
> once performed — so `brew install oxidantdata/tap/ctxlake` failed for everyone
> while `docs/getting-started.md` advertised it. If you are tempted to document
> another manual post-release step, that is the precedent.

Sanity-check a rendered formula without a release existing yet:

```sh
mkdir -p /tmp/shas
for t in aarch64-apple-darwin x86_64-apple-darwin \
         aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  echo "0000000000000000000000000000000000000000000000000000000000000000  ctxlake-${t}.tar.xz" \
    > "/tmp/shas/ctxlake-${t}.tar.xz.sha256"
done
bash packaging/render-formula.sh 0.1.0 /tmp/shas | ruby -c -
```

## Releasing twice for one tag is safe, and used not to be

Both a tag push and `gh workflow run release.yml` fire this workflow, and the
concurrency group serialises them rather than deduping — so one tag can genuinely
produce two builds. Rust builds are not byte-reproducible, so those two builds
produce different archives.

The release job used to upload with `--clobber`, which meant the second run replaced
archives the first had already published. Anything that read the release in between
was then wrong about its contents: for v0.1.4 the Homebrew formula was rendered from
run A's checksums, run B overwrote the archives, and `brew install` failed with a
checksum mismatch against a release that looked complete and green.

Published artifacts are now immutable. A run that finds a complete asset set exits
saying so; one that finds a partial set — a previous run that died mid-upload — tops
up only what is missing. Re-running a release is a no-op rather than a rewrite.
