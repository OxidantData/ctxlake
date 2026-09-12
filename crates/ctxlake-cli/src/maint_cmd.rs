//! `ctxlake maint [--once]` — run the maintenance chain under the fleet-wide
//! maintenance lease (`ctxlake_store::layout::lease_maintenance`).
//!
//! **What this command is, honestly.** `ctxlake-maint` — the crate that will own
//! compaction, Tier 0 digests, Tier 2 extraction, the four promotion gates, and
//! snapshot publish (docs/summarization.md) — is an empty scaffold as of this
//! wave, built by a separate wave-3 track (this wiring wave's task brief: "Do not
//! touch crates/ctxlake-maint internals — other agents own those"). This command
//! cannot call a chain that does not exist yet.
//!
//! What it delivers instead is the coordination half, which *is* ready and is the
//! part most worth getting right first: acquire the lease so at most one host
//! ever runs maintenance at a time, exit 0 quietly when another host already holds
//! it (AGENTS.md invariant 5 — advisory, and the fleet should never see this as an
//! error), run whatever chain exists, release, and optionally repeat on an
//! interval. That "exit 0 quietly on contention" is exactly what makes a cron
//! entry or systemd timer *optional* rather than required, per docs/cli.md: point
//! one at every host, or none at all, and nothing breaks either way — see
//! docs/summarization.md's own framing of Tier 2 as batch, not latency-sensitive.
//!
//! Wiring in the real chain, once `ctxlake-maint` ships one, is a one-line change
//! to [`run_chain`] — replacing its honest "nothing to run yet" report with an
//! actual call — not a redesign of this module.

use std::time::Duration;

use anyhow::{Context, Result};
use ctxlake_store::layout;
use ctxlake_store::lease::{self, AcquireOutcome};

use crate::config::Config;
use crate::store_ctx;

/// Independent of `ctxlake_sync::presence::MAINTENANCE_LEASE_TTL` on purpose: that
/// constant governs the daemon's own opportunistic background heartbeat cadence,
/// while this one governs a human- or cron-triggered run of the actual chain,
/// which may legitimately take longer than a 60s heartbeat interval to finish.
/// Both lease the exact same key (`layout::lease_maintenance`), so whichever
/// caller acquires it first simply holds it until its own TTL or `release`.
const MAINT_LEASE_TTL: Duration = Duration::from_secs(300);

/// How long between chain runs when not `--once`. Generous on purpose:
/// maintenance is definitionally not latency-sensitive (docs/summarization.md's
/// Tier 2 section calls batch extraction "half the price" of a live call
/// specifically because nothing is waiting on it).
const MAINT_LOOP_INTERVAL: Duration = Duration::from_secs(300);

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
    let key = store_ctx::full_path(&ctx, &layout::lease_maintenance());

    match lease::acquire(
        ctx.store.as_ref(),
        ctx.clock.as_ref(),
        &key,
        &cfg.agent_id,
        Some("ctxlake maint"),
        MAINT_LEASE_TTL,
    )
    .await
    .with_context(|| "acquiring the maintenance lease")?
    {
        AcquireOutcome::NotAcquired { holder, .. } => {
            // AGENTS.md invariant 5: another host holding this lease is the
            // expected, common case in a fleet — never an error, never a nonzero
            // exit, never a loud message. This is the behavior that lets an
            // operator run `ctxlake maint` from cron on *every* host without ever
            // seeing a spurious failure on the ones that lost the race.
            println!(
                "maintenance lease held by {} — nothing to do",
                holder.as_deref().unwrap_or("someone else")
            );
            Ok(())
        }
        AcquireOutcome::Acquired(handle) => {
            let result = run_chain(cfg).await;
            // Release even if the chain returned an error — an error mid-chain
            // must not wedge the lease for its whole TTL when the next attempt
            // (this host or another) could otherwise retry immediately.
            let _ = lease::release(ctx.store.as_ref(), handle).await;
            result
        }
    }
}

