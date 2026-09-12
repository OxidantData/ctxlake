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
//! put-if-absent, no conditional-GET 304) degrades *coordination* — leases,
//! roster fan-in cost — without stopping a single event from reaching the lake, so
//! those are loud in the printed report and do not flip the exit code.

use std::time::{Duration, SystemTime};

use anyhow::Result;
use object_store::{ObjectStoreExt, PutPayload};

use crate::config::Config;
use crate::hooks::{self, Runtime as HookRuntime};
use crate::paths;
use crate::store_ctx::{self, full_path};

pub struct Report {
    pub store_url: String,
    pub reachable: bool,
    pub reachable_detail: String,
    pub probes: Vec<ctxlake_store::probe::ProbeResult>,
    pub runtimes: Vec<RuntimeReport>,
    pub llm: Option<LlmReport>,
    pub cache: CacheReport,
    pub spool_backlog: SpoolReport,
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
             leases and roster heartbeats are CAS-only by design (AGENTS.md invariant 4)"
        }
        "cas-update" | "cas-conflict-detection" => {
            "leases and the roster fan-in cannot work correctly on this backend — CAS \
             is the one primitive coordination depends on. Capture (writing sessions) \
             is unaffected; this breaks live/, not sessions/."
        }
        "conditional-get-304" => {
            "roster polling will cost a full GET every cycle instead of a cheap 304 \
             — works, but scales worse (see docs/scaling.md)"
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
    let (reachable, reachable_detail, probes) = match connected {
        Err(e) => (false, format!("{e:#}"), Vec::new()),
        Ok(ctx) => {
            let probe_key = full_path(
                &ctx,
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

    let spool_dir = paths::spool_dir(&cfg.fleet_id);
    let spool_backlog = scan_spool(&spool_dir);

    Ok(Report {
        store_url: cfg.store.clone(),
        reachable,
        reachable_detail,
        probes,
        runtimes,
        llm,
        cache,
        spool_backlog,
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
    }

    #[test]
    fn only_unreachability_breaks_capture() {
        // Regression guard for the exit-code policy: a backend that fails every
        // fine-grained CAS probe but still accepts a plain write has not broken
        // capture — only a store that cannot be written to at all has.
        let report = Report {
            store_url: "s3://bucket".into(),
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
}
