//! `ctxlake doctor` — the trust-building command.
//!
//! Two sections, matching docs/getting-started.md's worked example: the backend's
//! actual CAS capability matrix (executed, never assumed — see
//! `ctxlake_store::probe`'s own module doc on why guessing from a hostname is how
//! MinIO's put-if-absent gap gets discovered mid-incident instead of here), and
//! which runtimes are installed and whether ctxlake is wired into them.
//!
//! Exit code policy: **only a backend that cannot be written to and read from at
//! all breaks capture** — the write path (`hook -> spool -> daemon -> sessions/`)
//! needs nothing but a plain `put`/`get`. Every other backend gap (no
//! put-if-absent, no conditional-GET 304) degrades *coordination* — the roster
//! fan-in's cost, the snapshot pointer's safety — without stopping a single event
//! from reaching the lake, so those are loud in the printed report and do not flip
//! the exit code.

use std::time::{Duration, SystemTime};

use anyhow::Result;
use object_store::{ObjectStoreExt, PutPayload};

use crate::config::Config;
use crate::hooks::{self, Runtime as HookRuntime};
use crate::paths;
use crate::store_ctx::{self, full_path};

pub struct Report {
    pub store_url: String,
    /// Best-effort identification of which backend `store_url` addresses
    /// (`ctxlake_store::backend::describe`) — `None` only when `store_url`
    /// itself doesn't parse as a URL, in which case `reachable` is already
    /// `false` and `reachable_detail` explains why.
    pub backend: Option<ctxlake_store::backend::BackendInfo>,
    pub reachable: bool,
    pub reachable_detail: String,
    pub probes: Vec<ctxlake_store::probe::ProbeResult>,
    pub runtimes: Vec<RuntimeReport>,
    pub llm: Option<LlmReport>,
    pub cache: CacheReport,
    pub spool_backlog: SpoolReport,
    pub daemon: DaemonReport,
    pub maint: MaintReport,
    /// docs/memory.md's three-tier spelling (`ctxlake.toml`'s own
    /// `[summarize].mode`) — see `config.rs`'s `SummarizeMode::Display` impl.
    pub summarize_mode: String,
    /// How many sessions currently show a fired Tier 1 nudge marker (`nudge.rs`)
    /// under this fleet's cache dir. Not a health signal by itself — just
    /// visibility into whether Tier 1 (docs/memory.md's default) is
    /// actually firing for this operator's agents.
    pub nudged_sessions: usize,
}

/// Is `ctxlake sync` running (a pidfile whose pid is still alive — see
/// `sync_cmd::read_running_pid`), and, separately, when did maintenance last
/// publish a snapshot.
pub struct DaemonReport {
    pub pid: Option<u32>,
}

pub struct MaintReport {
    /// Age since `snapshot/latest.json` was last published, read straight from
    /// the store's own `last_modified` for that object (never this host's clock
    /// for anything CAS-related — AGENTS.md invariant 6 — though this is a plain
    /// read for display, not a coordination decision). `None` when the store is
    /// unreachable or nothing has ever published a snapshot.
    ///
    /// This is a **proxy**, not a real completion marker: `ctxlake-maint` (the
    /// crate that would publish one) is an empty scaffold as of this wave — see
    /// `maint_cmd.rs`'s module doc — so "last snapshot publish" is the closest
    /// honest answer to "when did maintenance last complete" available today.
    pub last_snapshot_age: Option<Duration>,
}

pub struct RuntimeReport {
    pub runtime: HookRuntime,
    pub path: std::path::PathBuf,
    pub exists: bool,
    pub wired: bool,
    pub foreign_entries: usize,
}

pub struct LlmReport {
    pub env_var: String,
    pub resolves: bool,
}

pub struct CacheReport {
    pub path: std::path::PathBuf,
    pub exists: bool,
    pub age: Option<Duration>,
}

pub struct SpoolReport {
    pub path: std::path::PathBuf,
    pub file_count: usize,
    pub byte_count: u64,
}

impl Report {
    /// See the module doc's exit-code policy: capture (`hook -> spool -> daemon ->
    /// sessions/`) needs nothing from the object store but a plain, unconditional
    /// `put` — not CAS, not `list`, not a conditional `GET`. Those all matter for
    /// *coordination* (`live/`, `snapshot/`) and are reported loudly above, but a
    /// backend that fails every one of them while still accepting a plain write has
    /// not broken capture, so the exit code stays 0.
    pub fn breaks_capture(&self) -> bool {
        !self.reachable
    }