/// The actual chain: compaction, Tier 0 digests, Tier 2 extraction, the four
/// promotion gates, snapshot publish. See this module's doc for why there is
/// nothing to call yet, and why that is reported honestly (the same house rule
/// `ctxlake-mcp`'s `memory_search` already applies to its own sibling gap:
/// `enabled: false` with a plain reason, never a fabricated result) rather than
/// silently no-op'd.
async fn run_chain(_cfg: &Config) -> Result<()> {
    println!(
        "maintenance lease acquired, but ctxlake-maint has no chain to run yet \
         (compaction/extraction/gates/snapshot — see docs/summarization.md); \
         releasing the lease"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_store::lease::LeaseState;

    fn cfg(dir: &std::path::Path, fleet: &str, agent: &str) -> Config {
        Config::new(format!("file://{}", dir.display()), fleet, agent)
    }

    #[tokio::test]
    async fn once_with_the_lease_already_held_exits_ok_and_does_no_work() {
        let dir = tempfile::tempdir().unwrap();
        let holder_cfg = cfg(dir.path(), "myteam", "cc-holder");
        let ctx = store_ctx::connect(&holder_cfg, "cc-holder").unwrap();
        let key = store_ctx::full_path(&ctx, &layout::lease_maintenance());
        lease::provision(ctx.store.as_ref(), &key).await.unwrap();
        let handle = match lease::acquire(
            ctx.store.as_ref(),
            ctx.clock.as_ref(),
            &key,
            "cc-holder",
            Some("holding it for the test"),
            Duration::from_secs(300),
        )
        .await
        .unwrap()
        {
            AcquireOutcome::Acquired(h) => h,
            AcquireOutcome::NotAcquired { .. } => panic!("expected to win an uncontended lease"),
        };

        let contender_cfg = cfg(dir.path(), "myteam", "cc-contender");
        // Must return Ok — contention is advisory and expected, not an error.
        run(&contender_cfg, true).await.unwrap();

        // And must genuinely have done nothing: the lease is still held by the
        // original holder, unchanged.
        let state: LeaseState = lease::read(ctx.store.as_ref(), &key).await.unwrap();
        assert_eq!(state.holder.as_deref(), Some("cc-holder"));
        assert_eq!(
            state.epoch, handle.epoch,
            "a contender that lost the race must not have touched the lease at all"
        );
    }

    #[tokio::test]
    async fn once_with_no_contention_acquires_runs_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_a = cfg(dir.path(), "myteam", "cc-01");
        let ctx = store_ctx::connect(&cfg_a, "cc-01").unwrap();
        let key = store_ctx::full_path(&ctx, &layout::lease_maintenance());
        lease::provision(ctx.store.as_ref(), &key).await.unwrap();

        run(&cfg_a, true).await.unwrap();

        let state: LeaseState = lease::read(ctx.store.as_ref(), &key).await.unwrap();
        assert_eq!(
            state.holder, None,
            "a completed --once run must release the lease, not hold it until TTL"
        );
        assert_eq!(state.epoch, 1, "exactly one acquire/release cycle happened");
    }

    #[tokio::test]
    async fn a_never_initialized_store_fails_clearly_rather_than_hanging_or_panicking() {
        // `ctxlake init` provisions `lease_maintenance` once, single-writer,
        // before any contender exists (`init.rs`'s own
        // `writes_config_and_provisions_the_maintenance_lease` test covers that
        // half). A store nobody has ever run `init` against is a real, reachable
        // operator mistake — `run` must report it plainly, not hang waiting on a
        // lease that will never materialize itself.
        let dir = tempfile::tempdir().unwrap();
        let cfg_a = cfg(dir.path(), "myteam", "cc-01"); // deliberately never provisioned
        let err = run(&cfg_a, true).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("provision"),
            "expected a pointer to provisioning/`ctxlake init`, got: {msg}"
        );
    }
}
