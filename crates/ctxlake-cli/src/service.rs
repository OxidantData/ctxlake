//! `ctxlake sync install` — hand the daemon to the machine's own service manager.
//!
//! `ctxlake sync start` daemonizes by re-exec'ing itself (see `sync_cmd.rs`), which
//! is fine until the machine reboots: nothing brings it back, and the first symptom
//! is a briefing that silently stops updating. Capture keeps working — the hook only
//! writes to the local spool — so the failure is invisible until someone notices the
//! fleet has gone quiet. That gap is what this module closes.
//!
//! The fix is not to write a better daemonizer. Every platform already ships a
//! supervisor that starts things at login, restarts them on crash, throttles restart
//! loops, and captures logs; reimplementing that badly in a Rust binary is how you
//! get a daemon that dies silently in a way nobody can debug. So `install` renders a
//! unit file and hands over:
//!
//! | | Linux | macOS |
//! |---|---|---|
//! | Manager | systemd **user** unit | launchd **LaunchAgent** |
//! | Unit | `~/.config/systemd/user/ctxlake-sync.service` | `~/Library/LaunchAgents/com.oxidantdata.ctxlake-sync.plist` |
//! | Runs as | the invoking user | the logged-in user |
//! | Survives reboot | only with `loginctl enable-linger` | yes, at login |
//!
//! Both are **user**-scoped on purpose. The daemon reads this user's spool and writes
//! with this user's store credentials; a system unit or a LaunchDaemon runs as root
//! and would reach neither.
//!
//! **The one real footgun is systemd lingering.** A user manager normally stops when
//! the last session for that user ends, so on a headless box — exactly where a fleet
//! agent runs — the service comes up, works, and then never returns after a reboot or
//! logout. [`install`] checks for it and says so loudly rather than leaving it to be
//! discovered days later.
//!
//! The templates live in `packaging/service/` and are `include_str!`'d rather than
//! read from disk: this binary is installed by `curl | sh` or Homebrew to a machine
//! that has no copy of the repo, so a runtime file read would work in development and
//! fail for every real user.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::paths;

/// The systemd unit name and the launchd label — also what `systemctl`/`launchctl`
/// are addressed by, so they are defined once here rather than spelled out at each
/// call site.
const SYSTEMD_UNIT: &str = "ctxlake-sync.service";
const LAUNCHD_LABEL: &str = "com.oxidantdata.ctxlake-sync";

const SYSTEMD_TEMPLATE: &str = include_str!("../../../packaging/service/ctxlake-sync.service.tmpl");
const LAUNCHD_TEMPLATE: &str =
    include_str!("../../../packaging/service/com.oxidantdata.ctxlake-sync.plist.tmpl");

/// Which supervisor this host uses.
///
/// Decided by the target triple rather than by probing for a running `systemd`: a
/// Linux host without systemd (a container, an Alpine box on OpenRC) should get a
/// clear "no supported service manager" message naming what to do instead, not a
/// rendered unit file that nothing will ever read.
///
/// `cfg!` rather than `#[cfg]` so both arms compile on every platform. With `#[cfg]`,
/// `Manager::Launchd` is never constructed in a Linux build and `clippy -D warnings`
/// — which CI runs on Linux — fails on a dead variant that is not dead at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    pub fn detect() -> Result<Self> {
        if cfg!(target_os = "linux") {
            Ok(Manager::Systemd)
        } else if cfg!(target_os = "macos") {
            Ok(Manager::Launchd)
        } else {
            bail!(
                "ctxlake sync install supports systemd (Linux) and launchd (macOS); \
                 on this platform, run `ctxlake sync run --foreground` from whatever \
                 supervisor you already use"
            )
        }
    }

    /// Where the rendered unit is written.
    pub fn unit_path(self, home: &Path) -> PathBuf {
        match self {
            Manager::Systemd => home
                .join(".config")
                .join("systemd")
                .join("user")
                .join(SYSTEMD_UNIT),
            Manager::Launchd => home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
        }
    }

    fn human_name(self) -> &'static str {
        match self {
            Manager::Systemd => "systemd (user)",
            Manager::Launchd => "launchd (LaunchAgent)",
        }
    }
}

/// `&`, `<` and `>` are the three characters that can turn a path into malformed XML.
///
/// A home directory containing `&` is rare but entirely legal, and the failure mode
/// is a plist that `launchctl` rejects with a parse error naming a line number rather
/// than the real cause. Escaping is two lines; debugging that is not.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The directories a supervised daemon gets on `PATH` when nobody pins one.
///
/// launchd hands a job `/usr/bin:/bin:/usr/sbin:/sbin` and a systemd user unit gets
/// something equally minimal. Neither contains `/opt/homebrew/bin`, `/usr/local/bin`
/// or `~/.local/bin` — which is to say neither contains `claude`, `ollama`, or almost
/// anything else a person installed deliberately.
const FALLBACK_PATH_DIRS: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// The `PATH` to pin into the unit.
///
/// **A unit must carry the PATH it was installed with, or exec-based config lies.**
/// `[summarize.batch] provider = "claude-cli"` (and `ollama`, and anything else that
/// shells out) is verified by `ctxlake doctor` and `sync install` in an interactive
/// shell, where `claude` is on `PATH` because Homebrew put it there. The daemon then
/// runs under launchd with `/usr/bin:/bin:/usr/sbin:/sbin` and cannot find the same
/// binary the check just succeeded against. The check and the thing it is checking
/// were running in different environments, so a green check meant nothing.
///
/// Taking the installing process's own `PATH` is what makes those two environments
/// the same one. Non-existent and relative entries are dropped — a `PATH` is allowed
/// to accumulate junk over years of dotfiles, and a unit file is a poor place to
/// enshrine it — and the standard directories are appended so the unit still works if
/// this was invoked from something with a deliberately empty `PATH`.
fn service_path() -> String {
    service_path_from(std::env::var_os("PATH").as_deref())
}

/// [`service_path`], over an explicit `PATH` value.
///
/// The split is only so tests can assert the filtering rules without calling
/// `set_var` on a process-global that every other test in this binary is reading in
/// parallel — the flakiness `paths.rs`'s module doc was written about.
fn service_path_from(path_var: Option<&std::ffi::OsStr>) -> String {
    let mut dirs: Vec<String> = Vec::new();
    let mut push = |d: &str| {
        if d.is_empty() || !d.starts_with('/') {
            return;
        }
        let d = d.trim_end_matches('/');
        let d = if d.is_empty() { "/" } else { d };
        if !dirs.iter().any(|existing| existing == d) {
            dirs.push(d.to_string());
        }
    };
    if let Some(path) = path_var {
        for dir in std::env::split_paths(path) {
            if dir.is_dir() {
                push(&dir.display().to_string());
            }
        }
    }
    for d in FALLBACK_PATH_DIRS {
        push(d);
    }
    dirs.join(":")
}

