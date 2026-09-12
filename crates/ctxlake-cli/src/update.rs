//! `ctxlake update` — one command that updates ctxlake however it was installed.
//!
//! There are four ways a `ctxlake` binary gets onto a machine (Homebrew, the curl
//! installer, `cargo install`, and a build from this repo), each with a different
//! correct way to upgrade it, and the docs used to answer "how do I update" with a
//! table of all four and a request that you know which one you are. That is a
//! question the binary can answer about itself.
//!
//! **The install method is inferred from where this binary lives**, not remembered
//! from install time — nothing writes a receipt, and a remembered one would be wrong
//! the moment someone moved the file.
//!
//! Two rules the implementation holds:
//!
//! - **Never write into a directory a package manager owns.** Dropping a new binary
//!   into `<brew prefix>/Cellar/...` leaves Homebrew's manifest describing a file that
//!   is no longer there: `brew list --versions` reports the old version, the next
//!   `brew upgrade` overwrites the update, and `brew doctor` starts complaining. So
//!   for a managed install this delegates to the manager instead of self-replacing.
//! - **Replace by rename, never by truncate.** `ctxlake-hook` fires on every tool call
//!   in every live agent session on this machine. Writing over it in place means some
//!   session's hook executes a half-written file. A rename within the same directory
//!   is atomic, and any process already running keeps the old inode.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The GitHub repository releases are published from — the same one
/// `packaging/install.sh` reads.
const REPO: &str = "OxidantData/ctxlake";

/// Both binaries this project ships. `ctxlake-hook` is easy to forget and is the one
/// with a compatibility contract: it writes envelopes the daemon reads, so leaving it
/// a version behind is how a spool starts failing to parse.
const BINARIES: &[&str] = &["ctxlake", "ctxlake-hook"];

/// How this copy of ctxlake got here, and therefore how it should be replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// Under a Homebrew prefix. `brew` owns the files and the manifest.
    Homebrew,
    /// Under `~/.cargo/bin` — `cargo install` wrote it and will overwrite it.
    Cargo,
    /// A `target/{debug,release}` directory in a checkout. Updating means `git pull`.
    CargoTarget,
    /// Anywhere else: the curl installer's `~/.local/bin`, or a manual copy. This is
    /// the only case where ctxlake owns the file and may replace it itself.
    SelfManaged,
}

/// Classify an executable path.
///
/// Pure, and takes the path explicitly, so the table of layouts is testable without
/// installing ctxlake four different ways.
pub fn classify(exe: &Path, brew_prefix: Option<&Path>) -> InstallMethod {
    let s = exe.to_string_lossy();
    // Checked before Homebrew: a checkout inside a Homebrew prefix is still a
    // checkout, and `brew upgrade` would be the wrong advice for it.
    if s.contains("/target/debug/") || s.contains("/target/release/") {
        return InstallMethod::CargoTarget;
    }
    if let Some(prefix) = brew_prefix {
        if exe.starts_with(prefix) {
            return InstallMethod::Homebrew;
        }
    }
    // Matched on the path rather than only on `$CARGO_HOME`, because the variable is
    // usually unset and the default location is where the binary actually is.
    if s.contains("/.cargo/bin/") {
        return InstallMethod::Cargo;
    }
    InstallMethod::SelfManaged
}

/// `brew --prefix`, or `None` when Homebrew is not installed.
fn brew_prefix() -> Option<PathBuf> {
    let out = std::process::Command::new("brew")
        .arg("--prefix")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then(|| PathBuf::from(p))
}

