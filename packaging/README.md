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

**This script does not push to `OxidantData/homebrew-tap`.** `release.yml` holds no
token scoped to that repo, and auto-committing a formula into someone else's tap on
every release is more blast radius than the convenience is worth. Publishing a new
version is a manual step after each release:

```sh
# from a checkout of OxidantData/homebrew-tap
curl -sSfL -o Formula/ctxlake.rb \
  "https://github.com/OxidantData/ctxlake/releases/download/<tag>/ctxlake.rb"
git commit -am "ctxlake <version>"
git push
```

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
