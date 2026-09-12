//! `ctxlake maint [--once]` — compact, digest, and publish the snapshot.
//!
//! **This used to be a lease, wrapped around nothing.** The command acquired a
//! fleet-wide maintenance lease, reported "held elsewhere" and exited if it lost the
//! race, and then — because `ctxlake-maint` was still a scaffold when this module was
//! written — printed that there was no chain to run. Both halves are now wrong: the
//! chain exists, and the lease does not.
//!
//! The lease is gone because every step the chain takes is idempotent **by content**,
//! which is a stronger guarantee than mutual exclusion and needs no coordination at
//! all:
//!
//! - **Compaction** writes into a directory named for the hash of its input set. Two
//!   hosts compacting the same sealed sessions write identical bytes to the same path;
//!   different input sets land in different directories and cannot collide.
//! - **Digests** are claimed per session with a create-if-absent marker, so exactly
//!   one host does each session's work — a marker, not a lock: no holder, no TTL,
//!   nothing to steal or expire.
//! - **The snapshot** is content-addressed and published by swapping a CAS pointer.
//!
//! So `ctxlake maint` on every host in the fleet, from cron, at the same minute, is
//! fine. That was already the *intent* behind the lease's quiet exit-0 on contention;
//! removing the lease keeps the property and deletes the concept.

use std::time::Duration;

use anyhow::{Context, Result};
use ctxlake_maint::run::Tier2;

use crate::config::{Config, SummarizeMode};
use crate::store_ctx;

/// How long between chain runs when not `--once`. Generous on purpose: maintenance is
/// definitionally not latency-sensitive — nothing is waiting on it.
const MAINT_LOOP_INTERVAL: Duration = Duration::from_secs(300);

/// Whether promoted claims may reach an agent's context window.
///
/// `shadow` is the whole point of this mapping: it runs extraction and the gates in
/// full so their output can be read and judged, while keeping every promoted claim out
/// of every session. `none` runs no belief layer at all, so there is nothing to serve
/// either way. Everything else serves normally.
fn agent_reads_enabled(mode: SummarizeMode) -> bool {
    !matches!(mode, SummarizeMode::Shadow | SummarizeMode::None)
}

pub async fn run(cfg: &Config, once: bool) -> Result<()> {
    loop {
        println!("{}", run_one_cycle(cfg).await?);
        if once {
            return Ok(());
        }
        tokio::time::sleep(MAINT_LOOP_INTERVAL).await;
    }
}

/// Run the chain once and return the one-line summary, rather than printing it.
///
/// Returned rather than printed because there are two callers with different output
/// channels: `ctxlake maint` writes to stdout for a human, and the sync daemon's
/// maintenance loop (`sync_cmd.rs`) writes to its log file. Printing here would put
/// the daemon's cycle reports on a stdout nobody reads.
pub(crate) async fn run_one_cycle(cfg: &Config) -> Result<String> {
    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;
    let store = store_ctx::prefixed_store(&ctx);

    // `ctxlake-cli`'s own `SummarizeConfig` is `ctxlake.toml`'s serde shape;
    // `ctxlake_maint::extract`'s is what its HTTP layer wants. See
    // `config.rs`'s `From` impls for the field-for-field bridge between them.
    let extract_cfg: ctxlake_maint::extract::SummarizeConfig = (&cfg.summarize).into();

    // A provider is only ever constructed when Tier 2 is actually configured —
    // `tier2_enabled` is the same gate `ctxlake_maint::extract` itself checks
    // first, so a `mode = "agent"` fleet never resolves an env var or builds an
    // HTTP client it isn't going to use. When it *is* enabled, an unresolvable
    // `api_key_env` must fail the whole run loudly rather than let maintenance
    // silently skip extraction forever — see `build_provider`'s doc.
    let provider: Option<Box<dyn ctxlake_maint::extract::Provider>> =
        if ctxlake_maint::extract::tier2_enabled(&extract_cfg) {
            let batch_cfg = extract_cfg
                .batch
                .as_ref()
                .expect("tier2_enabled just confirmed cfg.batch.is_some()");
            Some(
                ctxlake_maint::extract::build_provider(batch_cfg)
                    .context("resolving the Tier 2 provider from [summarize.batch]")?,
            )
        } else {
            None
        };
    let tier2 = provider.as_deref().map(|provider| Tier2 {
        cfg: &extract_cfg,
        provider,
    });

    // The gate always runs, `tier2` or not: claims also arrive via the
    // `memory_propose` MCP tool, so `mode = "agent"` fleets still have
    // candidates for it to promote or contest even with Tier 2 off.
    let report = ctxlake_maint::run::run(
        store.as_ref(),
        &cfg.fleet_id,
        agent_reads_enabled(cfg.summarize.mode),
        tier2,
    )
    .await
    .context("running the maintenance chain")?;

    Ok(format!(
        "compacted {} date(s) · {} digest(s) written, {} already done · snapshot {} · {} · {}",
        report.dates_compacted.len(),
        report.digests_written,
        report.digests_skipped,
        describe_snapshot(&report.snapshot),
        describe_extraction(&report.extraction),
        describe_gate(&report.gate),
    ))
}