    pub fn print(&self) {
        println!("store   {}", self.store_url);
        if let Some(backend) = &self.backend {
            println!("backend: {}", backend.kind);
            for caveat in &backend.caveats {
                println!("  caveat: {caveat}");
            }
        }
        if !self.reachable {
            println!("  UNREACHABLE: {}", self.reachable_detail);
        } else {
            for p in &self.probes {
                let status = if p.passed { "ok" } else { "FAIL" };
                println!("  {:<28} {status:<6} {}", display_name(p.name), p.detail);
                if !p.passed {
                    println!("      -> {}", meaning(p.name));
                }
            }
        }

        println!("\nruntimes");
        for r in &self.runtimes {
            let state = if !r.exists {
                "not found".to_string()
            } else if r.wired {
                format!("wired      ({} other entries)", r.foreign_entries)
            } else {
                "found, not wired".to_string()
            };
            println!(
                "  {:<12} {:<10} {}",
                r.runtime.name(),
                state,
                r.path.display()
            );
        }
        if self.runtimes.iter().any(|r| r.exists && !r.wired) {
            println!("  (coexistence is fine — run `ctxlake install <runtime>` to wire one in)");
        }

        if let Some(llm) = &self.llm {
            println!(
                "\nllm     {} resolves: {}",
                llm.env_var,
                if llm.resolves { "yes" } else { "no" }
            );
        }

        println!("\ndaemon");
        if self.cache.exists {
            match self.cache.age {
                Some(age) if age < Duration::from_secs(60) => {
                    println!(
                        "  cache fresh ({}s old) — {}",
                        age.as_secs(),
                        self.cache.path.display()
                    );
                }
                Some(age) => {
                    println!(
                        "  cache STALE ({}s old) — is `ctxlake sync` still running? {}",
                        age.as_secs(),
                        self.cache.path.display()
                    );
                }
                None => println!(
                    "  cache present, age unknown — {}",
                    self.cache.path.display()
                ),
            }
        } else {
            println!(
                "  no cache at {} yet — daemon not running, or hasn't completed a first refresh",
                self.cache.path.display()
            );
        }
        println!(
            "  spool backlog: {} file(s), {} bytes — {}",
            self.spool_backlog.file_count,
            self.spool_backlog.byte_count,
            self.spool_backlog.path.display()
        );
        match self.daemon.pid {
            Some(pid) => println!("  process: running (pid {pid})"),
            None => println!(
                "  process: not running — `ctxlake sync` (see docs/reference.md) starts it"
            ),
        }

        println!("\nmaintenance");
        match self.maint.last_snapshot_age {
            Some(age) => println!(
                "  last snapshot published {}s ago (proxy for last completion — see \
                 docs/memory.md)",
                age.as_secs()
            ),
            None => println!(
                "  never — ctxlake-maint has not published a snapshot yet (optional: \
                 `ctxlake maint` from cron/systemd, or run it by hand; see docs/reference.md)"
            ),
        }

        println!("\nsummarization");
        println!("  mode: {}", self.summarize_mode);
        if let Some(note) = summarize_mode_enforcement_note(&self.summarize_mode) {
            println!("  {note}");
        }
        println!(
            "  tier 1 nudges fired: {} session(s) (see docs/memory.md)",
            self.nudged_sessions
        );
    }
}

/// The caveat `Report::print` shows under `summarization / mode: <mode>`,
/// or `None` when `mode` carries no promise this crate could fail to keep. A
/// separate, pure function rather than inline `println!` logic specifically so
/// a test can pin the honesty of its wording without capturing stdout.
///
/// **Why this note exists, and why it is `shadow`-only.** Of the five modes
/// (`docs/memory.md`), only `shadow` makes a claim about what happens on
/// a *read* path: "runs the whole chain ... with agent reads disabled ...
/// nothing reaches a context window." `none`/`agent` never produce a claim to
/// read in the first place, and `batch`/`both` promise extraction, not
/// read-blocking, so there is nothing for those four to fail to keep here. But
/// nothing in this workspace enforces `shadow`'s specific promise yet:
/// `ctxlake_mcp::memory::search` renders promoted claims into an agent's
/// context window without ever consulting `[summarize].mode`. A bare
/// `mode: shadow` line would read as that guarantee being in effect — it is
/// not, so this crate says so rather than letting the config value imply an
/// enforcement it doesn't perform. (`ctxlake-maint` — the crate that would
/// produce a promoted claim at all — is also still an empty scaffold, so
/// `shadow` has nothing to gate today regardless; that is `maint`'s absence,
/// reported separately above, not this note's concern.)
fn summarize_mode_enforcement_note(mode: &str) -> Option<&'static str> {
    if mode == "shadow" {
        Some(
            "NOTE: shadow's \"reads disabled\" is not enforced yet — memory_search \
             does not consult [summarize].mode (see docs/memory.md)",
        )
    } else {
        None
    }
}

