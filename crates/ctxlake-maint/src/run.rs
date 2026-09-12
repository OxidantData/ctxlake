//! `run` — the maintenance chain under the lease. See `docs/architecture.md`'s
//! maintenance-chain diagram.
//!
//! Acquires `live/leases/_maintenance` ([`ctxlake_store::lease`]), renews it at each
//! phase boundary, runs compact -> digest -> snapshot, releases cleanly. **If the
//! lease cannot be acquired, this returns [`RunOutcome::LeaseHeldElsewhere`] — not
//! an error.** Someone else is already doing this work, which is the system
//! behaving correctly, not a failure to report.
//!
//! There is deliberately no fallback that does any of compact/digest/snapshot
//! *without* holding the lease. That fallback is exactly what would turn "nothing
//! holds the lease and no agent is running" into standing background compute — the
//! zero-standing-compute property this whole design trades on. If nothing is
//! scheduled to call `run`, nothing runs, and nothing breaks; that is correct, not a
//! gap to paper over.

use std::time::Duration;

use ctxlake_store::clock::Clock;
use ctxlake_store::lease::{self, LeaseHandle};
use object_store::ObjectStore;

use crate::error::MaintError;
use crate::{compact, digest, snapshot};

/// Matches `docs/architecture.md`'s documented default lease TTL.
pub const LEASE_TTL: Duration = Duration::from_secs(5 * 60);

/// What one full [`run`] call did, when it actually ran.
#[derive(Debug, Clone, PartialEq)]
pub struct MaintenanceReport {
    pub dates_compacted: Vec<compact::CompactionOutcome>,
    pub digests_written: usize,
    pub digests_skipped: usize,
    pub snapshot: snapshot::SnapshotOutcome,
}

/// The result of one [`run`] call.
#[derive(Debug)]
pub enum RunOutcome {
    Ran(MaintenanceReport),
    /// Someone else holds the maintenance lease and its TTL hasn't lapsed. The
    /// correct, expected outcome whenever more than one host's scheduler fires close
    /// together — see the module doc.
    LeaseHeldElsewhere,
}

/// Run the full maintenance chain for `fleet_id`, under the maintenance lease,
/// identifying this attempt as `holder` (an operator- or host-stable string — see
/// `docs/coordination.md` on lease holder identity).
///
/// `agent_reads_enabled` is the boolean form of `docs/summarization.md`'s
/// `[summarize] mode` (`shadow`/`none` -> `false`, everything else -> `true`) —
/// see `snapshot`'s module doc for exactly what it does and does not gate. It is
/// a plain `bool`, not that config enum, because no wave has wired
/// `ctxlake.toml`-reading into this crate yet; whoever adds that wiring computes
/// this value and passes it in, rather than this function reaching into a config
/// module this crate's task brief excludes ("NOT extraction or gates").
pub async fn run(
    store: &dyn ObjectStore,
    clock: &dyn Clock,
    fleet_id: &str,
    holder: &str,
    agent_reads_enabled: bool,
) -> Result<RunOutcome, MaintError> {
    let lease_key = ctxlake_store::layout::lease_maintenance();
    // A one-time, pre-contention step normally run by `ctxlake init` (see
    // `lease.rs`'s module doc). Calling it here too costs one cheap GET once the
    // key already exists (`provision` is a no-op in that case) and means a fleet
    // that reaches its first maintenance run before `init` provisioned this key
    // doesn't hard-fail with `LeaseNotProvisioned` instead of just acquiring.
    lease::provision(store, &lease_key).await?;

    let mut handle = match lease::acquire(
        store,
        clock,
        &lease_key,
        holder,
        Some("maintenance"),
        LEASE_TTL,
    )
    .await?
    {
        lease::AcquireOutcome::Acquired(h) => h,
        lease::AcquireOutcome::NotAcquired { .. } => return Ok(RunOutcome::LeaseHeldElsewhere),
    };

    let report =
        run_maintenance_chain(store, clock, fleet_id, &mut handle, agent_reads_enabled).await;

    // Release regardless of whether the chain succeeded. A lease abandoned mid-crash
    // is exactly the advisory-expiry case AGENTS.md invariant 5 already accounts
    // for; releasing promptly on a clean error is strictly kinder to the next
    // contender than making it wait out the full TTL.
    let _ = lease::release(store, handle).await;

    Ok(RunOutcome::Ran(report?))
}

