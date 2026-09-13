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
/// `ctxlake maint --prune` — remove objects nothing reads any more.
///
/// Separate from the chain rather than a step in it. The chain only ever adds, which
/// is what makes running it from every host at once safe; deleting is different in
/// kind and should be something an operator asks for, not something that happens on a
/// five-minute timer they forgot they installed.
pub(crate) async fn run_prune(cfg: &Config, dry_run: bool) -> Result<()> {
    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;
    let store = store_ctx::prefixed_store(&ctx);

    let report = ctxlake_maint::prune::run(store.as_ref(), &cfg.fleet_id, dry_run)
        .await
        .context("pruning the lake")?;

    let verb = if dry_run { "would remove" } else { "removed" };
    for key in report.snapshots.iter().chain(report.legacy_live.iter()) {
        println!("  {verb} {key}");
    }
    if report.snapshots_within_grace > 0 {
        println!(
            "  kept {} superseded snapshot(s) younger than {}h — a reader that just \
             resolved the pointer may still be fetching one",
            report.snapshots_within_grace,
            ctxlake_maint::prune::SNAPSHOT_GRACE.as_secs() / 3600,
        );
    }
    match (report.total(), dry_run) {
        (0, _) => println!("nothing to prune"),
        (n, true) => println!("{n} object(s) would be removed — re-run without --dry-run"),
        (n, false) => println!("{n} object(s) removed"),
    }
    Ok(())
}

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
    // HTTP client it isn't going to use.
    //
    // **A provider that cannot be built does not fail the cycle.** This used to be
    // `?`, on the reasoning that an unresolvable `api_key_env` should fail loudly
    // rather than let maintenance silently skip extraction forever. Loudly was right;
    // *failing the cycle* was not, and a live lake showed why: compaction, digests,
    // the gates and the snapshot need no model, and every one of them stopped running
    // because an optional summarizer could not resolve. The snapshot went three and a
    // half hours stale on a lake that was still receiving sessions, and the only
    // symptom was one line about a provider.
    //
    // It is not silent either — the failure rides in this cycle's summary, which the
    // daemon prints every pass. That is the distinction the original comment was
    // reaching for: loud, without taking four working steps down with it.
    let mut tier2_unavailable: Option<String> = None;
    let provider: Option<Box<dyn ctxlake_maint::extract::Provider>> =
        if ctxlake_maint::extract::tier2_enabled(&extract_cfg) {
            let batch_cfg = extract_cfg
                .batch
                .as_ref()
                .expect("tier2_enabled just confirmed cfg.batch.is_some()");
            match ctxlake_maint::extract::build_provider(batch_cfg) {
                Ok(p) => Some(p),
                Err(e) => {
                    tier2_unavailable = Some(e.to_string());
                    None
                }
            }
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

    let extraction = match &tier2_unavailable {
        // Named in the summary rather than logged separately, so the one line the
        // daemon prints per cycle carries both what ran and what could not.
        Some(why) => format!("extraction UNAVAILABLE ({why})"),
        None => describe_extraction(&report.extraction),
    };

    Ok(format!(
        "compacted {} date(s) · {} digest(s) written, {} already done · snapshot {} · {} · {}",
        report.dates_compacted.len(),
        report.digests_written,
        report.digests_skipped,
        describe_snapshot(&report.snapshot),
        extraction,
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
        // `kept/returned` rather than one number: they diverge exactly when the model
        // answered and `claim_from_raw` discarded it — an unparseable claim_type, or a
        // citation resolving to no captured message. One number made a prompt problem
        // and a parser bug indistinguishable.
        Some(e) if e.sessions_failed > 0 => format!(
            "tier 2: {} session(s) extracted, {} claim(s) proposed, {} FAILED (retried \
             next pass; last: {})",
            e.sessions_processed,
            e.claims_proposed,
            e.sessions_failed,
            e.last_error.as_deref().unwrap_or("unknown"),
        ),
        // Name the cause. "7 DROPPED" sent me reading source to find out whether the
        // model had named a type we do not have or cited something we could not
        // resolve; they are different bugs, and the line now says which.
        Some(e) if e.claims_returned != e.claims_proposed => format!(
            "tier 2: {} session(s) extracted, {} of {} claim(s) kept ({} DROPPED: {} \
             unresolvable citation, {} bad claim_type)",
            e.sessions_processed,
            e.claims_proposed,
            e.claims_returned,
            e.claims_returned.saturating_sub(e.claims_proposed),
            e.dropped_unresolvable,
            e.dropped_bad_type,
        ),
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
            claims_returned: 0,
            sessions_failed: 0,
            last_error: None,
            sessions_processed: 3,
            claims_proposed: 5,
            ..Default::default()
        };
        let text = describe_extraction(&Some(summary));
        assert!(text.contains('3'), "{text}");
        assert!(text.contains('5'), "{text}");
        assert!(!text.contains("disabled"), "{text}");
    }

    /// A drop count with no cause is what sent an operator reading source to find out
    /// whether the model had named an unknown type or cited an unresolvable message.
    #[test]
    fn a_dropped_claim_is_reported_with_the_cause_that_dropped_it() {
        let summary = ctxlake_maint::extract::ExtractRunSummary {
            sessions_processed: 4,
            claims_returned: 27,
            claims_proposed: 20,
            dropped_unresolvable: 7,
            dropped_bad_type: 0,
            ..Default::default()
        };
        let text = describe_extraction(&Some(summary));
        assert!(text.contains("7 DROPPED"), "{text}");
        assert!(
            text.contains("7 unresolvable citation"),
            "the cause must be named, not left to a source read: {text}"
        );
        assert!(text.contains("0 bad claim_type"), "{text}");
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

    /// Build a config whose Tier 2 provider cannot possibly resolve.
    fn cfg_with_unbuildable_tier2(dir: &std::path::Path, var: &str) -> Config {
        let mut c = cfg(dir, "myteam", "cc-01");
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
        c
    }

    #[tokio::test]
    async fn an_unbuildable_provider_does_not_stop_compaction_digests_gates_or_the_snapshot() {
        // Found on a live lake. The daemon was up (v0.1.6 made an unreachable provider
        // non-fatal at startup) and the snapshot was still three and a half hours
        // stale, on a bucket that was receiving sessions the whole time — because
        // every maintenance cycle aborted on `build_provider` before reaching any of
        // the four steps that need no model at all.
        //
        // This is the same mistake as the startup one, one level down: an optional
        // step taking mandatory ones with it. Fixing the outer boundary and not this
        // one left the daemon alive and the chain dead, which is arguably worse —
        // `sync status` reports a healthy daemon.
        let var = "CTXLAKE_TEST_MAINT_CMD_MISSING_KEY";
        // SAFETY: a name unique to this test; nothing else reads it.
        unsafe { std::env::remove_var(var) };
        let dir = tempfile::tempdir().unwrap();

        let summary = run_one_cycle(&cfg_with_unbuildable_tier2(dir.path(), var))
            .await
            .expect("the chain must complete without a usable Tier 2 provider");

        // The four model-independent steps all reported.
        for step in ["compacted", "digest", "snapshot", "gate"] {
            assert!(
                summary.contains(step),
                "'{step}' must still run without a provider: {summary}"
            );
        }
    }

    #[tokio::test]
    async fn an_unbuildable_provider_is_named_in_the_summary_rather_than_passing_silently() {
        // The half of the original behaviour worth keeping. Maintenance quietly
        // running forever while extraction does nothing is the failure the old `?`
        // existed to prevent — it just should not have cost the rest of the chain.
        // The daemon prints this line every cycle.
        let var = "CTXLAKE_TEST_MAINT_CMD_MISSING_KEY_NAMED";
        // SAFETY: a name unique to this test; nothing else reads it.
        unsafe { std::env::remove_var(var) };
        let dir = tempfile::tempdir().unwrap();

        let summary = run_one_cycle(&cfg_with_unbuildable_tier2(dir.path(), var))
            .await
            .unwrap();

        assert!(
            summary.contains("UNAVAILABLE"),
            "a cycle that could not extract must say so: {summary}"
        );
        assert!(
            summary.contains(var),
            "and must name the unset variable, or the operator has nothing to act on: {summary}"
        );
        // Must not be confusable with the honest "Tier 2 is switched off" wording.
        assert!(
            !summary.contains("extraction disabled"),
            "a broken provider is not the same as a disabled one: {summary}"
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