/// Build the Tier 2 provider and throw it away, purely to find out whether it *can*
/// be built.
///
/// The sync daemon calls this once before it starts, so a `[summarize.batch]` naming
/// an env var nobody exported fails at startup — loudly, with `EX_CONFIG`, which the
/// service unit declines to restart — instead of failing identically on every
/// maintenance tick forever inside a log file nobody is reading. `ctxlake sync
/// install` runs the same check ahead of writing a unit at all.
pub(crate) fn validate_tier2(cfg: &Config) -> Result<()> {
    let extract_cfg: ctxlake_maint::extract::SummarizeConfig = (&cfg.summarize).into();
    if !ctxlake_maint::extract::tier2_enabled(&extract_cfg) {
        return Ok(());
    }
    let batch_cfg = extract_cfg
        .batch
        .as_ref()
        .expect("tier2_enabled just confirmed cfg.batch.is_some()");
    ctxlake_maint::extract::build_provider(batch_cfg)
        .context("resolving the Tier 2 provider from [summarize.batch]")?;
    Ok(())
}

/// One line an operator can read, rather than the struct's `Debug`.
///
/// The distinction that matters here is "published" vs "already current": on a fleet
/// running `ctxlake maint` from cron on every host, *most* runs legitimately publish
/// nothing, because another host already produced byte-identical output. Printing
/// that as a normal outcome rather than a warning is what makes running it everywhere
/// feel correct instead of alarming.
fn describe_snapshot(o: &ctxlake_maint::snapshot::SnapshotOutcome) -> String {
    let short = o.content_hash.get(..12).unwrap_or(&o.content_hash);
    let state = match (o.blob_written, o.pointer_updated) {
        (_, true) => "published",
        (true, false) => "written, pointer already current",
        (false, false) => "already current",
    };
    let reads = if o.agent_reads_enabled {
        ""
    } else {
        " · shadow: not served to agents"
    };
    format!("{short} ({} claim(s), {state}){reads}", o.claim_count)
}

/// One line for Tier 2 extraction — `None` when `[summarize.batch]` isn't
/// configured (`mode = "agent"` or `"none"`), matching `describe_snapshot`'s
/// "read as normal, not as a warning" tone: a fleet that hasn't turned Tier 2 on
/// is a configuration, not a failure.
fn describe_extraction(o: &Option<ctxlake_maint::extract::ExtractRunSummary>) -> String {
    match o {
        Some(e) => format!(
            "tier 2: {} session(s) extracted, {} claim(s) proposed",
            e.sessions_processed, e.claims_proposed
        ),
        None => "tier 2: disabled".to_string(),
    }
}