/// Substitute the template placeholders. Pure, so the rendered output is testable
/// without writing to `$HOME` or shelling out to a service manager.
pub fn render(
    manager: Manager,
    exec: &Path,
    config_path: &Path,
    home: &Path,
    log_dir: &Path,
    path: &str,
) -> String {
    let (exec, config, home_s, log) = (
        exec.display().to_string(),
        config_path.display().to_string(),
        home.display().to_string(),
        log_dir.display().to_string(),
    );
    match manager {
        Manager::Systemd => SYSTEMD_TEMPLATE
            .replace("{{EXEC}}", &exec)
            .replace("{{CONFIG}}", &config)
            .replace("{{HOME}}", &home_s)
            .replace("{{PATH}}", path),
        Manager::Launchd => LAUNCHD_TEMPLATE
            .replace("{{EXEC}}", &xml_escape(&exec))
            .replace("{{CONFIG}}", &xml_escape(&config))
            .replace("{{HOME}}", &xml_escape(&home_s))
            .replace("{{LOG_DIR}}", &xml_escape(&log))
            .replace("{{PATH}}", &xml_escape(path)),
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The absolute path to write into the unit's `ExecStart`.
///
/// **Deliberately not canonicalized**, which reverses this function's first version.
///
/// The original reasoning was that `current_exe` can hand back a symlink — exactly
/// what Homebrew installs into `/opt/homebrew/bin` — and that a unit pointing at a
/// symlink would break when `brew upgrade` repointed it mid-flight. Both halves are
/// true and the conclusion is backwards. Resolving the symlink pins
/// `/opt/homebrew/Cellar/ctxlake/<version>/bin/ctxlake`, and `brew upgrade` *deletes*
/// that directory: the unit then names a path that does not exist, and the daemon
/// crash-loops until someone reinstalls the service. The symlink is repointed, not
/// removed — it is the stable name, and the worst a mid-flight upgrade costs is one
/// restart, which the supervisor was going to do anyway.
///
/// The same argument covers every version-stamped install layout, not just Homebrew's
/// (`~/.local/share/mise/installs/...`, Nix profiles, and so on). `current_exe` on
/// macOS preserves the invoked symlink path, and on Linux `/proc/self/exe` resolves
/// it — so on Linux this is whatever the kernel reports and the guard below only
/// absolutizes it.
fn exec_path() -> Result<PathBuf> {
    let exe =
        std::env::current_exe().context("locating the ctxlake binary for the service unit")?;
    Ok(stable_exec_path(&exe))
}

/// [`exec_path`]'s policy, over a given `current_exe` — the seam the test drives.
///
/// Testing this through `exec_path` would assert nothing: under `cargo test` the
/// running binary is `target/debug/deps/…`, which is not a symlink and not inside a
/// version-stamped directory, so canonicalizing and not canonicalizing return the
/// same path and the regression sails through.
fn stable_exec_path(exe: &Path) -> PathBuf {
    if exe.is_absolute() {
        return exe.to_path_buf();
    }
    // A relative `current_exe` cannot go into a unit file — the supervisor's working
    // directory is not this shell's. Resolving is strictly better than shipping it.
    std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf())
}

/// True when the rendered unit for this host is already on disk.
///
/// This drives whether `start`/`stop`/`status` talk to the service manager or fall
/// back to the pidfile path, so a user who never ran `install` keeps exactly the
/// behaviour they had before this module existed.
pub fn is_installed() -> bool {
    Manager::detect()
        .map(|m| m.unit_path(&home_dir()).exists())
        .unwrap_or(false)
}

fn run(program: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running `{program} {}`", args.join(" ")))
}

/// Run a service-manager command, failing loudly with the manager's own stderr.
///
/// `systemctl` and `launchctl` both explain themselves well on failure and both are
/// silent on success, so surfacing stderr verbatim is more useful than any message
/// this module could invent.
fn run_checked(program: &str, args: &[&str]) -> Result<()> {
    let out = run(program, args)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        bail!(
            "`{program} {}` failed{}",
            args.join(" "),
            if err.is_empty() {
                String::new()
            } else {
                format!(": {err}")
            }
        );
    }
    Ok(())
}

/// Parse `loginctl show-user --property=Linger` output.
///
/// Split out from the shell-out so the parse is tested directly — the property is
/// `Linger=yes`/`Linger=no`, and treating an unparseable answer as "lingering" would
/// silently suppress the one warning this whole check exists to print.
fn parse_linger(output: &str) -> Option<bool> {
    output
        .lines()
        .find_map(|l| l.trim().strip_prefix("Linger="))
        .map(|v| v.trim() == "yes")
}

/// Whether this user's systemd manager survives logout. `None` when it cannot be
/// determined (no `loginctl`, no logind session), which is reported as unknown
/// rather than assumed either way.
fn linger_enabled() -> Option<bool> {
    let user = std::env::var("USER").ok()?;
    let out = run("loginctl", &["show-user", &user, "--property=Linger"]).ok()?;
    if !out.status.success() {
        return None;
    }
    parse_linger(&String::from_utf8_lossy(&out.stdout))
}

