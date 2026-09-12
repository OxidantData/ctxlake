//! `ctxlake sync` — run, check, and stop the daemon behind `ctxlake-sync`'s three
//! loops (upload, cache, presence — see that crate's module doc).
//!
//! `--foreground` runs attached to the invoking process, catches SIGTERM/SIGINT,
//! and shuts the daemon down cleanly before exiting — this is what tests drive
//! directly and what a `systemd`/`launchd` unit should invoke in production. The
//! bare form daemonizes by re-exec'ing itself with `--foreground` as a detached
//! child: its own process group (so a `Ctrl-C` on the launching terminal doesn't
//! also kill it), stdio redirected to a log file, pidfile written by the child
//! itself once it's actually running.
//!
//! **Honesty about the daemonization.** This is not a true double-forked Unix
//! daemon — there is no `setsid`, because the workspace carries no `libc`
//! dependency to call it with (AGENTS.md: "don't add a dependency without saying
//! why" cuts against pulling one in just for this), so the child can still receive
//! a `SIGHUP` if its controlling terminal's session ends. A `systemd`/`launchd`
//! unit running `ctxlake sync --foreground` directly is the robust option for a
//! production host; docs/reference.md says so.
//!
//! Resolves spool and cache roots through `paths.rs` (a thin wrapper over
//! `ctxlake_core::paths`) — never independently, per AGENTS.md's hard-won-facts
//! section on exactly that class of bug. [`run_foreground_until`] takes both as
//! explicit parameters rather than re-reading the environment internally: the one
//! caller that needs a different root — this module's own test — can then inject
//! a tempdir directly instead of mutating `$CTXLAKE_SPOOL_DIR`/`$CTXLAKE_CACHE_DIR`
//! for the whole process, which every other test in this crate that touches those
//! same env vars (`doctor.rs`'s spool-backlog checks, notably) would otherwise be
//! exposed to under `cargo test`'s default parallelism — the exact flakiness
//! `paths.rs`'s own module doc warns against.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use ctxlake_core::Runtime as CoreRuntime;
use ctxlake_sync::{Daemon, DaemonConfig};

use crate::config::Config;
use crate::hooks::Runtime as HookRuntime;
use crate::paths;
use crate::repo::detect_repo;
use crate::store_ctx;

fn map_runtime(runtime: Option<HookRuntime>) -> CoreRuntime {
    match runtime {
        Some(HookRuntime::ClaudeCode) => CoreRuntime::ClaudeCode,
        Some(HookRuntime::Cursor) => CoreRuntime::Cursor,
        Some(HookRuntime::Hermes) => CoreRuntime::Hermes,
        // A sync daemon uploads every runtime's spool regardless of this value —
        // it only labels this daemon's own `live/agents/<id>.json` heartbeat, so
        // "unspecified" degrading to `Other` costs nothing but a slightly less
        // informative `ctxlake status` row.
        None => CoreRuntime::Other,
    }
}

/// Poll interval overrides, read once at startup — millisecond env vars rather
/// than CLI flags because their only real consumer is this module's own test
/// (driving the daemon through real, but fast, ticks; production defaults tick in
/// the tens of seconds, per `DaemonConfig::with_defaults`). No other code in this
/// crate reads these three specific names, so — unlike the spool/cache roots
/// above — using env vars here carries none of this module's own doc's
/// cross-test flakiness risk.
fn interval_override(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

fn build_daemon_config(
    cfg: &Config,
    runtime: Option<HookRuntime>,
    spool_root: PathBuf,
    cache_root: PathBuf,
) -> DaemonConfig {
    let mut daemon_cfg = DaemonConfig::with_defaults(
        cfg.fleet_id.clone(),
        cfg.agent_id.clone(),
        map_runtime(runtime),
        spool_root,
        cache_root,
    );
    daemon_cfg.repo = Some(detect_repo());
    daemon_cfg.upload_poll_interval = interval_override(
        "CTXLAKE_SYNC_UPLOAD_INTERVAL_MS",
        daemon_cfg.upload_poll_interval,
    );
    daemon_cfg.cache_poll_interval = interval_override(
        "CTXLAKE_SYNC_CACHE_INTERVAL_MS",
        daemon_cfg.cache_poll_interval,
    );
    daemon_cfg.presence_poll_interval = interval_override(
        "CTXLAKE_SYNC_PRESENCE_INTERVAL_MS",
        daemon_cfg.presence_poll_interval,
    );
    daemon_cfg
}

fn write_pid_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, std::process::id().to_string())
        .with_context(|| format!("writing pidfile {}", path.display()))
}

