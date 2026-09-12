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
    /// Whether the configured provider actually answered a real request.
    ///
    /// `resolves` alone is a much weaker check than it looks: a revoked key, a
    /// typo'd model name, a base URL pointing at nothing, and an account over its
    /// quota all resolve an env var perfectly well and then fail at extraction time,
    /// hours later, inside a maintenance log. `None` when no call was attempted
    /// (nothing to call, or the key was missing so there was no point).
    pub reachable: Option<Result<(), String>>,
    /// Whether the daemon will have this key too, or only this shell does.
    ///
    /// `doctor` runs in a terminal that has your `export`s; a launchd job and a
    /// systemd user unit have neither those nor your shell rc. So "the key resolves"
    /// answers a question nobody asked — the one that matters is whether it resolves
    /// *where the daemon runs*, and the only place that is true of is
    /// [`crate::paths::env_file`].
    ///
    /// `true` for a provider that needs no key at all, since there is nothing for the
    /// daemon to be missing.
    pub daemon_will_resolve: bool,
}

impl LlmReport {
    /// The warning for a key that this shell has and the daemon will not.
    ///
    /// A warning rather than a `blocks_service_install` reason on purpose. Someone
    /// running `ctxlake sync run --foreground` under a process manager of their own,
    /// or with a systemd drop-in carrying their own `EnvironmentFile=`, has a
    /// perfectly working setup that this cannot see — refusing to install would be
    /// wrong for them. And since an unreachable provider no longer takes the daemon
    /// down with it (`sync_cmd::run_foreground_until`), the cost of being wrong in
    /// this direction is some missing claims, not a dead daemon.
    ///
    /// `None` when there is nothing to warn about: no key needed, the key is already
    /// in the env file, or it does not resolve at all (which `print` reports as the
    /// louder, more basic problem).
    pub fn daemon_env_warning(&self) -> Option<String> {
        if self.daemon_will_resolve || !self.resolves || self.env_var.trim().is_empty() {
            return None;
        }
        let k = &self.env_var;
        let f = crate::paths::env_file().display().to_string();
        Some(format!(
            "-> {k} is set in this shell, but the sync daemon will not see it.\n\
             \x20  launchd and systemd start with neither your exports nor your shell\n\
             \x20  rc, so Tier 2 would fail on every maintenance cycle. Put it where\n\
             \x20  both can read it:\n\
             \x20    mkdir -p $(dirname {f}) && touch {f} && chmod 600 {f}\n\
             \x20    echo \"{k}=$(printenv {k})\" >> {f}"
        ))
    }
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

    /// Why this host is not fit to run a supervised daemon yet, or empty if it is.
    ///
    /// `ctxlake sync install` refuses on a non-empty list. Installing a service that
    /// cannot reach the store, or that is configured for a model it cannot call,
    /// produces a daemon that looks healthy in `systemctl status` and silently
    /// achieves nothing — and because capture keeps working regardless (the hook only
    /// writes locally), nobody finds out until a briefing goes stale days later.
    ///
    /// Deliberately narrower than "everything `doctor` printed". Missing runtime hooks
    /// are a warning, not a blocker: wiring a runtime after installing the daemon is a
    /// perfectly ordinary order to do things in, and a fleet host that only ships
    /// other machines' spools may never have a runtime at all.
    pub fn blocks_service_install(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if !self.reachable {
            reasons.push(format!(
                "the store is unreachable: {}",
                self.reachable_detail
            ));
        } else {
            for p in self.probes.iter().filter(|p| !p.passed) {
                reasons.push(format!(
                    "{} failed — {}",
                    display_name(p.name),
                    meaning(p.name)
                ));
            }
        }
        if let Some(llm) = &self.llm {
            match &llm.reachable {
                Some(Ok(())) => {}
                Some(Err(e)) => reasons.push(format!(
                    "[summarize.batch] is configured but the provider rejected a test call: {e}"
                )),
                None => reasons.push(if llm.env_var.trim().is_empty() {
                    "[summarize.batch] is configured but its provider could not be \
                     reached"
                        .to_string()
                } else {
                    format!(
                        "[summarize.batch] is configured but {} is not set",
                        llm.env_var
                    )
                }),
            }
        }
        reasons
    }