fn display_name(probe: &str) -> &'static str {
    match probe {
        "put-if-absent" => "put-if-absent",
        "cas-update" => "compare-and-swap",
        "cas-conflict-detection" => "conflict detection",
        "conditional-get-304" => "conditional GET",
        "list" => "list",
        "delete" => "delete",
        _ => "unknown probe",
    }
}

fn meaning(probe: &str) -> &'static str {
    match probe {
        "put-if-absent" => {
            "expected on MinIO (minio/minio#20346) — ctxlake never relies on this; \
             roster heartbeats and the snapshot pointer are CAS-only by design \
             (AGENTS.md invariant 4)"
        }
        "cas-update" | "cas-conflict-detection" => {
            "the roster fan-in and the snapshot publish cannot work correctly on \
             this backend — CAS is the one primitive coordination depends on. \
             Capture (writing sessions) is unaffected; this breaks live/, not \
             sessions/."
        }
        "conditional-get-304" => {
            "roster polling will cost a full GET every cycle instead of a cheap 304 \
             — works, but scales worse (see docs/storage.md)"
        }
        "list" => "the roster fan-in and maintenance both require LIST — capture (writing sessions) does not",
        "delete" => "scratch-object cleanup won't fully work; harmless on its own",
        _ => "unrecognized probe result",
    }
}

pub async fn run(cfg: &Config) -> Result<Report> {
    // Connecting at all can fail before any request is made — `file://`'s backend
    // canonicalizes its root directory up front, for instance. That is exactly as
    // much an "unreachable store" as a `put` that fails once connected, so both
    // collapse into the same reachable/not-reachable question rather than one of
    // them propagating as a hard `Result::Err` out of `doctor` and the other
    // showing up in the printed report.
    let connected = store_ctx::connect(cfg, &cfg.agent_id);
    let mut maint = MaintReport {
        last_snapshot_age: None,
    };
    // Best-effort label only — an unparseable `store_url` already shows up as
    // `reachable: false` with `reachable_detail` explaining why, so there is
    // nothing more useful to say here than "we don't know" (`None`).
    let backend = url::Url::parse(&cfg.store).ok().map(|url| {
        ctxlake_store::backend::describe(&url, &ctxlake_store::backend::BackendOptions::default())
    });
    let (reachable, reachable_detail, probes) = match &connected {
        Err(e) => (false, format!("{e:#}"), Vec::new()),
        Ok(ctx) => {
            let probe_key = full_path(
                ctx,
                &object_store::path::Path::from("_meta")
                    .join("doctor-probe")
                    .join(format!("{}.json", cfg.agent_id)),
            );
            let put_result = ctx
                .store
                .put(&probe_key, PutPayload::from_static(b"{}"))
                .await;
            match put_result {
                Ok(_) => {
                    let _ = ctx.store.delete(&probe_key).await;
                    let probes = ctxlake_store::probe::run(ctx.store.as_ref(), &cfg.agent_id).await;

                    // Best-effort: absent (`NotFound`) is the overwhelmingly common
                    // case for now (see `MaintReport::last_snapshot_age`'s doc) and
                    // any other error just leaves this `None` rather than failing
                    // the whole report over a display-only field.
                    let snapshot_key = full_path(ctx, &ctxlake_store::layout::snapshot_latest());
                    if let Ok(meta) = ctx.store.head(&snapshot_key).await {
                        if let Ok(age) =
                            SystemTime::now().duration_since(SystemTime::from(meta.last_modified))
                        {
                            maint.last_snapshot_age = Some(age);
                        }
                    }

                    (true, "ok".to_string(), probes)
                }
                Err(e) => (false, e.to_string(), Vec::new()),
            }
        }
    };

    let runtimes = HookRuntime::ALL
        .into_iter()
        .map(|runtime| {
            let path = runtime.default_config_path();
            let status = hooks::detect(runtime, &path).unwrap_or(hooks::RuntimeStatus {
                config_exists: path.exists(),
                ctxlake_wired: false,
                foreign_entries: 0,
            });
            RuntimeReport {
                runtime,
                path,
                exists: status.config_exists,
                wired: status.ctxlake_wired,
                foreign_entries: status.foreign_entries,
            }
        })
        .collect();

    let llm = if cfg.summarize.mode.needs_batch() {
        cfg.summarize.batch.as_ref().map(|b| LlmReport {
            resolves: std::env::var(&b.api_key_env).is_ok(),
            env_var: b.api_key_env.clone(),
        })
    } else {
        None
    };

    let cache_dir = paths::cache_dir(&cfg.fleet_id);
    let cache_marker = cache_dir.join("roster.json");
    let cache = match std::fs::metadata(&cache_marker) {
        Ok(meta) => CacheReport {
            path: cache_marker,
            exists: true,
            age: meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok()),
        },
        Err(_) => CacheReport {
            path: cache_marker,
            exists: false,
            age: None,
        },
    };

    let spool_backlog = scan_spool(&paths::spool_root());

    let daemon = DaemonReport {
        pid: crate::sync_cmd::read_running_pid(&paths::pid_file(&cfg.fleet_id)),
    };
    // Not fleet-scoped: the marker directory keys by session id alone (see
    // `nudge.rs`'s doc), so this counts every fleet's fired nudges on this host.
    let nudged_sessions = crate::nudge::count_nudged(&ctxlake_core::paths::cache_root());

    Ok(Report {
        store_url: cfg.store.clone(),
        backend,
        reachable,
        reachable_detail,
        probes,
        runtimes,
        llm,
        cache,
        spool_backlog,
        daemon,
        maint,
        summarize_mode: cfg.summarize.mode.to_string(),
        nudged_sessions,
    })
}