/// Run the daemon attached to this process, against `spool_root`/`cache_root`,
/// until `shutdown` resolves — then shut down cleanly (see `ctxlake-sync`'s own
/// `Daemon::shutdown` doc) and remove the pidfile. Split out from [`run_foreground`] so a test can inject both an
/// isolated pair of directories and a deterministic shutdown trigger instead of
/// mutating global environment state or sending a real OS signal to the whole
/// test process.
pub async fn run_foreground_until(
    cfg: &Config,
    runtime: Option<HookRuntime>,
    spool_root: PathBuf,
    cache_root: PathBuf,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;
    let store = store_ctx::prefixed_store(&ctx);
    let daemon_cfg = build_daemon_config(cfg, runtime, spool_root, cache_root);

    let pid_path = paths::pid_file(&cfg.fleet_id);
    write_pid_file(&pid_path)?;

    let daemon = Daemon::spawn(store, ctx.clock.clone(), daemon_cfg);
    shutdown.await;
    daemon.shutdown().await;

    let _ = std::fs::remove_file(&pid_path);
    Ok(())
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    // A handler that fails to install means no signal support at all — a
    // startup-time environment problem worth crashing loudly for, rather than
    // silently running a daemon `ctxlake sync --stop` could never actually stop.
    let mut term = signal(SignalKind::terminate()).expect("failed to install a SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// `ctxlake sync --foreground`: the real roots, blocking on a real OS signal.
pub async fn run_foreground(cfg: &Config, runtime: Option<HookRuntime>) -> Result<()> {
    run_foreground_until(
        cfg,
        runtime,
        paths::spool_root(),
        ctxlake_core::paths::cache_root(),
        wait_for_shutdown_signal(),
    )
    .await
}

fn process_alive(pid: u32) -> bool {
    // `kill -0` sends no signal, just checks existence/permission — the same
    // shell-out-to-a-real-tool convention `repo::detect_repo` already uses
    // for `git`, rather than adding a `libc` dependency for one syscall.
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn send_signal(pid: u32, sig: &str) -> Result<()> {
    let status = std::process::Command::new("kill")
        .arg(sig)
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("running kill {sig} {pid}"))?;
    if !status.success() {
        anyhow::bail!("kill {sig} {pid} failed");
    }
    Ok(())
}

/// The pid in `pid_path`, but only if that process is actually still alive — a
/// pidfile left behind by a process that crashed without cleaning up must read as
/// "not running," not as a phantom daemon nothing can ever stop.
pub(crate) fn read_running_pid(pid_path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(pid_path).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    process_alive(pid).then_some(pid)
}

pub fn status(cfg: &Config) -> Result<()> {
    let pid_path = paths::pid_file(&cfg.fleet_id);
    match read_running_pid(&pid_path) {
        Some(pid) => println!(
            "ctxlake sync is running (pid {pid}) — {}",
            pid_path.display()
        ),
        None => println!("ctxlake sync is not running"),
    }
    Ok(())
}

pub fn stop(cfg: &Config) -> Result<()> {
    let pid_path = paths::pid_file(&cfg.fleet_id);
    let Some(pid) = read_running_pid(&pid_path) else {
        println!("ctxlake sync is not running");
        return Ok(());
    };
    send_signal(pid, "-TERM")?;
    for _ in 0..50 {
        if !process_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if process_alive(pid) {
        println!("ctxlake sync (pid {pid}) did not stop within 5s — it may still be shutting down");
    } else {
        println!("ctxlake sync (pid {pid}) stopped");
        // Best-effort: the process removes its own pidfile on a clean
        // `run_foreground_until` exit; this covers the case where it didn't
        // (killed harder than SIGTERM would ever need to, say).
        let _ = std::fs::remove_file(&pid_path);
    }
    Ok(())
}

/// `ctxlake sync` with neither `--foreground` nor `--status`/`--stop`: daemonize.
pub fn run_background(
    cfg: &Config,
    config_path: &Path,
    runtime: Option<HookRuntime>,
) -> Result<()> {
    let pid_path = paths::pid_file(&cfg.fleet_id);
    if let Some(pid) = read_running_pid(&pid_path) {
        println!("ctxlake sync already running (pid {pid})");
        return Ok(());
    }

    let exe = std::env::current_exe()
        .context("locating the ctxlake binary to relaunch in the background")?;
    let log_path = paths::sync_log_file(&cfg.fleet_id);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let log_err = log_out
        .try_clone()
        .with_context(|| format!("cloning a handle to {}", log_path.display()))?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--config").arg(config_path);
    cmd.arg("sync").arg("--foreground");
    if let Some(rt) = runtime {
        cmd.arg("--runtime").arg(rt.name());
    }
    cmd.stdin(Stdio::null()).stdout(log_out).stderr(log_err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, not `setsid` — see the module doc's
        // daemonization caveat for exactly what this does and doesn't buy.
        cmd.process_group(0);
    }
    let child = cmd.spawn().context("spawning the background sync daemon")?;
    println!(
        "ctxlake sync started in background (pid {}) — logs: {}",
        child.id(),
        log_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &Path, fleet: &str, agent: &str) -> Config {
        Config::new(format!("file://{}", dir.display()), fleet, agent)
    }

    #[test]
    fn read_running_pid_is_none_for_a_stale_pidfile() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("stale.pid");
        // A pid essentially guaranteed not to be a live process in any test
        // environment.
        std::fs::write(&pid_path, "999999").unwrap();
        assert_eq!(read_running_pid(&pid_path), None);
    }

    #[test]
    fn read_running_pid_finds_this_very_test_process() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("live.pid");
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();
        assert_eq!(read_running_pid(&pid_path), Some(std::process::id()));
    }

    #[test]
    fn status_reports_not_running_before_anything_starts() {
        // A fleet id essentially guaranteed to have no real pidfile under this
        // host's `$HOME` — `paths::pid_file` isn't overridable per-test by
        // design (see `paths.rs`'s own module doc), so a distinctive name is
        // what keeps this from colliding with a real `ctxlake sync` a developer
        // happens to be running locally.
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path(), "ctxlake-cli-test-fleet-never-real", "cc-01");
        assert!(status(&cfg).is_ok());
    }

    #[tokio::test]
    async fn foreground_ingests_a_spooled_event_writes_the_cache_and_shuts_down_on_signal() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let spool_dir = dir.path().join("spool");
        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::create_dir_all(spool_dir.join("claude_code")).unwrap();

        // A spooled event exactly as `ctxlake-hook` would have appended it,
        // mirroring `ctxlake-sync`'s own daemon test fixture.
        {
            use std::io::Write as _;
            let mut f =
                std::fs::File::create(spool_dir.join("claude_code").join("sess-1.ndjson")).unwrap();
            let e = ctxlake_core::Envelope::new(
                "myteam",
                "cc-01",
                ctxlake_core::Runtime::ClaudeCode,
                "sess-1",
                ctxlake_core::EventType::ToolCall,
                "2026-09-11T18:22:00.000Z",
            );
            writeln!(f, "{}", e.to_ndjson().unwrap()).unwrap();
        }

        // Fast ticks so the test doesn't wait on production-cadence polling —
        // see `interval_override`'s doc for why these three names are safe to
        // set globally without the cross-test flakiness `spool_root`/`cache_root`
        // would carry.
        std::env::set_var("CTXLAKE_SYNC_UPLOAD_INTERVAL_MS", "10");
        std::env::set_var("CTXLAKE_SYNC_CACHE_INTERVAL_MS", "10");
        std::env::set_var("CTXLAKE_SYNC_PRESENCE_INTERVAL_MS", "10");

        let cfg = cfg(&store_dir, "sync-fg-test-fleet", "cc-01");

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let cfg_for_task = cfg.clone();
        let spool_for_task = spool_dir.clone();
        let cache_for_task = cache_dir.clone();
        let handle = tokio::spawn(async move {
            run_foreground_until(&cfg_for_task, None, spool_for_task, cache_for_task, async {
                let _ = rx.await;
            })
            .await
        });

        // Give the loops a few ticks to actually run before asking them to stop.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Pidfile must exist while the daemon is up.
        let pid_path = paths::pid_file(&cfg.fleet_id);
        assert!(pid_path.exists(), "expected a pidfile while running");

        let _ = tx.send(());
        handle.await.unwrap().unwrap();

        std::env::remove_var("CTXLAKE_SYNC_UPLOAD_INTERVAL_MS");
        std::env::remove_var("CTXLAKE_SYNC_CACHE_INTERVAL_MS");
        std::env::remove_var("CTXLAKE_SYNC_PRESENCE_INTERVAL_MS");

        // Pidfile is gone after a clean shutdown.
        assert!(
            !pid_path.exists(),
            "expected the pidfile to be removed on clean shutdown"
        );

        // The upload loop shipped the spooled event to `sessions/`.
        let seg = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "sync-fg-test-fleet",
            ctxlake_core::Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        assert!(
            store_dir.join(seg.as_ref()).exists(),
            "expected the upload loop to have shipped the spooled event to {}",
            store_dir.join(seg.as_ref()).display()
        );

        // The cache loop wrote a roster cache.
        assert!(
            cache_dir
                .join("sync-fg-test-fleet")
                .join("roster.json")
                .exists(),
            "expected the cache loop to have refreshed the roster cache"
        );

        // This daemon's presence entry reached the store. The block that used to sit
        // here asserted that graceful shutdown released a maintenance lease; there is
        // no lease to release. What still has to be true is that the presence loop ran
        // at all — asserted rather than branched on, so a regression that stops it
        // fails loudly here instead of this whole block silently checking nothing.
        let intent = store_dir.join("live").join("agents").join("cc-01.json");
        assert!(
            intent.exists(),
            "expected the presence loop to have published this agent's intent to {}",
            intent.display()
        );

        // And no lease object was created by anything in the chain.
        assert!(
            !store_dir.join("live").join("leases").exists(),
            "nothing in the daemon may create a lease object any more"
        );
    }
}