/// Write `contents` to `path`, creating parents. Returns whether the file changed,
/// so a re-install can say "already current" instead of implying it rewrote
/// something.
fn write_unit(path: &Path, contents: &str) -> Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if std::fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(false);
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// The `--config` path recorded in an already-installed unit.
///
/// The unit is the only place that knows which config the daemon was installed for —
/// it may not be the default, and `ctxlake update` deliberately loads no config at all,
/// since a machine whose `ctxlake.toml` is broken is one you most want to be able to
/// update out of. Both templates write `--config` immediately before the path, in a
/// form this reads back: quoted on one line for systemd, in its own `<string>` element
/// for launchd.
fn config_path_from_unit(contents: &str) -> Option<PathBuf> {
    // systemd: ExecStart="…/ctxlake" --config "/path/to/ctxlake.toml" sync run …
    if let Some(rest) = contents.split("--config \"").nth(1) {
        if let Some(path) = rest.split('"').next() {
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    // launchd: <string>--config</string> then <string>/path/to/ctxlake.toml</string>
    let rest = contents.split("<string>--config</string>").nth(1)?;
    let open = rest.find("<string>")? + "<string>".len();
    let close = rest[open..].find("</string>")?;
    let path = rest[open..open + close].trim();
    (!path.is_empty()).then(|| PathBuf::from(unxml_escape(path)))
}

/// Undo [`xml_escape`], for a path read back out of a rendered plist.
fn unxml_escape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Re-render an installed unit against this binary and this environment.
///
/// `ctxlake update` calls this after replacing the binaries, and it is the difference
/// between an upgrade that works and one that needs a step the user has to remember.
/// Two things in a unit go stale on an upgrade and neither is visible until the daemon
/// is already dead:
///
/// - **The binary path.** A package manager that installs into a version-stamped
///   directory deletes the old one, so a unit written before the upgrade names a file
///   that no longer exists.
/// - **`PATH`.** A provider binary installed after `sync install` ran is not on the
///   `PATH` the unit pinned.
///
/// Deliberately does *not* re-run the pre-install checks. Those exist to stop someone
/// installing a daemon that cannot work; refusing to refresh an already-installed unit
/// because a store is briefly unreachable would leave it pointing at a deleted binary,
/// which is worse than either outcome the checks were protecting against.
///
/// Returns `Ok(false)` when there is no unit to refresh.
pub fn refresh_installed_unit() -> Result<bool> {
    let manager = match Manager::detect() {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    let home = home_dir();
    let unit_path = manager.unit_path(&home);
    let Ok(existing) = std::fs::read_to_string(&unit_path) else {
        return Ok(false);
    };
    let config_path = config_path_from_unit(&existing).with_context(|| {
        format!(
            "{} does not record a --config path — reinstall with `ctxlake sync install`",
            unit_path.display()
        )
    })?;

    // The same directory `install` uses — `paths::sync_log_file`'s parent, which is
    // fleet-independent. Created here because launchd refuses to start a job whose
    // StandardOutPath directory does not exist.
    let log_dir = home.join(".ctxlake").join("run");
    std::fs::create_dir_all(&log_dir).with_context(|| format!("creating {}", log_dir.display()))?;
    let contents = render(
        manager,
        &exec_path()?,
        &config_path,
        &home,
        &log_dir,
        &service_path(),
    );
    write_unit(&unit_path, &contents)
}

/// The `systemctl --user` invocations `install` makes, in order.
///
/// Pure so the one decision that matters here is testable without a systemd session:
/// **`restart`, not `enable --now`**.
///
/// `--now` means *start*, and `systemctl start` on an already-active unit is a no-op.
/// So re-running `ctxlake sync install` rewrote the unit, reported success, and left
/// the previous daemon running — with the previous binary. Seen on a real host
/// mid-upgrade: the unit carried the new `PATH`, the binary on disk was the new
/// version, and `/proc/<pid>/exe` still pointed at the old one, marked `(deleted)`. It
/// had been running that way for an hour, writing to a `live/` layout the rest of the
/// fleet had already moved off — and `sync status` reported `active` throughout.
///
/// The launchd branch never had this bug, because `bootout` + `bootstrap` genuinely
/// replaces the process. `restart` is the systemd equivalent: it starts a stopped unit
/// and replaces a running one, which is what "install this and run it" has to mean on
/// both platforms.
///
/// `enable` stays separate from `restart` rather than collapsing into `enable --now`,
/// because the two answer different questions — "come back after a reboot" and "be
/// running now" — and `--no-start` needs the first without the second.
fn systemd_install_commands(start: bool) -> Vec<Vec<&'static str>> {
    let mut cmds = vec![
        vec!["--user", "daemon-reload"],
        vec!["--user", "enable", SYSTEMD_UNIT],
    ];
    if start {
        cmds.push(vec!["--user", "restart", SYSTEMD_UNIT]);
    }
    cmds
}

/// `ctxlake sync install` — render the unit and hand the daemon to the supervisor.
///
/// Idempotent: re-running against an unchanged host rewrites nothing and re-loads the
/// same unit, which is what makes it safe to call from `ctxlake init --daemon` and
/// again by hand.
pub async fn install(
    cfg: &Config,
    config_path: &Path,
    start: bool,
    skip_checks: bool,
) -> Result<()> {
    // Before writing a unit: prove this host can actually do the job. A supervised
    // daemon that cannot reach the store, or that is pointed at a model it cannot
    // call, comes up `active` and achieves nothing — and capture keeps working
    // regardless, because the hook only writes locally, so the first symptom is a
    // briefing going stale days later with nothing in `systemctl status` to explain
    // it. Refusing now is the whole point.
    if skip_checks {
        println!("skipping pre-install checks (--skip-checks)");
    } else {
        let report = crate::doctor::run(cfg)
            .await
            .context("running pre-install checks")?;
        let blockers = report.blocks_service_install();
        if !blockers.is_empty() {
            let mut msg = String::from("this host is not ready to run the sync daemon:\n");
            for b in &blockers {
                msg.push_str(&format!("  - {b}\n"));
            }
            msg.push_str(
                "\nRun `ctxlake doctor` for the full report. Fix these and re-run, or \
                 pass --skip-checks\nto install anyway (the daemon will start and fail \
                 at whatever you skipped).",
            );
            anyhow::bail!(msg);
        }
    }

    let manager = Manager::detect()?;
    let home = home_dir();
    let exec = exec_path()?;
    let log_dir = paths::sync_log_file(&cfg.fleet_id)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".ctxlake").join("run"));
    std::fs::create_dir_all(&log_dir).with_context(|| format!("creating {}", log_dir.display()))?;

    let config_path = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_owned());
    if !config_path.exists() {
        bail!(
            "no config at {} — the unit would start a daemon that exits 78 immediately; \
             run `ctxlake init` first",
            config_path.display()
        );
    }

    let unit_path = manager.unit_path(&home);
    // Captured before the write: it decides whether a daemon already running is one
    // the supervisor owns or a stray one this install has to clear out.
    let was_installed = unit_path.exists();
    let contents = render(
        manager,
        &exec,
        &config_path,
        &home,
        &log_dir,
        &service_path(),
    );
    let changed = write_unit(&unit_path, &contents)?;

    println!(
        "{} {} — {}",
        if changed { "wrote" } else { "unchanged" },
        unit_path.display(),
        manager.human_name()
    );

    // A daemonized `ctxlake sync start` from before this install would keep running
    // alongside the supervised one, both draining the same spool. Stop it first; the
    // supervisor is the single owner from here on.
    //
    // Only when there was no unit yet. On a re-install the running daemon IS the
    // supervised one, and SIGTERMing it is worse than pointless: it exits 0, which
    // both `Restart=on-failure` and `KeepAlive{SuccessfulExit=false}` correctly
    // decline to restart — so a plain `ctxlake sync install` would leave the service
    // installed, enabled, and dead. The bootout/bootstrap below is what replaces a
    // running supervised daemon.
    if !was_installed {
        if let Some(pid) = crate::sync_cmd::read_running_pid(&paths::pid_file(&cfg.fleet_id)) {
            println!("  stopping the unsupervised daemon already running (pid {pid})");
            crate::sync_cmd::stop(cfg)?;
        }
    }

    match manager {
        Manager::Systemd => {
            for args in systemd_install_commands(start) {
                run_checked("systemctl", &args)?;
            }
            report_linger();
        }
        Manager::Launchd => {
            let domain = gui_domain();
            let unit = unit_path.display().to_string();
            if start {
                // `launchd_replace` rather than a bare bootout + bootstrap: `bootout`
                // is asynchronous, and bootstrapping before it lands makes
                // `launchd_bootstrap` observe a job still mid-teardown and skip its
                // own work. See that function.
                //
                // `bootstrap` alone starts it: the plist sets `RunAtLoad`. An earlier
                // version also ran `kickstart -k`, which killed the process launchd
                // had just spawned and so tripped `ThrottleInterval` — making
                // `ctxlake init --daemon` block for 33 seconds on a clean install,
                // measured.
                launchd_replace(&unit)?;
            } else {
                // bootout so a re-install with --no-start leaves nothing running from
                // the previous unit. A never-loaded job makes this fail, which is why
                // its status is ignored.
                let _ = run("launchctl", &["bootout", &domain, &unit]);
            }
            // With `--no-start`, the plist is deliberately left un-bootstrapped:
            // bootstrapping it would honour `RunAtLoad` and start the daemon anyway.
            // It comes up at the next login.
        }
    }

    if start {
        report_started(cfg, "ctxlake sync is installed as a service and running");
    } else {
        println!("ctxlake sync is installed as a service (not started).");
    }
    println!(
        "  status: ctxlake sync status\n  logs:   {}",
        log_dir.display()
    );
    Ok(())
}

