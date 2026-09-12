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
        run_one_cycle(cfg).await?;
        if once {
            return Ok(());
        }
        tokio::time::sleep(MAINT_LOOP_INTERVAL).await;
    }
}

async fn run_one_cycle(cfg: &Config) -> Result<()> {
    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;
    let store = store_ctx::prefixed_store(&ctx);

    let report = ctxlake_maint::run::run(
        store.as_ref(),
        &cfg.fleet_id,
        agent_reads_enabled(cfg.summarize.mode),
    )
    .await
    .context("running the maintenance chain")?;

    println!(
        "compacted {} date(s) · {} digest(s) written, {} already done · snapshot {}",
        report.dates_compacted.len(),
        report.digests_written,
        report.digests_skipped,
        describe_snapshot(&report.snapshot),
    );
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

#[cfg(test)]
mod tests {
    use super::*;

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
