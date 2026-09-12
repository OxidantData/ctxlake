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

/// Substitute the template placeholders. Pure, so the rendered output is testable
/// without writing to `$HOME` or shelling out to a service manager.
pub fn render(
    manager: Manager,
    exec: &Path,
    config_path: &Path,
    home: &Path,
    log_dir: &Path,
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
            .replace("{{HOME}}", &home_s),
        Manager::Launchd => LAUNCHD_TEMPLATE
            .replace("{{EXEC}}", &xml_escape(&exec))
            .replace("{{CONFIG}}", &xml_escape(&config))
            .replace("{{HOME}}", &xml_escape(&home_s))
            .replace("{{LOG_DIR}}", &xml_escape(&log)),
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The absolute path to write into the unit's `ExecStart`.
///
/// Canonicalized because `current_exe` can hand back a symlink — which is exactly
/// what Homebrew installs into `/opt/homebrew/bin` — and a unit pointing at a symlink
/// breaks the moment `brew upgrade` repoints it mid-flight.
fn exec_path() -> Result<PathBuf> {
    let exe =
        std::env::current_exe().context("locating the ctxlake binary for the service unit")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
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

/// `ctxlake sync install` — render the unit and hand the daemon to the supervisor.
///
/// Idempotent: re-running against an unchanged host rewrites nothing and re-loads the
/// same unit, which is what makes it safe to call from `ctxlake init --daemon` and
/// again by hand.
pub fn install(cfg: &Config, config_path: &Path, start: bool) -> Result<()> {
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
    let contents = render(manager, &exec, &config_path, &home, &log_dir);
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
            run_checked("systemctl", &["--user", "daemon-reload"])?;
            let enable: &[&str] = if start {
                &["--user", "enable", "--now", SYSTEMD_UNIT]
            } else {
                &["--user", "enable", SYSTEMD_UNIT]
            };
            run_checked("systemctl", enable)?;
            report_linger();
        }
        Manager::Launchd => {
            let domain = gui_domain();
            let unit = unit_path.display().to_string();
            // bootout first so a re-install replaces a loaded job rather than failing.
            // A never-loaded job makes this fail, which is why its status is ignored.
            let _ = run("launchctl", &["bootout", &domain, &unit]);
            if start {
                // `bootstrap` alone starts it: the plist sets `RunAtLoad`. An earlier
                // version also ran `kickstart -k`, which killed the process launchd
                // had just spawned and so tripped `ThrottleInterval` — making
                // `ctxlake init --daemon` block for 33 seconds on a clean install,
                // measured.
                launchd_bootstrap(&unit)?;
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
            let _ = run("launchctl", &["bootout", &gui_domain(), &unit]);
            launchd_bootstrap(&unit)?
        }
    }
    report_started(cfg, "ctxlake sync restarted (service)");
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
    crate::sync_cmd::status(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_systemd_unit_leaves_no_placeholder_behind() {
        let out = render(
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
        let out = render(
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
            let out = render(
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
        let sd = render(
            Manager::Systemd,
            Path::new("/b"),
            Path::new("/c"),
            Path::new("/h"),
            Path::new("/l"),
        );
        assert!(sd.contains("Restart=on-failure"));
        assert!(!sd.contains("Restart=always"));

        let ld = render(
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
        let sd = render(
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
        let out = render(
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
        let sd = render(
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

        let ld = render(
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