/// Renew the lease at a phase boundary. `Err` here means someone else's steal
/// already landed (our version is stale) — the chain must stop immediately rather
/// than keep writing under the belief it still holds exclusivity, per
/// `lease::renew`'s own contract: "the caller no longer holds the lease and should
/// treat their critical section as over."
async fn renew_or_stop(
    store: &dyn ObjectStore,
    clock: &dyn Clock,
    handle: &mut LeaseHandle,
) -> Result<(), MaintError> {
    if lease::renew(store, clock, handle, LEASE_TTL).await? {
        Ok(())
    } else {
        Err(MaintError::Other(
            "maintenance lease was stolen mid-run; stopping rather than continuing without it"
                .to_string(),
        ))
    }
}

async fn run_maintenance_chain(
    store: &dyn ObjectStore,
    clock: &dyn Clock,
    fleet_id: &str,
    handle: &mut LeaseHandle,
    agent_reads_enabled: bool,
) -> Result<MaintenanceReport, MaintError> {
    let mut dates_compacted = Vec::new();
    for date in compact::discover_dates(store).await? {
        dates_compacted.push(compact::run(store, &date, fleet_id).await?);
    }
    renew_or_stop(store, clock, handle).await?;

    let thresholds = digest::FrictionThresholds::default();
    let mut digests_written = 0usize;
    let mut digests_skipped = 0usize;
    for marker in digest::discover_sealed_sessions(store, fleet_id).await? {
        match digest::run_for_session(store, &marker, &thresholds).await? {
            digest::DigestOutcome::Written(_) => digests_written += 1,
            digest::DigestOutcome::Skipped => digests_skipped += 1,
        }
    }
    renew_or_stop(store, clock, handle).await?;

    let snapshot = snapshot::publish(store, agent_reads_enabled).await?;

    Ok(MaintenanceReport {
        dates_compacted,
        digests_written,
        digests_skipped,
        snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::envelope::{Envelope, EventType};
    use ctxlake_core::Runtime;
    use ctxlake_store::clock::SystemClock;
    use object_store::memory::InMemory;
    use object_store::{ObjectStoreExt, PutPayload};

    async fn seal_a_session(store: &dyn ObjectStore) {
        let mut e = Envelope::new(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            "sess-1",
            EventType::ToolCall,
            "2026-09-11T18:22:00.000Z",
        );
        e.content = Some("hello".into());
        let bytes = ctxlake_sync::codec::encode(&[e]).unwrap();
        let seg = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        store.put(&seg, PutPayload::from(bytes)).await.unwrap();
        let sealed = ctxlake_store::layout::session_sealed(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
        );
        store
            .put(&sealed, PutPayload::from_static(b"{}"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_full_run_compacts_digests_and_publishes() {
        let store = InMemory::new();
        let clock = SystemClock;
        seal_a_session(&store).await;

        let outcome = run(&store, &clock, "oxidant", "host-a", true)
            .await
            .unwrap();
        let RunOutcome::Ran(report) = outcome else {
            panic!("expected the chain to run");
        };
        assert_eq!(report.dates_compacted.len(), 1);
        assert_eq!(report.dates_compacted[0].sealed_session_count, 1);
        assert_eq!(report.digests_written, 1);
        assert_eq!(report.digests_skipped, 0);
        assert_eq!(report.snapshot.claim_count, 0);

        // The lease must have been released, not left held.
        let state = lease::read(&store, &ctxlake_store::layout::lease_maintenance())
            .await
            .unwrap();
        assert_eq!(
            state.holder, None,
            "run() must release the lease when it finishes"
        );
    }

    #[tokio::test]
    async fn a_second_run_right_after_is_a_clean_no_op_not_a_duplication() {
        let store = InMemory::new();
        let clock = SystemClock;
        seal_a_session(&store).await;

        run(&store, &clock, "oxidant", "host-a", true)
            .await
            .unwrap();
        let RunOutcome::Ran(second) = run(&store, &clock, "oxidant", "host-a", true)
            .await
            .unwrap()
        else {
            panic!("lease was released; a second run must be able to acquire it");
        };
        assert!(
            second.dates_compacted[0].skipped,
            "compaction must recognize the partition as unchanged"
        );
        assert_eq!(second.digests_skipped, 1, "the digest must already exist");
        assert_eq!(second.digests_written, 0);
    }

    #[tokio::test]
    async fn two_concurrent_runs_exactly_one_does_work() {
        // `object_store::memory::InMemory`'s async operations never actually
        // suspend (nothing here is a real network call), so racing two `run()`
        // futures through `tokio::join!` would just run them back-to-back on one
        // executor thread — the first would acquire, do all its work, and release
        // *before the second is ever polled*, which proves nothing about
        // contention. What actually needs proving is `lease::acquire`'s exclusivity
        // (already covered end-to-end in `ctxlake_store::lease`'s own test suite)
        // wired correctly into `run`'s control flow — so this simulates the
        // contended case the way `ctxlake_store::lease`'s own tests do
        // (`second_contender_is_refused_while_held`): host-a is already mid-run
        // (lease held, not yet released) when host-b's `run` is called.
        let store = InMemory::new();
        let clock = SystemClock;
        seal_a_session(&store).await;

        let lease_key = ctxlake_store::layout::lease_maintenance();
        lease::provision(&store, &lease_key).await.unwrap();
        let lease::AcquireOutcome::Acquired(host_a_handle) = lease::acquire(
            &store,
            &clock,
            &lease_key,
            "host-a",
            Some("maintenance"),
            LEASE_TTL,
        )
        .await
        .unwrap() else {
            panic!("host-a should have acquired the never-before-held lease");
        };

        let outcome_b = run(&store, &clock, "oxidant", "host-b", true)
            .await
            .unwrap();
        assert!(
            matches!(outcome_b, RunOutcome::LeaseHeldElsewhere),
            "host-b must not run the chain while host-a holds the lease: {outcome_b:?}"
        );
        // And host-b's attempt must not have touched anything — no compaction, no
        // digest, no snapshot — exactly the "does no work" half of the property.
        assert!(
            store
                .get(&ctxlake_store::layout::snapshot_latest())
                .await
                .is_err(),
            "a run that never acquired the lease must not have published a snapshot"
        );

        // host-a's own in-flight lease must be completely undisturbed by host-b's
        // failed attempt.
        let state = lease::read(&store, &lease_key).await.unwrap();
        assert_eq!(state.holder.as_deref(), Some("host-a"));

        lease::release(&store, host_a_handle).await.unwrap();
    }

    #[tokio::test]
    async fn a_run_that_finds_nothing_still_exits_cleanly_and_releases() {
        let store = InMemory::new();
        let clock = SystemClock;
        let RunOutcome::Ran(report) = run(&store, &clock, "oxidant", "host-a", true)
            .await
            .unwrap()
        else {
            panic!("an idle fleet still owns the lease briefly to publish an empty snapshot");
        };
        assert!(report.dates_compacted.is_empty());
        assert_eq!(report.digests_written, 0);
        assert_eq!(report.snapshot.claim_count, 0);

        let state = lease::read(&store, &ctxlake_store::layout::lease_maintenance())
            .await
            .unwrap();
        assert_eq!(state.holder, None);
    }

    /// `run`'s `agent_reads_enabled` parameter must actually reach `snapshot::publish`
    /// — not get lost or hardcoded somewhere in the chain — since it is the only
    /// thing standing between a shadow-mode fleet and a servable snapshot. See
    /// `snapshot`'s own test suite for what the flag does once it arrives there.
    #[tokio::test]
    async fn agent_reads_enabled_threads_through_to_the_published_snapshot() {
        let store = InMemory::new();
        let clock = SystemClock;

        let RunOutcome::Ran(report) = run(&store, &clock, "oxidant", "host-a", false)
            .await
            .unwrap()
        else {
            panic!("expected the chain to run");
        };
        assert!(
            !report.snapshot.agent_reads_enabled,
            "run(..., agent_reads_enabled: false) must not silently publish a \
             reads-enabled snapshot"
        );
    }
}