/// One line for the promotion gate — always printed, because the gate always
/// runs: claims arrive from batch extraction *and* from the `memory_propose`
/// MCP tool, so it has work to do even under `mode = "agent"`.
fn describe_gate(g: &ctxlake_maint::gate::GateRunSummary) -> String {
    let quarantine = if g.blocked_by_quarantine > 0 || g.demoted_by_quarantine > 0 {
        format!(
            ", {} blocked and {} demoted by quarantine",
            g.blocked_by_quarantine, g.demoted_by_quarantine
        )
    } else {
        String::new()
    };
    format!(
        "gate: {} promoted, {} contested pair(s), {} sent to review{quarantine}",
        g.promoted, g.contested_pairs, g.sent_to_review
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BatchConfig, ProviderKind};

    fn cfg(dir: &std::path::Path, fleet: &str, agent: &str) -> Config {
        Config::new(format!("file://{}", dir.display()), fleet, agent)
    }

    #[test]
    fn shadow_and_none_are_the_only_modes_that_withhold_claims_from_agents() {
        // This boolean is the entire enforcement point for shadow mode: it is
        // checked where claims are *read*, not where config is parsed, so getting it
        // backwards would silently serve claims into every session in the fleet
        // while `ctxlake config` still printed `mode = "shadow"`.
        assert!(!agent_reads_enabled(SummarizeMode::Shadow));
        assert!(!agent_reads_enabled(SummarizeMode::None));
        for mode in [
            SummarizeMode::Agent,
            SummarizeMode::Batch,
            SummarizeMode::Both,
        ] {
            assert!(agent_reads_enabled(mode), "{mode} must serve claims");
        }
    }

    #[test]
    fn the_snapshot_summary_distinguishes_published_from_already_current() {
        use ctxlake_maint::snapshot::SnapshotOutcome;
        let base = SnapshotOutcome {
            content_hash: "03b40624c5b99ee754e344c426cfe919".into(),
            claim_count: 7,
            agent_reads_enabled: true,
            blob_written: true,
            pointer_updated: true,
        };
        assert!(describe_snapshot(&base).contains("published"));
        assert!(describe_snapshot(&base).contains("03b40624c5b9"));

        // The common case on a fleet running maint from cron everywhere: another
        // host already published byte-identical output. It must not read as failure.
        let noop = SnapshotOutcome {
            blob_written: false,
            pointer_updated: false,
            ..base.clone()
        };
        let text = describe_snapshot(&noop);
        assert!(text.contains("already current"), "{text}");
        assert!(!text.to_lowercase().contains("fail"), "{text}");

        // Shadow mode is visible in the line, because a silent shadow is the one way
        // an operator can believe claims are reaching agents when they are not.
        let shadow = SnapshotOutcome {
            agent_reads_enabled: false,
            ..base
        };
        assert!(describe_snapshot(&shadow).contains("shadow"));
    }

    #[test]
    fn describe_extraction_distinguishes_disabled_from_a_real_run() {
        assert_eq!(describe_extraction(&None), "tier 2: disabled");
        let summary = ctxlake_maint::extract::ExtractRunSummary {
            sessions_processed: 3,
            claims_proposed: 5,
        };
        let text = describe_extraction(&Some(summary));
        assert!(text.contains('3'), "{text}");
        assert!(text.contains('5'), "{text}");
        assert!(!text.contains("disabled"), "{text}");
    }

    #[test]
    fn describe_gate_reports_promotions_and_only_mentions_quarantine_when_it_fired() {
        let quiet = ctxlake_maint::gate::GateRunSummary {
            promoted: 2,
            contested_pairs: 1,
            sent_to_review: 4,
            blocked_by_quarantine: 0,
            demoted_by_quarantine: 0,
        };
        let text = describe_gate(&quiet);
        assert!(text.contains("2 promoted"), "{text}");
        assert!(text.contains("1 contested"), "{text}");
        assert!(text.contains("4 sent to review"), "{text}");
        assert!(
            !text.to_lowercase().contains("quarantine"),
            "a quiet run must not mention quarantine at all: {text}"
        );

        let loud = ctxlake_maint::gate::GateRunSummary {
            blocked_by_quarantine: 1,
            demoted_by_quarantine: 2,
            ..quiet
        };
        let text = describe_gate(&loud);
        assert!(text.contains("1 blocked"), "{text}");
        assert!(text.contains("2 demoted"), "{text}");
    }

    #[tokio::test]
    async fn a_run_on_an_empty_store_succeeds_and_needs_no_initialization() {
        // The lease this command used to take had to be *provisioned* by `ctxlake
        // init` first, so a store nobody had run `init` against made `ctxlake maint`
        // fail with a message about provisioning. With the lease gone there is
        // nothing to provision and nothing to fail on: an empty store simply has no
        // work in it.
        let dir = tempfile::tempdir().unwrap();
        run(&cfg(dir.path(), "myteam", "cc-01"), true)
            .await
            .expect("an un-initialized store has no work, not an error");
    }

    #[tokio::test]
    async fn a_batch_mode_run_fails_loudly_when_the_configured_env_var_is_unset() {
        // Tier 2 misconfiguration must not fail silently: a fleet that turned
        // batch mode on and then never set the key should see maintenance stop
        // and say why, not quietly run compaction/digest/gate forever while
        // extraction does nothing.
        let var = "CTXLAKE_TEST_MAINT_CMD_MISSING_KEY";
        std::env::remove_var(var);
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path(), "myteam", "cc-01");
        c.summarize.mode = SummarizeMode::Shadow;
        c.summarize.batch = Some(BatchConfig {
            provider: ProviderKind::Anthropic,
            model: "claude-haiku-4-5".into(),
            api_key_env: var.into(),
            base_url: None,
            use_batch_api: true,
            max_sessions_per_run: 50,
            max_input_tokens: 8000,
        });
        let err = run(&c, true).await.unwrap_err();
        // `{:#}` (the "alternate" Display anyhow uses to walk the full `.source()`
        // chain — see `main.rs`'s own `eprintln!("Error: {err:#}")`) is what an
        // operator actually sees; a plain `{err}` shows only the outer
        // `.context(...)` line and would miss the variable name entirely.
        let full = format!("{err:#}");
        assert!(
            full.contains(var),
            "error must name the unset env var: {full}"
        );
    }

    #[tokio::test]
    async fn two_hosts_running_at_once_both_succeed() {
        // The property that replaced the lease, asserted at the level the lease used
        // to live at. `ctxlake-maint`'s own suite proves the chain is idempotent by
        // content; this proves the CLI no longer gates on anything that would make
        // one of these two report "held elsewhere" and skip its work.
        let dir = tempfile::tempdir().unwrap();
        let a = cfg(dir.path(), "myteam", "cc-01");
        let b = cfg(dir.path(), "myteam", "cc-02");
        let (ra, rb) = tokio::join!(run(&a, true), run(&b, true));
        ra.expect("host A");
        rb.expect("host B");
    }
}