/// The newest published release tag, e.g. `v0.1.6`.
///
/// Same endpoint and same tolerant extraction as `packaging/install.sh`, which cannot
/// take a JSON parser as a dependency because it is `curl | sh` bootstrap.
async fn latest_tag() -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = reqwest::Client::new()
        .get(&url)
        // GitHub rejects an API request with no User-Agent.
        .header("User-Agent", concat!("ctxlake/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("asking {url} for the latest release"))?
        .error_for_status()
        .context("the GitHub releases API rejected the request")?
        .text()
        .await
        .context("reading the releases API response")?;
    let v: serde_json::Value =
        serde_json::from_str(&body).context("parsing the releases API response")?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .context("the releases API response carried no tag_name")
}

/// Compare `v0.1.6`-style tags against this build's version.
///
/// Deliberately not a semver dependency: these are the project's own tags, always
/// `vMAJOR.MINOR.PATCH`, and a comparison that cannot parse one falls back to
/// "different means newer" rather than refusing to update.
pub fn is_newer(tag: &str, current: &str) -> bool {
    fn parts(s: &str) -> Option<(u64, u64, u64)> {
        let s = s.trim().trim_start_matches('v');
        // Drop any pre-release or build suffix before comparing numbers.
        let core = s.split(['-', '+']).next()?;
        let mut it = core.split('.');
        Some((
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        ))
    }
    match (parts(tag), parts(current)) {
        (Some(a), Some(b)) => a > b,
        // Unparseable on either side: treat "not identical" as "worth offering",
        // which errs toward telling the user something rather than silently claiming
        // they are current.
        _ => tag.trim().trim_start_matches('v') != current.trim(),
    }
}

/// `ctxlake update`.
pub async fn run(check_only: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let exe = std::env::current_exe().context("locating the running ctxlake binary")?;
    let method = classify(&exe, brew_prefix().as_deref());

    println!("installed  {current}  ({})", exe.display());

    let tag = latest_tag().await?;
    if !is_newer(&tag, current) {
        println!("latest     {tag}");
        println!("\nAlready up to date.");
        return Ok(());
    }
    println!("latest     {tag}   <- newer");

    if check_only {
        println!("\nRun `ctxlake update` to install it.");
        return Ok(());
    }

    match method {
        InstallMethod::Homebrew => {
            println!("\nHomebrew owns this install, so it does the upgrade:");
            run_visible("brew", &["update"])?;
            run_visible("brew", &["upgrade", "oxidantdata/tap/ctxlake"])?;
        }
        InstallMethod::Cargo => {
            println!(
                "\nInstalled by cargo, so cargo does the upgrade (this compiles from source):"
            );
            for bin in BINARIES {
                run_visible(
                    "cargo",
                    &[
                        "install",
                        "--git",
                        &format!("https://github.com/{REPO}"),
                        "--tag",
                        &tag,
                        "--force",
                        &format!("ctxlake-{}", if *bin == "ctxlake" { "cli" } else { "hook" }),
                    ],
                )?;
            }
        }
        InstallMethod::CargoTarget => {
            bail!(
                "this is a development build from a checkout ({}), not an installed \
                 release — update it with `git pull && cargo build --release` in that \
                 repository. Refusing to overwrite a build directory.",
                exe.display()
            );
        }
        InstallMethod::SelfManaged => {
            self_replace(&exe, &tag).await?;
        }
    }

    restart_daemon_if_supervised();
    Ok(())
}

/// Run a command with its output attached to this terminal.
///
/// Inherited stdio rather than captured, because `brew upgrade` and `cargo install`
/// take minutes and print progress — swallowing it and re-printing at the end would
/// look like a hang.
fn run_visible(program: &str, args: &[&str]) -> Result<()> {
    println!("  $ {program} {}", args.join(" "));
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running `{program}`"))?;
    if !status.success() {
        bail!("`{program} {}` failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Download the release archive and replace both binaries in place.
///
/// Only reached for [`InstallMethod::SelfManaged`] — the case where ctxlake, not a
/// package manager, owns the files.
async fn self_replace(exe: &Path, tag: &str) -> Result<()> {
    let dir = exe
        .parent()
        .context("the running binary has no parent directory")?;
    let target = target_triple()?;
    let archive = format!("ctxlake-{target}.tar.xz");
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");

    println!("\nUpdating {} in {}", BINARIES.join(" and "), dir.display());

    let work = ScratchDir::new().context("creating a scratch directory for the download")?;
    let archive_path = work.path().join(&archive);
    download(&format!("{base}/{archive}"), &archive_path).await?;

    // Verified against SHA256SUMS, exactly as the installer does. An update path that
    // skips the check the install path makes is a downgrade in security that nobody
    // would notice until it mattered.
    let sums_path = work.path().join("SHA256SUMS");
    download(&format!("{base}/SHA256SUMS"), &sums_path).await?;
    verify_checksum(&archive_path, &sums_path, &archive)?;
    println!("  checksum ok");

    // No tar/xz crate: the workspace has neither, and shelling out to the same `tar`
    // `packaging/install.sh` already relies on keeps one extraction behaviour to
    // reason about instead of two. Extract wholesale rather than naming members —
    // GNU tar matches `./ctxlake` literally and bsdtar does not, which is the bug
    // v0.1.0's archives hit.
    let status = std::process::Command::new("tar")
        .arg("-xJf")
        .arg(&archive_path)
        .arg("-C")
        .arg(work.path())
        .status()
        .context("running `tar` to unpack the release archive")?;
    if !status.success() {
        bail!("`tar -xJf {archive}` failed ({status}) — is xz support available?");
    }

    for bin in BINARIES {
        let found = find_file(work.path(), bin)
            .with_context(|| format!("{archive} does not contain {bin}"))?;
        install_atomically(&found, &dir.join(bin))?;
        println!("  updated {}", dir.join(bin).display());
    }
    println!("\nctxlake is now {}.", tag.trim_start_matches('v'));
    Ok(())
}

/// A scratch directory removed when it goes out of scope.
///
/// `tempfile` is a dev-dependency of this crate, and promoting it to a runtime one for
/// a single directory is not a trade worth making — this is the whole of what is
/// needed. Named by pid so two concurrent updates cannot share one.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("ctxlake-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Best effort: leaving a few megabytes in the temp directory is not worth
        // failing an otherwise successful update over.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Move `src` into place at `dest` atomically.
///
/// **Rename, never write-in-place.** `ctxlake-hook` runs on every tool call in every
/// live agent session; overwriting it in place means one of those sessions executes a
/// partially written file, and the failure surfaces as a corrupt hook in somebody
/// else's terminal. A rename within one filesystem is atomic, and a process that
/// already has the old file open keeps it.
///
/// The staging copy is made in the destination directory rather than in the temp
/// directory, because `/tmp` is frequently a different filesystem and `rename(2)`
/// across filesystems fails with `EXDEV`.
fn install_atomically(src: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().context("destination has no directory")?;
    let staged = dir.join(format!(
        ".{}.new-{}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&staged);
    std::fs::copy(src, &staged)
        .with_context(|| format!("staging the new binary at {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", staged.display()))?;
    }
    std::fs::rename(&staged, dest).with_context(|| {
        let _ = std::fs::remove_file(&staged);
        format!(
            "replacing {} — is it writable? ({} is where it lives)",
            dest.display(),
            dir.display()
        )
    })?;
    Ok(())
}

async fn download(url: &str, to: &Path) -> Result<()> {
    println!("  fetching {url}");
    let bytes = reqwest::Client::new()
        .get(url)
        .header("User-Agent", concat!("ctxlake/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading {url}"))?;
    std::fs::write(to, &bytes).with_context(|| format!("writing {}", to.display()))?;
    Ok(())
}

/// Check `archive` against the one line in `SHA256SUMS` that names it.
///
/// The *one line*, not the file as a whole: SHA256SUMS covers all four targets and
/// only one archive was downloaded. Split out and pure so the mismatch path is
/// testable — an unverified update path would be worse than no update command.
pub fn verify_checksum(archive: &Path, sums: &Path, archive_name: &str) -> Result<()> {
    let sums = std::fs::read_to_string(sums).context("reading SHA256SUMS")?;
    let expected = expected_sum(&sums, archive_name)
        .with_context(|| format!("no checksum entry for {archive_name} in SHA256SUMS"))?;
    let bytes = std::fs::read(archive)
        .with_context(|| format!("reading {} to hash it", archive.display()))?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!(
            "checksum mismatch for {archive_name}: expected {expected}, got {actual}. \
             Not installing it."
        );
    }
    Ok(())
}

/// The expected hash for `name`, from `SHA256SUMS` content.
///
/// Matched on the whole final field so `ctxlake-x86_64-unknown-linux-gnu.tar.xz` can
/// never be satisfied by a line for `ctxlake-aarch64-unknown-linux-gnu.tar.xz`, and
/// `.tar.xz` is never satisfied by a `.tar.xz.sha256` sidecar line.
pub fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        // `sha256sum` writes "<hash>  <name>" and may prefix the name with `*`.
        let file = it.next()?.trim_start_matches('*');
        (file == name).then(|| hash.to_string())
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The release archive naming this host — the same four targets the release workflow
/// builds and `packaging/install.sh` resolves.
fn target_triple() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => bail!("no prebuilt release for architecture '{other}' — build from source"),
    };
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => bail!("no prebuilt release for '{other}' — build from source"),
    };
    Ok(format!("{arch}-{os}"))
}

fn find_file(root: &Path, name: &str) -> Result<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().map(|f| f == name).unwrap_or(false) {
                return Ok(path);
            }
        }
    }
    bail!("{name} not found under {}", root.display())
}