    pub fn print(&self) {
        println!("store   {}", self.store_url);
        if let Some(backend) = &self.backend {
            println!("backend: {}", backend.kind);
            if self.store_url.starts_with("s3://") || self.store_url.starts_with("s3a://") {
                // Printed whether or not the store is reachable: knowing which
                // identity ctxlake will use is the thing you want *before* it fails,
                // and the thing nobody can otherwise find out.
                println!(
                    "  credentials: {}",
                    ctxlake_store::aws_profile::describe_source()
                );
            }
            for caveat in &backend.caveats {
                println!("  caveat: {caveat}");
            }
        }
        if !self.reachable {
            println!("  UNREACHABLE: {}", self.reachable_detail);
            // A 403 means the request was signed and refused, which is a completely
            // different problem from a network failure and has a completely different
            // fix — but the raw error looks like neither. Say which identity signed
            // it, because on a machine with more than one AWS account configured
            // "Access Denied" and "wrong profile" are indistinguishable.
            if let Some(d) = ctxlake_store::aws_profile::diagnose(&self.reachable_detail) {
                println!("      -> {d}");
            }
            if let Some(h) = ctxlake_store::aws_profile::unsupported_profile_hint() {
                println!("      -> {h}");
            }
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
            let named = if llm.env_var.trim().is_empty() {
                "no API key needed".to_string()
            } else {
                llm.env_var.clone()
            };
            match &llm.reachable {
                // A keyless provider (claude-cli, ollama) has no variable to name,
                // and printing an empty one left a blank gap mid-sentence.
                Some(Ok(())) => println!("\nllm     {named} — provider answered"),
                Some(Err(e)) => println!("\nllm     {named} — provider FAILED: {e}"),
                None => println!("\nllm     {named} — does not resolve"),
            }
            if !llm.resolves {
                println!(
                    "      -> export it, or point api_key_env at the variable that holds the key"
                );
            } else if let Some(warning) = llm.daemon_env_warning() {
                for line in warning.lines() {
                    println!("      {line}");
                }
            }
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
                "  never — no snapshot published yet. `ctxlake sync` runs the \
                 maintenance chain every 5 min; `ctxlake maint --once` forces a cycle \
                 now (docs/reference.md)"
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
/// Whether a mode's promise needs a caveat printed beside it.
///
/// It used to: `shadow` promises that no promoted claim reaches a context window,
/// and when this was written nothing enforced that — `ctxlake-maint` was an empty
/// scaffold and `memory_search` consulted no mode. Printing `mode: shadow` alone
/// would have implied a guarantee that was not in effect.
///
/// Both halves are now true, so the caveat is gone. `snapshot::publish` populates
/// `claims_fts` — the only index `memory_search` queries — from rows marked
/// `visible_to_agents`, and a caller passing `agent_reads_enabled: false` produces a
/// snapshot with none. The enforcement is at the *publish* point, which is stronger
/// than a read-time check: the rows are not there to serve, rather than present and
/// skipped by a reader who has to remember.
///
/// Kept as a function rather than deleted because the shape is right — a mode whose
/// promise outruns its enforcement should say so here — and because a stale warning
/// about a safety feature costs as much as a missing one. This one survived past the
/// work that made it false and was reported by an operator reading their own
/// `doctor` output.
fn summarize_mode_enforcement_note(_mode: &str) -> Option<&'static str> {
    None
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

/// Ask the configured provider to answer one trivial request.
///
/// The smallest call that still exercises everything that can be wrong: credentials,
/// the endpoint, the model name, and the account's standing. A structural check of
/// the config cannot tell you any of those, and every one of them surfaces otherwise
/// as "extraction quietly produced no claims".
///
/// The error is returned as a plain string rather than propagated, because this is a
/// *report* — `doctor` prints every finding and then decides, and one unreachable
/// provider must not stop it reporting the store and the runtimes too.
async fn probe_provider(b: &crate::config::BatchConfig) -> Result<(), String> {
    let batch: ctxlake_maint::extract::BatchConfig = b.into();
    let provider = ctxlake_maint::extract::build_provider(&batch).map_err(|e| e.to_string())?;
    let req = ctxlake_maint::extract::CompletionRequest {
        // Deliberately not the extraction prompt: this is a liveness check, and
        // sending a real transcript to prove the endpoint answers would put session
        // content on the wire for a command the user ran to check their config.
        system_prompt: "Reply with the single word: ok".to_string(),
        user_prompt: "ok".to_string(),
        model: batch.model.clone(),
    };
    provider
        .complete(&req)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
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
        match cfg.summarize.batch.as_ref() {
            Some(b) => {
                // Not every provider has a key. `claude-cli` uses the subscription the
                // CLI is signed in to and `ollama` is a local endpoint, so both leave
                // `api_key_env` empty — and checking `std::env::var("")` for those
                // reported "configured but  is not set", with a blank where the
                // variable name should be, and blocked `ctxlake sync install` on a
                // machine that was correctly configured. Reported from a real install.
                let needs_key = !b.api_key_env.trim().is_empty();
                let resolves = !needs_key || std::env::var(&b.api_key_env).is_ok();
                let daemon_will_resolve = !needs_key
                    || crate::envfile::names(&paths::env_file()).contains(&b.api_key_env);
                // A round trip is worth it whenever there is nothing already known to
                // be wrong — which for a keyless provider is always.
                let reachable = if resolves {
                    Some(probe_provider(b).await)
                } else {
                    None
                };
                Some(LlmReport {
                    resolves,
                    env_var: b.api_key_env.clone(),
                    reachable,
                    daemon_will_resolve,
                })
            }
            None => None,
        }
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
    async fn a_missing_cache_and_a_spool_backlog_are_reported_not_fatal() {
        // Deliberately asserts nothing about the spool's *contents*. `spool_root()`
        // is not injectable by design (see paths.rs), so this reads the developer's
        // real `~/.ctxlake/spool` — and the original version required it to be empty,
        // which held only on a machine where ctxlake had never actually run. It
        // started failing the moment the tool was installed here and began capturing
        // a live session: a test that passes only while the product is unused.
        //
        // What this test is really for is the exit-code policy — neither a missing
        // cache nor a backlog breaks capture. Counting is covered hermetically by
        // `spool_scan_counts_files_recursively` against a tempdir.
        let store_dir = tempfile::tempdir().unwrap();
        let cfg = Config::new(
            format!("file://{}", store_dir.path().display()),
            "no-such-fleet",
            "cc-01",
        );
        let report = run(&cfg).await.unwrap();
        assert!(!report.cache.exists, "a fleet with no cache must say so");
        assert!(
            !report.breaks_capture(),
            "neither a missing cache nor a spool backlog is fatal"
        );
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

    fn llm(env_var: &str, resolves: bool, daemon_will_resolve: bool) -> LlmReport {
        LlmReport {
            env_var: env_var.into(),
            resolves,
            reachable: resolves.then_some(Ok(())),
            daemon_will_resolve,
        }
    }

    #[test]
    fn a_key_only_this_shell_has_is_reported_as_a_gap_the_daemon_will_hit() {
        // The bug class this closes, seen twice on real machines: `doctor` answers
        // "does this key resolve", the operator reads it as "the daemon is configured",
        // and those are different questions. launchd's job and a systemd user unit get
        // neither the `export` nor the shell rc that set it.
        let w = llm("OPENROUTER_API_KEY", true, false)
            .daemon_env_warning()
            .expect("a shell-only key must warn");
        assert!(w.contains("OPENROUTER_API_KEY"), "{w}");
        assert!(w.contains("will not see it"), "{w}");
        assert!(w.contains("chmod 600"), "must say where to put it: {w}");
        // The remedy must not ask anyone to retype a secret into their shell history.
        assert!(w.contains("printenv OPENROUTER_API_KEY"), "{w}");
    }

    #[test]
    fn nothing_is_warned_about_when_the_daemon_already_has_what_it_needs() {
        assert_eq!(
            llm("OPENROUTER_API_KEY", true, true).daemon_env_warning(),
            None
        );
        // A keyless provider (claude-cli, ollama) has no variable to be missing.
        assert_eq!(llm("", true, true).daemon_env_warning(), None);
        // A key that does not resolve at all is a louder, more basic problem, and
        // `print` already says so — two overlapping paragraphs would bury both.
        assert_eq!(
            llm("OPENROUTER_API_KEY", false, false).daemon_env_warning(),
            None
        );
    }

    #[tokio::test]
    async fn a_shell_only_key_warns_but_does_not_block_installing_the_daemon() {
        // Deliberate: someone running the daemon under their own supervisor, or with
        // a systemd drop-in carrying EnvironmentFile=, has a working setup this cannot
        // see. And an unreachable provider no longer takes the daemon down with it
        // (`sync_cmd::run_foreground_until`), so being wrong here costs claims, not
        // capture.
        let (_dir, mut report) = healthy_report().await;
        report.llm = Some(llm("OPENROUTER_API_KEY", true, false));
        assert!(
            report.blocks_service_install().is_empty(),
            "got: {:?}",
            report.blocks_service_install()
        );
        assert!(
            report.llm.as_ref().unwrap().daemon_env_warning().is_some(),
            "it must still be said, loudly"
        );
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
    fn no_mode_carries_an_enforcement_caveat_now_that_shadow_is_enforced() {
        // `shadow` used to print "reads disabled is not enforced yet", which was
        // true when nothing produced a promoted claim and `memory_search` consulted
        // no mode. Both changed, and the warning outlived them — an operator read it
        // in their own `doctor` output and reasonably concluded the safety property
        // they had been promised was not in effect.
        //
        // Enforcement now lives in `snapshot::publish`: `claims_fts`, the only index
        // `memory_search` queries, is built from rows marked `visible_to_agents`, and
        // shadow marks none. ctxlake-maint's own suite proves it
        // (`shadow_mode_still_publishes_nothing_agent_queryable` and three others).
        for mode in ["none", "agent", "batch", "both", "shadow"] {
            assert_eq!(
                summarize_mode_enforcement_note(mode),
                None,
                "mode {mode:?} makes no promise this crate cannot back"
            );
        }
    }

    /// A report from a real, reachable `file://` store with nothing configured — the
    /// baseline the install gate should let through. Built by actually running
    /// `doctor` rather than hand-constructing a `Report`, so these tests cannot drift
    /// from what the real command produces as fields are added.
    async fn healthy_report() -> (tempfile::TempDir, Report) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::new(
            format!("file://{}", dir.path().display()),
            "myteam",
            "cc-01",
        );
        let report = run(&cfg).await.unwrap();
        assert!(
            report.blocks_service_install().is_empty(),
            "the baseline must be unblocked or every test below proves nothing"
        );
        (dir, report)
    }

    /// The install gate is the one check whose *failure* has to be loud, so each of
    /// its three blocking reasons is asserted independently — a gate that returns
    /// non-empty for the wrong reason still blocks, and would pass a single
    /// "is it non-empty" test while reporting nonsense to the user.
    #[tokio::test]
    async fn a_healthy_host_is_not_blocked_from_installing_the_service() {
        let store_dir = tempfile::tempdir().unwrap();
        let cfg = Config::new(
            format!("file://{}", store_dir.path().display()),
            "myteam",
            "cc-01",
        );
        let report = run(&cfg).await.unwrap();
        assert!(
            report.blocks_service_install().is_empty(),
            "a reachable store with no model configured is ready: {:?}",
            report.blocks_service_install()
        );
    }

    #[tokio::test]
    async fn an_unreachable_store_blocks_the_service_install() {
        // A path that cannot be created, so the store is genuinely unreachable
        // rather than merely empty.
        let cfg = Config::new(
            "file:///dev/null/not-a-directory".to_string(),
            "myteam",
            "cc-01",
        );
        let report = run(&cfg).await.unwrap();
        let blockers = report.blocks_service_install();
        assert!(
            blockers.iter().any(|b| b.contains("unreachable")),
            "expected an unreachable-store blocker, got: {blockers:?}"
        );
    }

    #[tokio::test]
    async fn a_keyless_provider_is_not_blocked_for_a_missing_env_var() {
        // `claude-cli` and `ollama` authenticate some other way, so `api_key_env` is
        // empty. Checking `std::env::var("")` for those made `ctxlake sync install`
        // refuse a correctly configured machine with "configured but  is not set" —
        // a blank where the variable name should be. Reported from a real install on
        // a second host.
        let (_dir, mut report) = healthy_report().await;
        report.llm = Some(LlmReport {
            env_var: String::new(),
            resolves: true,
            reachable: Some(Ok(())),
            // Keyless: there is no variable for the daemon to be missing.
            daemon_will_resolve: true,
        });
        assert!(
            report.blocks_service_install().is_empty(),
            "a keyless provider that answers must not block: {:?}",
            report.blocks_service_install()
        );
    }

    #[tokio::test]
    async fn a_keyless_provider_that_cannot_be_reached_still_blocks_but_reads_sensibly() {
        let (_dir, mut report) = healthy_report().await;
        report.llm = Some(LlmReport {
            env_var: String::new(),
            resolves: true,
            reachable: Some(Err("claude is not on PATH".to_string())),
            // Keyless: there is no variable for the daemon to be missing.
            daemon_will_resolve: true,
        });
        let blockers = report.blocks_service_install();
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert!(
            !blockers[0].contains("  is not set"),
            "must never print a blank variable name: {blockers:?}"
        );
        assert!(blockers[0].contains("PATH"), "{blockers:?}");
    }

    #[tokio::test]
    async fn a_configured_model_with_no_key_blocks_the_service_install() {
        // Built directly rather than through `run`, because `run` would have to
        // mutate the process environment to simulate an unset variable and this
        // crate's tests run in parallel threads (see paths.rs's module doc).
        let (_dir, mut report) = healthy_report().await;
        report.llm = Some(LlmReport {
            env_var: "CTXLAKE_TEST_KEY_NOT_SET".to_string(),
            resolves: false,
            reachable: None,
            // Keyless: there is no variable for the daemon to be missing.
            daemon_will_resolve: true,
        });
        let blockers = report.blocks_service_install();
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert!(
            blockers[0].contains("CTXLAKE_TEST_KEY_NOT_SET") && blockers[0].contains("not set"),
            "the blocker must name the variable: {blockers:?}"
        );
    }

    #[tokio::test]
    async fn a_key_that_resolves_but_a_provider_that_rejects_it_still_blocks() {
        // The case `resolves: true` alone would wave through, and the reason this
        // check exists: a revoked key, a typo'd model, or an account over quota all
        // resolve an env var perfectly and fail hours later inside a maint log.
        let (_dir, mut report) = healthy_report().await;
        report.llm = Some(LlmReport {
            env_var: "OPENROUTER_API_KEY".to_string(),
            resolves: true,
            reachable: Some(Err("401 Unauthorized".to_string())),
            // Keyless: there is no variable for the daemon to be missing.
            daemon_will_resolve: true,
        });
        let blockers = report.blocks_service_install();
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert!(blockers[0].contains("401"), "{blockers:?}");
    }

    #[tokio::test]
    async fn a_working_provider_does_not_block() {
        let (_dir, mut report) = healthy_report().await;
        report.llm = Some(LlmReport {
            env_var: "OPENROUTER_API_KEY".to_string(),
            resolves: true,
            reachable: Some(Ok(())),
            // Keyless: there is no variable for the daemon to be missing.
            daemon_will_resolve: true,
        });
        assert!(report.blocks_service_install().is_empty());
    }

    #[tokio::test]
    async fn missing_runtime_hooks_are_a_warning_not_a_blocker() {
        // Installing the daemon before wiring a runtime is an ordinary order to do
        // things in, and a host that only ships other machines' spools may never
        // have a runtime at all.
        let (_dir, mut report) = healthy_report().await;
        report.runtimes = vec![RuntimeReport {
            runtime: HookRuntime::ClaudeCode,
            path: std::path::PathBuf::from("/home/alice/.claude/settings.json"),
            exists: false,
            wired: false,
            foreign_entries: 0,
        }];
        assert!(report.blocks_service_install().is_empty());
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