/// `gui/<uid>` — the launchd domain a LaunchAgent lives in.
fn gui_domain() -> String {
    // `id -u` rather than a `libc::getuid` call: the workspace carries no libc
    // dependency and AGENTS.md says not to add one without a reason, and shelling out
    // to a coreutil is the convention `sync_cmd.rs` already uses for `kill -0`.
    let uid = run("id", &["-u"])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    format!("gui/{uid}")
}

/// Block until the daemon has actually written its pidfile, or give up.
///
/// Both supervisors accept a start request and return success immediately, so
/// "started" is a statement about the *request*, not the process. launchd makes the
/// gap visible: `ThrottleInterval` holds a relaunch for up to 30 seconds, so
/// `ctxlake sync start && ctxlake sync status` reported "started" then "not running"
/// — measured on a real LaunchAgent, where the pid appeared at t+30s.
///
/// Lowering the throttle would trade away the thing it is for (a crash-looping daemon
/// filling the log), so the command waits and explains instead. systemd has no
/// equivalent delay on an explicit `start`, so this returns immediately there.
fn wait_for_daemon(cfg: &Config) -> Option<u32> {
    const EXPLAIN_AFTER: Duration = Duration::from_secs(2);
    const GIVE_UP_AFTER: Duration = Duration::from_secs(40);

    let pid_path = paths::pid_file(&cfg.fleet_id);
    let start = Instant::now();
    let mut explained = false;
    loop {
        if let Some(pid) = crate::sync_cmd::read_running_pid(&pid_path) {
            return Some(pid);
        }
        let waited = start.elapsed();
        if waited >= GIVE_UP_AFTER {
            return None;
        }
        if !explained && waited >= EXPLAIN_AFTER {
            explained = true;
            println!("  waiting — the supervisor throttles relaunches by up to 30s...");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Report what actually happened, rather than what was requested. `headline` is the
/// full success sentence minus the pid.
fn report_started(cfg: &Config, headline: &str) {
    match wait_for_daemon(cfg) {
        Some(pid) => println!("{headline} — pid {pid}"),
        None => println!(
            "{headline}, but no daemon appeared within 40s.\n  \
             Check `ctxlake sync status` and the log in {}",
            paths::sync_log_file(&cfg.fleet_id)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
    }
}

/// Load the LaunchAgent into this user's GUI domain, tolerating "already loaded."
///
/// `stop` boots the job *out* of the domain rather than merely signalling it — that
/// is the only way it stays stopped, because `KeepAlive` would otherwise relaunch it.
/// The cost is that after a `stop` there is no longer any such service for `kickstart`
/// to find, so `start` has to bootstrap it back first. Discovered by running the real
/// lifecycle: `stop` then `start` failed with *Could not find service
/// "com.oxidantdata.ctxlake-sync" in domain for user gui: 501*.
fn launchd_bootstrap(unit: &str) -> Result<()> {
    // Ask whether it is loaded rather than bootstrapping and interpreting the
    // failure. Bootstrapping an already-loaded job reports `Bootstrap failed: 5:
    // Input/output error`, which is indistinguishable from a dozen genuine problems
    // and is not the EEXIST an earlier version of this function guessed at. Checking
    // first is both correct and legible.
    if launchd_is_loaded() {
        return Ok(());
    }
    let domain = gui_domain();
    let out = run("launchctl", &["bootstrap", &domain, unit])?;
    if out.status.success() {
        return Ok(());
    }
    bail!(
        "`launchctl bootstrap {domain} {unit}` failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// Whether launchd currently has the agent in this user's GUI domain.
/// How long to wait for a booted-out job to actually disappear.
///
/// `launchctl bootout` returns before the job is gone. A tenth of a second per poll
/// over two seconds is far more than the teardown has ever taken, and the cost of
/// waiting slightly too long is nothing — whereas not waiting cost a real machine its
/// daemon for twenty minutes, with `ctxlake update` reporting a successful restart.
const BOOTOUT_SETTLE: Duration = Duration::from_secs(2);

/// Replace a loaded launchd job: boot it out, wait for that to take effect, bootstrap.
///
/// **The wait is the whole point.** `bootout` is asynchronous, and
/// [`launchd_bootstrap`] deliberately early-returns when the job is already loaded —
/// a check that is right for `install` (bootstrapping a loaded job reports a
/// misleading `Bootstrap failed: 5: Input/output error`) and exactly wrong here.
/// Called immediately after a bootout, it observed the job still mid-teardown,
/// concluded there was nothing to do, and returned success. The teardown then
/// finished, leaving the job booted out and never bootstrapped.
///
/// Observed on a real Mac: plist on disk, `launchctl` reporting no such job, no
/// process, no heartbeat for twenty minutes — after `ctxlake update` printed
/// "restarted".
fn launchd_replace(unit: &str) -> Result<()> {
    let _ = run("launchctl", &["bootout", &gui_domain(), unit]);
    wait_until_unloaded(BOOTOUT_SETTLE, launchd_is_loaded);
    launchd_bootstrap(unit)
}

/// Poll `is_loaded` until it reports false, or `timeout` elapses.
///
/// Returns whether the job actually went away. A timeout is not fatal on its own:
/// [`launchd_bootstrap`] still runs, and its own error is the one worth surfacing —
/// this only exists to stop that call being skipped by a stale observation.
///
/// `is_loaded` is injected so the polling logic is testable without launchd.
fn wait_until_unloaded(timeout: Duration, mut is_loaded: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_loaded() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn launchd_is_loaded() -> bool {
    run(
        "launchctl",
        &["print", &format!("{}/{LAUNCHD_LABEL}", gui_domain())],
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
}

fn report_linger() {
    match linger_enabled() {
        Some(true) => {}
        Some(false) => {
            let user = std::env::var("USER").unwrap_or_else(|_| "<user>".into());
            println!(
                "\n  WARNING: lingering is off for this user, so systemd will stop the \
                 daemon\n  at logout and NOT restart it after a reboot. Capture keeps \
                 working (the hook\n  only writes locally), so the first symptom is a \
                 briefing that quietly stops\n  updating. Fix:\n\n      sudo loginctl \
                 enable-linger {user}\n"
            );
        }
        None => println!(
            "  note: could not determine whether lingering is enabled; if this is a \
             headless\n  host, check `loginctl show-user $USER --property=Linger`"
        ),
    }
}

/// `ctxlake sync delete` — remove exactly what [`install`] added.
pub fn uninstall() -> Result<()> {
    let manager = Manager::detect()?;
    let home = home_dir();
    let unit_path = manager.unit_path(&home);
    if !unit_path.exists() {
        println!(
            "no ctxlake sync service installed ({})",
            unit_path.display()
        );
        return Ok(());
    }
    match manager {
        Manager::Systemd => {
            // Best-effort: a unit that is already stopped or was never enabled makes
            // these fail, and neither is a reason to refuse to delete the file.
            let _ = run("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
        }
        Manager::Launchd => {
            let _ = run(
                "launchctl",
                &["bootout", &gui_domain(), &unit_path.display().to_string()],
            );
        }
    }
    std::fs::remove_file(&unit_path)
        .with_context(|| format!("removing {}", unit_path.display()))?;
    if manager == Manager::Systemd {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        // Without this, a unit removed while in `failed` state stays in systemd's
        // list — `systemctl --user --failed` keeps reporting a ctxlake-sync.service
        // that no longer exists on disk. Observed after deleting the unit on a real
        // host.
        let _ = run("systemctl", &["--user", "reset-failed", SYSTEMD_UNIT]);
    }
    println!("removed {}", unit_path.display());
    Ok(())
}

/// `ctxlake sync start`. Delegates to the supervisor when one is installed, and
/// otherwise keeps the pre-existing re-exec behaviour so nothing regresses for a user
/// who never ran `install`.
pub fn start(
    cfg: &Config,
    config_path: &Path,
    runtime: Option<crate::hooks::Runtime>,
) -> Result<()> {
    if !is_installed() {
        return crate::sync_cmd::run_background(cfg, config_path, runtime);
    }
    match Manager::detect()? {
        Manager::Systemd => run_checked("systemctl", &["--user", "start", SYSTEMD_UNIT])?,
        Manager::Launchd => {
            let unit = Manager::Launchd
                .unit_path(&home_dir())
                .display()
                .to_string();
            launchd_bootstrap(&unit)?;
            run_checked(
                "launchctl",
                &["kickstart", &format!("{}/{LAUNCHD_LABEL}", gui_domain())],
            )?
        }
    }
    report_started(cfg, "ctxlake sync started (service)");
    Ok(())
}

/// `ctxlake sync stop`.
pub fn stop(cfg: &Config) -> Result<()> {
    if !is_installed() {
        return crate::sync_cmd::stop(cfg);
    }
    match Manager::detect()? {
        Manager::Systemd => run_checked("systemctl", &["--user", "stop", SYSTEMD_UNIT])?,
        // `bootout` unloads the job entirely, which is what makes it stay stopped
        // despite `KeepAlive`. The plist stays on disk, so `start` bootstraps it back.
        Manager::Launchd => {
            let unit = Manager::Launchd
                .unit_path(&home_dir())
                .display()
                .to_string();
            run_checked("launchctl", &["bootout", &gui_domain(), &unit])?;
        }
    }
    println!("ctxlake sync stopped (service)");
    Ok(())
}

/// `ctxlake sync restart`.
pub fn restart(
    cfg: &Config,
    config_path: &Path,
    runtime: Option<crate::hooks::Runtime>,
) -> Result<()> {
    if !is_installed() {
        crate::sync_cmd::stop(cfg)?;
        return crate::sync_cmd::run_background(cfg, config_path, runtime);
    }
    match Manager::detect()? {
        Manager::Systemd => run_checked("systemctl", &["--user", "restart", SYSTEMD_UNIT])?,
        Manager::Launchd => {
            // bootout + bootstrap, not `kickstart -k`. Both restart the daemon, but
            // `-k` kills a running process and is therefore subject to
            // `ThrottleInterval` — up to 30 seconds of nothing. Unloading and
            // reloading the job is not, and lands in well under a second.
            let unit = Manager::Launchd
                .unit_path(&home_dir())
                .display()
                .to_string();
            launchd_replace(&unit)?
        }
    }
    report_started(cfg, "ctxlake sync restarted (service)");
    Ok(())
}

/// Restart the supervised daemon, without needing a `Config` or a config path.
///
/// `ctxlake update` calls this after replacing the binary: a running daemon holds the
/// *old* file open (which is exactly what makes replacing it safe), so it keeps
/// running the previous version until something restarts it. Split out of [`restart`]
/// because that function's unsupervised branch re-launches the daemon itself and
/// needs a config to do it — here there is definitionally a unit, and the unit
/// already names its own config.
pub fn restart_installed() -> Result<()> {
    match Manager::detect()? {
        Manager::Systemd => run_checked("systemctl", &["--user", "restart", SYSTEMD_UNIT])?,
        Manager::Launchd => {
            let unit = Manager::Launchd
                .unit_path(&home_dir())
                .display()
                .to_string();
            launchd_replace(&unit)?
        }
    }
    Ok(())
}

/// `ctxlake sync status` — the service manager's view *and* the pidfile's.
///
/// Both are printed because they can disagree, and the disagreement is the
/// interesting case: a unit reporting `active` while no pidfile exists means the
/// daemon is crash-looping faster than it can write one.
pub fn status(cfg: &Config) -> Result<()> {
    let manager = Manager::detect().ok();
    match manager {
        Some(m) if m.unit_path(&home_dir()).exists() => {
            let unit_path = m.unit_path(&home_dir());
            println!("service:  installed — {}", unit_path.display());
            match m {
                Manager::Systemd => {
                    let out = run("systemctl", &["--user", "is-active", SYSTEMD_UNIT])?;
                    println!("state:    {}", String::from_utf8_lossy(&out.stdout).trim());
                    match linger_enabled() {
                        Some(true) => println!("lingering: yes — survives reboot"),
                        Some(false) => println!(
                            "lingering: NO — will not restart after reboot \
                             (sudo loginctl enable-linger $USER)"
                        ),
                        None => println!("lingering: unknown"),
                    }
                }
                Manager::Launchd => {
                    let out = run(
                        "launchctl",
                        &["print", &format!("{}/{LAUNCHD_LABEL}", gui_domain())],
                    )?;
                    if out.status.success() {
                        let text = String::from_utf8_lossy(&out.stdout);
                        let state = text
                            .lines()
                            .find_map(|l| l.trim().strip_prefix("state = "))
                            .unwrap_or("loaded");
                        println!("state:    {}", state.trim());
                    } else {
                        println!("state:    not loaded");
                    }
                }
            }
        }
        _ => println!("service:  not installed (run `ctxlake sync install` to survive reboots)"),
    }
    crate::sync_cmd::status(cfg)?;

    // The part that was missing, and that cost two machines a debugging session:
    // "state: activating" plus "ctxlake sync is not running" is a description, not a
    // diagnosis. Both supervisors had the reason — one line, in a file — and neither
    // status command mentioned that the file existed.
    if let Some(m) = manager {
        if m.unit_path(&home_dir()).exists()
            && crate::sync_cmd::read_running_pid(&paths::pid_file(&cfg.fleet_id)).is_none()
        {
            print_why_not_running(m, cfg);
        }
    }
    Ok(())
}

/// How many lines of the daemon's own error output to show.
///
/// Enough to carry a panic's message and a line of context, short enough that it does
/// not bury the status report it is appended to. A crash loop repeats the same line,
/// so more would mostly be the same error again.
const LOG_TAIL_LINES: usize = 6;

/// Why the supervisor has a unit installed and no daemon running.
fn print_why_not_running(manager: Manager, cfg: &Config) {
    println!("\nwhy:      the service is installed but no daemon is running.");
    let lines = match manager {
        // launchd redirects the job's stderr to a file the plist names, so the reason
        // is sitting there whether or not anyone knew to look.
        Manager::Launchd => {
            let err_log = paths::sync_log_file(&cfg.fleet_id)
                .parent()
                .map(|d| d.join("sync.err.log"));
            match err_log {
                Some(path) => {
                    let tail = tail_lines(&path, LOG_TAIL_LINES);
                    if !tail.is_empty() {
                        println!("          last output — {}:", path.display());
                    }
                    tail
                }
                None => Vec::new(),
            }
        }
        // A systemd unit logs to the journal, so there is no file to tail.
        Manager::Systemd => {
            let out = run(
                "journalctl",
                &[
                    "--user",
                    "-u",
                    SYSTEMD_UNIT,
                    "-n",
                    "20",
                    "--no-pager",
                    "-p",
                    "warning",
                    "-o",
                    "cat",
                ],
            );
            match out {
                Ok(o) if o.status.success() => {
                    let text = String::from_utf8_lossy(&o.stdout);
                    let tail: Vec<String> = text
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .rev()
                        .take(LOG_TAIL_LINES)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    if !tail.is_empty() {
                        println!("          last output — journalctl --user -u {SYSTEMD_UNIT}:");
                    }
                    tail
                }
                _ => Vec::new(),
            }
        }
    };

    for line in &lines {
        println!("            {line}");
    }
    if lines.is_empty() {
        match manager {
            Manager::Launchd => println!(
                "          no output captured yet. Full log: {}",
                paths::sync_log_file(&cfg.fleet_id)
                    .parent()
                    .map(|d| d.join("sync.err.log").display().to_string())
                    .unwrap_or_default()
            ),
            Manager::Systemd => println!(
                "          nothing in the journal yet: journalctl --user -u {SYSTEMD_UNIT} -f"
            ),
        }
    }
    println!("          `ctxlake doctor` checks the things that usually cause this.");
}

/// The last `n` non-empty lines of a file, or nothing if it cannot be read.
///
/// Reads the whole file, which is fine for a log a supervisor rotates and which only
/// ever holds this daemon's own stderr — and much less code than seeking backwards
/// for a diagnostic that runs when something is already wrong.
fn tail_lines(path: &Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(n)
        .map(str::to_string)
        .collect();
    lines.reverse();
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `render` with a fixed PATH, for the assertions that are not about PATH.
    ///
    /// A real `service_path()` reads the test process's own environment, which makes
    /// every unrelated assertion depend on whatever CI happens to have on PATH.
    fn render_t(
        manager: Manager,
        exec: &Path,
        config_path: &Path,
        home: &Path,
        log_dir: &Path,
    ) -> String {
        render(manager, exec, config_path, home, log_dir, "/usr/bin:/bin")
    }

    #[test]
    fn the_systemd_unit_leaves_no_placeholder_behind() {
        let out = render_t(
            Manager::Systemd,
            Path::new("/usr/local/bin/ctxlake"),
            Path::new("/home/alice/.config/ctxlake/ctxlake.toml"),
            Path::new("/home/alice"),
            Path::new("/home/alice/.ctxlake/run"),
        );
        assert!(!out.contains("{{"), "unsubstituted placeholder:\n{out}");
        assert!(out.contains(
            r#"ExecStart="/usr/local/bin/ctxlake" --config "/home/alice/.config/ctxlake/ctxlake.toml" sync run --foreground"#
        ), "got:\n{out}");
    }

    #[test]
    fn the_launchd_plist_leaves_no_placeholder_behind() {
        let out = render_t(
            Manager::Launchd,
            Path::new("/opt/homebrew/bin/ctxlake"),
            Path::new("/Users/alice/.config/ctxlake/ctxlake.toml"),
            Path::new("/Users/alice"),
            Path::new("/Users/alice/.ctxlake/run"),
        );
        assert!(!out.contains("{{"), "unsubstituted placeholder:\n{out}");
        assert!(out.contains("<string>/opt/homebrew/bin/ctxlake</string>"));
        assert!(out.contains("<string>--config</string>"));
        assert!(out.contains("/Users/alice/.ctxlake/run/sync.log"));
    }

    #[test]
    fn the_unit_always_runs_the_daemon_in_the_foreground() {
        // Both supervisors watch the process they spawn. If the unit ever invoked
        // the daemonizing form, the supervised process would exit immediately after
        // forking, systemd would call that a failure and restart it forever, and
        // launchd's KeepAlive would do the same — a restart loop that still, somehow,
        // leaves a working daemon running unsupervised. Pin it.
        for m in [Manager::Systemd, Manager::Launchd] {
            let out = render_t(
                m,
                Path::new("/bin/ctxlake"),
                Path::new("/c/ctxlake.toml"),
                Path::new("/h"),
                Path::new("/l"),
            );
            assert!(
                out.contains("--foreground"),
                "{m:?} must supervise directly"
            );
        }
    }

    #[test]
    fn a_clean_exit_must_not_be_restarted_by_either_supervisor() {
        // `ctxlake sync stop` exits 0. `Restart=always` or a bare `KeepAlive=true`
        // would bring the daemon straight back, making the stop command a no-op that
        // reports success — the worst kind of bug to debug.
        let sd = render_t(
            Manager::Systemd,
            Path::new("/b"),
            Path::new("/c"),
            Path::new("/h"),
            Path::new("/l"),
        );
        assert!(sd.contains("Restart=on-failure"));
        assert!(!sd.contains("Restart=always"));

        let ld = render_t(
            Manager::Launchd,
            Path::new("/b"),
            Path::new("/c"),
            Path::new("/h"),
            Path::new("/l"),
        );
        assert!(ld.contains("<key>SuccessfulExit</key>"), "got:\n{ld}");
    }

    #[test]
    fn a_config_error_is_never_restarted_into_a_loop() {
        // Pairs with main.rs's EX_CONFIG exit. If either half moves, systemd
        // restarts a daemon whose ctxlake.toml is missing, five seconds apart,
        // forever.
        let sd = render_t(
            Manager::Systemd,
            Path::new("/b"),
            Path::new("/c"),
            Path::new("/h"),
            Path::new("/l"),
        );
        assert!(
            sd.contains(&format!("RestartPreventExitStatus={}", crate::EX_CONFIG)),
            "the unit must name the same exit code main.rs uses, got:\n{sd}"
        );
    }

    #[test]
    fn an_ampersand_in_a_path_cannot_break_the_plist() {
        let out = render_t(
            Manager::Launchd,
            Path::new("/Users/a&b/bin/ctxlake"),
            Path::new("/Users/a&b/ctxlake.toml"),
            Path::new("/Users/a&b"),
            Path::new("/Users/a&b/run"),
        );
        assert!(out.contains("/Users/a&amp;b/bin/ctxlake"), "got:\n{out}");
        assert!(!out.contains("a&b"), "raw ampersand left in XML:\n{out}");
    }

    #[test]
    fn both_units_pin_home_rather_than_inheriting_it() {
        // Found by actually installing the LaunchAgent: launchd hands the job its own
        // $HOME from the user record, not the one the installer ran under. Every local
        // path ctxlake resolves — spool, cache, pidfile — hangs off $HOME, so the
        // daemon came up healthy, drained a spool nothing was writing to, and reported
        // itself as not running because its pidfile was somewhere else entirely.
        let sd = render_t(
            Manager::Systemd,
            Path::new("/b"),
            Path::new("/c"),
            Path::new("/home/alice"),
            Path::new("/l"),
        );
        assert!(
            sd.contains(r#"Environment="HOME=/home/alice""#),
            "got:\n{sd}"
        );

        let ld = render_t(
            Manager::Launchd,
            Path::new("/b"),
            Path::new("/c"),
            Path::new("/Users/alice"),
            Path::new("/l"),
        );
        assert!(
            ld.contains("<key>HOME</key><string>/Users/alice</string>"),
            "got:\n{ld}"
        );
    }

    #[test]
    fn both_units_pin_a_path_rather_than_inheriting_the_supervisors() {
        // Found by a real install on two machines. `provider = "claude-cli"` passed
        // `ctxlake doctor` and `sync install` in an interactive shell, then the daemon
        // failed with "`claude` is not on PATH" on every start — fourteen restarts on
        // macOS — because launchd's PATH is /usr/bin:/bin:/usr/sbin:/sbin and systemd's
        // user PATH is no better. The pre-install check and the checked thing were
        // running in different environments, so a green check proved nothing.
        let sd = render(
            Manager::Systemd,
            Path::new("/b"),
            Path::new("/c"),
            Path::new("/home/alice"),
            Path::new("/l"),
            "/opt/tools/bin:/usr/bin",
        );
        assert!(
            sd.contains(r#"Environment="PATH=/opt/tools/bin:/usr/bin""#),
            "got:\n{sd}"
        );

        let ld = render(
            Manager::Launchd,
            Path::new("/b"),
            Path::new("/c"),
            Path::new("/Users/alice"),
            Path::new("/l"),
            "/opt/homebrew/bin:/usr/bin",
        );
        assert!(
            ld.contains("<key>PATH</key><string>/opt/homebrew/bin:/usr/bin</string>"),
            "got:\n{ld}"
        );
    }

    #[test]
    fn the_pinned_path_carries_the_installing_shells_directories() {
        // The whole point: the directory `claude` was found in during the pre-install
        // checks has to still be on PATH when the daemon looks for it.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("tools");
        std::fs::create_dir_all(&real).unwrap();
        let real_s = real.to_str().unwrap().to_string();

        let got = service_path_from(Some(std::ffi::OsStr::new(&format!(
            "{real_s}:/definitely/not/here:relative/dir:"
        ))));
        let dirs: Vec<&str> = got.split(':').collect();

        assert!(
            dirs.contains(&real_s.as_str()),
            "the installing shell's directory must survive: {got}"
        );
        assert!(
            !got.contains("/definitely/not/here"),
            "a PATH entry that does not exist is junk to enshrine in a unit: {got}"
        );
        assert!(
            !got.contains("relative/dir"),
            "a relative PATH entry means nothing to a supervisor: {got}"
        );
        assert!(
            dirs.iter().all(|d| !d.is_empty()),
            "an empty entry means `.` to some shells — never in a unit: {got}"
        );
        for fallback in FALLBACK_PATH_DIRS {
            if std::path::Path::new(fallback).is_dir() {
                assert!(
                    dirs.contains(fallback),
                    "{fallback} must be appended so the unit works from an empty PATH: {got}"
                );
            }
        }
    }

    #[test]
    fn a_pinned_path_never_repeats_a_directory() {
        // `Environment="PATH=..."` with duplicates is harmless but reads as a bug in
        // a file an operator is going to open while debugging.
        let got = service_path_from(Some(std::ffi::OsStr::new("/usr/bin:/usr/bin/:/bin")));
        let dirs: Vec<&str> = got.split(':').collect();
        let mut uniq = dirs.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(dirs.len(), uniq.len(), "duplicate entries: {got}");
    }

    #[test]
    fn the_unit_names_a_path_that_survives_an_upgrade() {
        // `exec_path` used to canonicalize, which on Homebrew pins
        // /opt/homebrew/Cellar/ctxlake/<version>/bin/ctxlake. `brew upgrade` deletes
        // that directory, so a unit installed before an upgrade names a file that no
        // longer exists and the daemon crash-loops with nothing saying why. The
        // symlink in <prefix>/bin is the stable name — brew repoints it rather than
        // removing it, and the worst a mid-flight upgrade costs is one restart the
        // supervisor was going to do anyway.
        //
        // Built here as a real symlink into a real version-stamped directory, because
        // that is the only shape where canonicalizing and not canonicalizing differ.
        let dir = tempfile::tempdir().unwrap();
        let cellar = dir.path().join("Cellar/ctxlake/0.1.5/bin");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&cellar).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(cellar.join("ctxlake"), "#!/bin/sh\n").unwrap();
        let link = bin.join("ctxlake");
        std::os::unix::fs::symlink(cellar.join("ctxlake"), &link).unwrap();

        let got = stable_exec_path(&link);
        assert_eq!(
            got, link,
            "the unit must name the stable symlink, not the version-stamped target"
        );
        assert!(
            !got.display().to_string().contains("/Cellar/"),
            "a version-stamped install path disappears on the next upgrade: {got:?}"
        );
    }

    #[test]
    fn the_log_tail_shows_the_last_lines_and_drops_the_blank_ones() {
        // `sync status` reporting "activating" / "not running" and nothing else is
        // what turned a one-line error into a debugging session on two machines. The
        // reason was already in this file; nothing pointed at it.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sync.err.log");
        std::fs::write(&f, "one\n\ntwo\nthree\n\nfour\n").unwrap();

        assert_eq!(tail_lines(&f, 2), vec!["three", "four"]);
        assert_eq!(
            tail_lines(&f, 99),
            vec!["one", "two", "three", "four"],
            "asking for more lines than exist must return what there is, in order"
        );
        assert!(
            tail_lines(&dir.path().join("absent"), 5).is_empty(),
            "a missing log is the normal case before a first run, not an error"
        );
    }

    #[test]
    fn the_config_path_survives_a_round_trip_through_either_unit() {
        // `ctxlake update` re-renders an installed unit, and the unit is the only
        // place that records which config the daemon was installed for — it is not
        // necessarily the default, and `update` loads no config of its own.
        for (m, home) in [
            (Manager::Systemd, "/home/alice"),
            (Manager::Launchd, "/Users/alice"),
        ] {
            let cfg = format!("{home}/.config/ctxlake/ctxlake.toml");
            let rendered = render_t(
                m,
                Path::new("/usr/local/bin/ctxlake"),
                Path::new(&cfg),
                Path::new(home),
                Path::new("/l"),
            );
            assert_eq!(
                config_path_from_unit(&rendered),
                Some(PathBuf::from(&cfg)),
                "{m:?} unit must give its config path back"
            );
        }
    }

    #[test]
    fn an_escaped_path_comes_back_unescaped() {
        // A home directory containing `&` is legal, and the plist stores it as
        // `&amp;`. Reading it back raw would hand the re-render a path that does not
        // exist, and the unit would be rewritten pointing at nothing.
        let cfg = "/Users/a&b/.config/ctxlake/ctxlake.toml";
        let rendered = render_t(
            Manager::Launchd,
            Path::new("/usr/local/bin/ctxlake"),
            Path::new(cfg),
            Path::new("/Users/a&b"),
            Path::new("/l"),
        );
        assert!(rendered.contains("a&amp;b"), "fixture must actually escape");
        assert_eq!(config_path_from_unit(&rendered), Some(PathBuf::from(cfg)));
    }

    #[test]
    fn a_unit_with_no_config_path_is_reported_rather_than_guessed() {
        // Guessing the default here would silently repoint a daemon that was
        // deliberately installed against a different config — one host running two
        // agent identities is a documented setup.
        assert_eq!(config_path_from_unit("nothing like a unit file"), None);
        assert_eq!(config_path_from_unit("<string>--config</string>"), None);
    }

    #[test]
    fn installing_over_a_running_systemd_daemon_replaces_it() {
        // The bug, on a real host mid-upgrade: `sync install` rewrote the unit with a
        // new PATH, the installer replaced the binary on disk, and the daemon kept
        // running the old one for an hour — `/proc/<pid>/exe` pointing at a path
        // marked `(deleted)`, `sync status` reporting `active` the whole time, and the
        // host writing to a `live/` layout the rest of the fleet had moved off.
        //
        // `enable --now` cannot fix that: `--now` means *start*, and starting an
        // already-active unit does nothing.
        let cmds = systemd_install_commands(true);
        assert!(
            cmds.iter().any(|c| c.contains(&"restart")),
            "install must replace a running daemon, not no-op on it: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains(&"--now")),
            "`enable --now` is a no-op against a running unit: {cmds:?}"
        );
        // And it must still survive a reboot.
        assert!(
            cmds.iter().any(|c| c.contains(&"enable")),
            "install must still enable the unit: {cmds:?}"
        );
        // The unit file has to be re-read before anything acts on it.
        assert_eq!(
            cmds.first().map(|c| c.as_slice()),
            Some(["--user", "daemon-reload"].as_slice()),
            "systemd must reload the rewritten unit first: {cmds:?}"
        );
    }

    #[test]
    fn no_start_enables_without_starting() {
        // `--no-start` means "be there after a reboot, but do not run now" — the
        // air-gapped / staged-rollout case. A `restart` here would defeat it.
        let cmds = systemd_install_commands(false);
        assert!(cmds.iter().any(|c| c.contains(&"enable")), "{cmds:?}");
        assert!(
            !cmds
                .iter()
                .any(|c| c.contains(&"restart") || c.contains(&"start")),
            "--no-start must not start the daemon: {cmds:?}"
        );
    }

    #[test]
    fn units_are_user_scoped_not_system_scoped() {
        let home = Path::new("/home/alice");
        assert_eq!(
            Manager::Systemd.unit_path(home),
            home.join(".config/systemd/user/ctxlake-sync.service")
        );
        assert_eq!(
            Manager::Launchd.unit_path(Path::new("/Users/alice")),
            Path::new("/Users/alice/Library/LaunchAgents/com.oxidantdata.ctxlake-sync.plist")
        );
        // Never /etc/systemd/system or /Library/LaunchDaemons: those run as root and
        // would read neither this user's spool nor this user's store credentials.
        for p in [
            Manager::Systemd.unit_path(home),
            Manager::Launchd.unit_path(home),
        ] {
            assert!(p.starts_with(home), "{p:?} escaped the user's home");
        }
    }

    #[test]
    fn linger_is_parsed_from_loginctls_actual_output_shape() {
        assert_eq!(parse_linger("Linger=yes\n"), Some(true));
        assert_eq!(parse_linger("Linger=no\n"), Some(false));
        assert_eq!(parse_linger(""), None);
        // Anything unrecognized must read as "not lingering" rather than silently
        // suppressing the warning this check exists to print.
        assert_eq!(parse_linger("Linger=\n"), Some(false));
    }

    #[test]
    fn rewriting_an_identical_unit_reports_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested").join("unit.service");
        assert!(write_unit(&p, "a").unwrap(), "first write is a change");
        assert!(!write_unit(&p, "a").unwrap(), "identical rewrite is not");
        assert!(write_unit(&p, "b").unwrap(), "different content is");
    }
}