/// Restart the supervised daemon, if there is one.
///
/// A running daemon holds the *old* binary open — that is what makes the rename safe —
/// so it keeps running the version you just replaced until something restarts it.
/// Doing that here is the difference between `ctxlake update` and `ctxlake update`
/// followed by a step the docs have to remember to mention.
///
/// Best-effort: an update that succeeded must not report failure because a restart
/// did, and someone running the daemon under their own supervisor has nothing here to
/// restart.
fn restart_daemon_if_supervised() {
    if !crate::service::is_installed() {
        return;
    }
    println!("\nrefreshing the sync daemon's service unit");
    // Before restarting, not after: a unit written by the previous version may name a
    // binary path this upgrade just deleted (a package manager's version-stamped
    // directory), in which case restarting it fails and the daemon stays down. It may
    // also pin a PATH from before a provider binary was installed. Re-rendering costs
    // nothing when neither is true — `write_unit` reports no change.
    match crate::service::refresh_installed_unit() {
        Ok(true) => println!("  unit updated for the new binary"),
        Ok(false) => println!("  unit already current"),
        Err(e) => println!("  could not refresh it ({e:#}) — run `ctxlake sync install`"),
    }
    match crate::service::restart_installed() {
        Ok(()) => println!("  restarted — `ctxlake sync status` to confirm"),
        Err(e) => println!("  could not restart it ({e:#}) — run `ctxlake sync restart`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_homebrew_install_is_never_replaced_by_ctxlake_itself() {
        // Writing into the Cellar leaves brew's manifest describing a file that is not
        // there: `brew list --versions` reports the old version, the next
        // `brew upgrade` silently overwrites the update.
        let prefix = Path::new("/opt/homebrew");
        assert_eq!(
            classify(Path::new("/opt/homebrew/bin/ctxlake"), Some(prefix)),
            InstallMethod::Homebrew
        );
        assert_eq!(
            classify(
                Path::new("/opt/homebrew/Cellar/ctxlake/0.1.5/bin/ctxlake"),
                Some(prefix)
            ),
            InstallMethod::Homebrew
        );
        // Intel macOS and Linuxbrew use a different prefix; the check is the reported
        // prefix, never a hardcoded path.
        assert_eq!(
            classify(
                Path::new("/usr/local/bin/ctxlake"),
                Some(Path::new("/usr/local"))
            ),
            InstallMethod::Homebrew
        );
    }

    #[test]
    fn the_curl_installers_location_is_ours_to_replace() {
        assert_eq!(
            classify(
                Path::new("/home/alice/.local/bin/ctxlake"),
                Some(Path::new("/home/linuxbrew/.linuxbrew"))
            ),
            InstallMethod::SelfManaged
        );
        // No Homebrew on the machine at all.
        assert_eq!(
            classify(Path::new("/usr/local/bin/ctxlake"), None),
            InstallMethod::SelfManaged
        );
    }

    #[test]
    fn cargo_installs_and_checkouts_are_told_apart() {
        assert_eq!(
            classify(Path::new("/home/alice/.cargo/bin/ctxlake"), None),
            InstallMethod::Cargo
        );
        assert_eq!(
            classify(
                Path::new("/home/alice/src/ctxlake/target/debug/ctxlake"),
                None
            ),
            InstallMethod::CargoTarget
        );
        assert_eq!(
            classify(
                Path::new("/home/alice/src/ctxlake/target/release/ctxlake"),
                None
            ),
            InstallMethod::CargoTarget
        );
        // A checkout that happens to sit under a Homebrew prefix is still a checkout,
        // and `brew upgrade` would be nonsense advice for it.
        assert_eq!(
            classify(
                Path::new("/opt/homebrew/src/ctxlake/target/release/ctxlake"),
                Some(Path::new("/opt/homebrew"))
            ),
            InstallMethod::CargoTarget
        );
    }

    #[test]
    fn version_comparison_orders_releases_the_way_a_person_would() {
        assert!(is_newer("v0.1.6", "0.1.5"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("v0.1.5", "0.1.5"));
        assert!(!is_newer("v0.1.4", "0.1.5"));
        // The bug a string comparison would have: "0.1.10" sorts before "0.1.9".
        assert!(
            is_newer("v0.1.10", "0.1.9"),
            "0.1.10 is newer than 0.1.9; lexical comparison says otherwise"
        );
        assert!(!is_newer("v0.1.9", "0.1.10"));
    }

    #[test]
    fn a_sidecar_line_can_never_satisfy_the_archives_checksum() {
        // This is the v0.1.4 failure in miniature: every `.sha256` sidecar agreed with
        // every other sidecar and none of them matched the archive a client downloaded.
        // Matching a prefix, or matching `contains`, would let the sidecar's own line
        // answer for the archive.
        let sums = "\
aaaa  ctxlake-aarch64-apple-darwin.tar.xz
bbbb  ctxlake-aarch64-apple-darwin.tar.xz.sha256
cccc  ctxlake-x86_64-unknown-linux-gnu.tar.xz
";
        assert_eq!(
            expected_sum(sums, "ctxlake-aarch64-apple-darwin.tar.xz").as_deref(),
            Some("aaaa")
        );
        assert_eq!(
            expected_sum(sums, "ctxlake-x86_64-unknown-linux-gnu.tar.xz").as_deref(),
            Some("cccc"),
            "one target's line must never answer for another's"
        );
        assert_eq!(expected_sum(sums, "ctxlake-nope.tar.xz"), None);
    }

    #[test]
    fn a_star_prefixed_binary_mode_line_still_matches() {
        // `sha256sum -b` writes "<hash> *<name>".
        assert_eq!(
            expected_sum(
                "dddd *ctxlake-aarch64-apple-darwin.tar.xz\n",
                "ctxlake-aarch64-apple-darwin.tar.xz"
            )
            .as_deref(),
            Some("dddd")
        );
    }

    #[test]
    fn a_tampered_archive_is_refused_rather_than_installed() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("ctxlake-test.tar.xz");
        std::fs::write(&archive, b"the real bytes").unwrap();
        let real = sha256_hex(b"the real bytes");

        let sums = dir.path().join("SHA256SUMS");
        std::fs::write(&sums, format!("{real}  ctxlake-test.tar.xz\n")).unwrap();
        verify_checksum(&archive, &sums, "ctxlake-test.tar.xz").expect("the honest case");

        std::fs::write(&archive, b"something else entirely").unwrap();
        let err = verify_checksum(&archive, &sums, "ctxlake-test.tar.xz")
            .expect_err("a changed archive must not install");
        let msg = format!("{err:#}");
        assert!(msg.contains("checksum mismatch"), "{msg}");
        assert!(msg.contains("Not installing"), "{msg}");
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        // Guards the hex formatting, where a `{:x}` on the whole digest or a dropped
        // leading zero would produce a hash that never matches anything.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex(b"").len(), 64, "every hash is 64 hex characters");
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_binary_is_atomic_and_leaves_it_executable() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("ctxlake-hook");
        std::fs::write(&dest, b"old").unwrap();

        let src = dir.path().join("downloaded");
        std::fs::write(&src, b"new").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();

        install_atomically(&src, &dest).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o111,
            0o111,
            "a binary that is not executable is a broken install"
        );
        // The staging copy must be cleaned up, not left beside the binary.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".new-"))
            .collect();
        assert!(strays.is_empty(), "left staging files behind: {strays:?}");
    }

    #[cfg(unix)]
    #[test]
    fn staging_happens_in_the_destination_directory_not_in_tmp() {
        // `rename(2)` across filesystems fails with EXDEV, and /tmp is very often a
        // different filesystem from ~/.local/bin. Staging in the destination directory
        // is what makes the rename possible at all.
        let dir = tempfile::tempdir().unwrap();
        let dest_dir = dir.path().join("bin");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("ctxlake");
        std::fs::write(&dest, b"old").unwrap();

        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let src = elsewhere.join("ctxlake");
        std::fs::write(&src, b"new").unwrap();

        install_atomically(&src, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(
            std::fs::read(&src).unwrap(),
            b"new",
            "the source is copied, not moved — a failed rename must not destroy it"
        );
    }
}