fn scan_spool(dir: &std::path::Path) -> SpoolReport {
    let mut file_count = 0;
    let mut byte_count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = entry.metadata() {
                file_count += 1;
                byte_count += meta.len();
            }
        }
    }
    SpoolReport {
        path: dir.to_path_buf(),
        file_count,
        byte_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn doctor_against_a_file_store_runs_every_probe_and_does_not_break_capture() {
        let store_dir = tempfile::tempdir().unwrap();
        let cfg = Config::new(
            format!("file://{}", store_dir.path().display()),
            "myteam",
            "cc-01",
        );
        let report = run(&cfg).await.unwrap();
        assert!(report.reachable, "{}", report.reachable_detail);
        assert!(!report.breaks_capture());
        assert_eq!(report.probes.len(), 6, "every probe should have run");
        assert!(
            report.probes.iter().all(|p| p.passed),
            "{:?}",
            report.probes.iter().find(|p| !p.passed)
        );
        assert_eq!(
            report.backend.as_ref().map(|b| b.kind),
            Some("local filesystem"),
            "doctor must report which backend it detected (see docs/storage.md)"
        );
    }

    #[test]
    fn only_unreachability_breaks_capture() {
        // Regression guard for the exit-code policy: a backend that fails every
        // fine-grained CAS probe but still accepts a plain write has not broken
        // capture — only a store that cannot be written to at all has.
        let report = Report {
            store_url: "s3://bucket".into(),
            backend: None,
            reachable: true,
            reachable_detail: "ok".into(),
            probes: vec![
                ctxlake_store::probe::ProbeResult {
                    name: "cas-update",
                    passed: false,
                    detail: "boom".into(),
                },
                ctxlake_store::probe::ProbeResult {
                    name: "list",
                    passed: false,
                    detail: "boom".into(),
                },
            ],
            runtimes: vec![],
            llm: None,
            cache: CacheReport {
                path: "/tmp/x".into(),
                exists: false,
                age: None,
            },
            spool_backlog: SpoolReport {
                path: "/tmp/y".into(),
                file_count: 0,
                byte_count: 0,
            },
            daemon: DaemonReport { pid: None },
            maint: MaintReport {
                last_snapshot_age: None,
            },
            summarize_mode: "agent".into(),
            nudged_sessions: 0,
        };
        assert!(!report.breaks_capture());
    }

    #[tokio::test]
    async fn an_unreachable_store_breaks_capture() {
        let bogus_parent = tempfile::NamedTempFile::new().unwrap();
        let cfg = Config::new(
            format!("file://{}/nested", bogus_parent.path().display()),
            "myteam",
            "cc-01",
        );
        let report = run(&cfg).await.unwrap();
        assert!(!report.reachable);
        assert!(report.breaks_capture());
    }

    #[tokio::test]
    async fn a_missing_cache_and_empty_spool_are_reported_not_fatal() {
        let store_dir = tempfile::tempdir().unwrap();
        let cfg = Config::new(
            format!("file://{}", store_dir.path().display()),
            "no-such-fleet",
            "cc-01",
        );
        let report = run(&cfg).await.unwrap();
        assert!(!report.cache.exists);
        assert_eq!(report.spool_backlog.file_count, 0);
        assert!(!report.breaks_capture());
    }

    #[test]
    fn spool_scan_counts_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cc-01")).unwrap();
        std::fs::write(dir.path().join("cc-01/sess-1.ndjson"), "line\n").unwrap();
        std::fs::write(dir.path().join("cc-01/sess-2.ndjson"), "line\nline\n").unwrap();
        let report = scan_spool(dir.path());
        assert_eq!(report.file_count, 2);
        assert_eq!(report.byte_count, 5 + 10);
    }

    #[test]
    fn llm_check_is_skipped_when_the_mode_never_needs_a_batch_key() {
        let cfg = Config::new("file:///tmp/lake", "myteam", "cc-01");
        assert!(!cfg.summarize.mode.needs_batch());
    }

    #[tokio::test]
    async fn reports_no_daemon_running_without_a_pidfile() {
        let store_dir = tempfile::tempdir().unwrap();
        let cfg = Config::new(
            format!("file://{}", store_dir.path().display()),
            "doctor-test-fleet-no-daemon",
            "cc-01",
        );
        let report = run(&cfg).await.unwrap();
        assert_eq!(report.daemon.pid, None);
    }

    #[tokio::test]
    async fn reports_the_daemon_pid_when_its_pidfile_is_live() {
        let store_dir = tempfile::tempdir().unwrap();
        let fleet_id = "doctor-test-fleet-live-daemon";
        let cfg = Config::new(
            format!("file://{}", store_dir.path().display()),
            fleet_id,
            "cc-01",
        );
        let pid_path = paths::pid_file(fleet_id);
        std::fs::create_dir_all(pid_path.parent().unwrap()).unwrap();
        // This test process's own pid is guaranteed alive for the test's duration.
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();

        let report = run(&cfg).await.unwrap();
        assert_eq!(report.daemon.pid, Some(std::process::id()));

        std::fs::remove_file(&pid_path).unwrap();
    }

    #[tokio::test]
    async fn reports_the_configured_summarize_mode() {
        let store_dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(
            format!("file://{}", store_dir.path().display()),
            "myteam",
            "cc-01",
        );
        assert_eq!(run(&cfg).await.unwrap().summarize_mode, "agent");

        cfg.summarize.mode = crate::config::SummarizeMode::None;
        assert_eq!(run(&cfg).await.unwrap().summarize_mode, "none");
    }

    /// `mode: shadow` on an operator's screen must never stand alone: nothing
    /// in this workspace enforces shadow's "reads disabled" promise on a read
    /// path today (`memory_search` doesn't consult the config at all), so
    /// doctor must say so rather than let the bare mode name imply the
    /// guarantee is active. The other four modes make no read-path promise at
    /// all (see `summarize_mode_enforcement_note`'s doc for why `batch`/`both`
    /// don't need this caveat either), so they must print no note — a blanket
    /// caveat on every mode would bury the one that actually matters.
    #[test]
    fn summarize_mode_note_flags_only_shadow() {
        assert!(
            summarize_mode_enforcement_note("shadow").is_some_and(|n| n.contains("not enforced")),
            "shadow must carry an honest not-enforced caveat"
        );
        for mode in ["none", "agent", "batch", "both"] {
            assert_eq!(
                summarize_mode_enforcement_note(mode),
                None,
                "mode {mode:?} makes no read-path promise to caveat"
            );
        }
    }

    #[tokio::test]
    async fn reports_no_snapshot_published_yet_for_a_fresh_store() {
        let store_dir = tempfile::tempdir().unwrap();
        let cfg = Config::new(
            format!("file://{}", store_dir.path().display()),
            "myteam",
            "cc-01",
        );
        let report = run(&cfg).await.unwrap();
        assert_eq!(report.maint.last_snapshot_age, None);
    }
}
