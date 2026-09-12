//! `run` — the maintenance chain: compact, digest, publish. See
//! `docs/architecture.md`'s maintenance-chain diagram.
//!
//! **No lock guards this, and none is needed.** An earlier version of this
//! function acquired a fleet-wide maintenance lease before doing anything, and
//! reported "lease held elsewhere" rather than run at all if it lost the race.
//! That lease is gone: every step this chain calls is already idempotent by
//! content, so two hosts running `run` at once — the case the lease was there to
//! prevent — do redundant work at worst, never conflicting or corrupt work.
//!
//! - [`compact::run`] writes into `sessions/compacted/dt=.../fleet=.../gen=<hash
//!   of the exact sealed-session set it folded in>/` (see
//!   `ctxlake_store::layout::sessions_compacted_part`'s doc). Two hosts compacting
//!   the same sessions compute the same hash and write the same bytes to the same
//!   directory; a different session set lands in a different one. Neither can
//!   overwrite or half-write the other's output, and a reader following
//!   `_COMPACTED` never sees a torn generation.
//! - [`digest::run_for_session`] recomputes a pure function of a sealed session's
//!   own segments and overwrites in place — recomputing it twice, from two hosts
//!   or from a retry, always reproduces the same bytes (see
//!   `ctxlake_store::layout::session_digest`'s doc).
//! - [`snapshot::publish`] writes an immutable, content-addressed blob and then
//!   CAS-swaps `snapshot/latest.json` to point at it — the same write-then-CAS-
//!   swap pattern every other content-addressed publish in this design uses.
//!
//! So concurrent `ctxlake maint` runs, on any number of hosts, at any overlap, are
//! safe: a compaction generation is content-addressed, an extraction marker
//! (`ctxlake_maint::extract::mark_extracted_if_new`) is a genuine create-if-absent
//! claim so exactly one host extracts a given session, and the snapshot pointer
//! only ever moves forward via CAS. **Do not reintroduce a lock here.** If a
//! genuinely new step is added to this chain and it turns out not to be safe
//! under concurrent runs, the fix is to make *that step* idempotent by content —
//! a new marker, a new content-addressed directory — not to wrap the whole chain
//! in exclusivity again.
//!
//! There is deliberately no fallback that does any of compact/digest/snapshot
//! *without* the chain actually running. If nothing is scheduled to call `run`,
//! nothing runs, and nothing breaks; that is correct, not a gap to paper over.

use object_store::ObjectStore;

use crate::error::MaintError;
use crate::{compact, digest, snapshot};

/// What one full [`run`] call did.
#[derive(Debug, Clone, PartialEq)]
pub struct MaintenanceReport {
    pub dates_compacted: Vec<compact::CompactionOutcome>,
    pub digests_written: usize,
    pub digests_skipped: usize,
    pub snapshot: snapshot::SnapshotOutcome,
}

/// Run the full maintenance chain for `fleet_id`: compact -> digest -> publish.
///
/// `agent_reads_enabled` is the boolean form of `docs/memory.md`'s
/// `[summarize] mode` (`shadow`/`none` -> `false`, everything else -> `true`) —
/// see `snapshot`'s module doc for exactly what it does and does not gate. It is
/// a plain `bool`, not that config enum, because no wave has wired
/// `ctxlake.toml`-reading into this crate yet; whoever adds that wiring computes
/// this value and passes it in, rather than this function reaching into a config
/// module this crate's task brief excludes ("NOT extraction or gates").
///
/// Safe to call concurrently, from any number of hosts, over the same input —
/// see the module doc for why.
pub async fn run(
    store: &dyn ObjectStore,
    fleet_id: &str,
    agent_reads_enabled: bool,
) -> Result<MaintenanceReport, MaintError> {
    let mut dates_compacted = Vec::new();
    for date in compact::discover_dates(store).await? {
        dates_compacted.push(compact::run(store, &date, fleet_id).await?);
    }

    let thresholds = digest::FrictionThresholds::default();
    let mut digests_written = 0usize;
    let mut digests_skipped = 0usize;
    for marker in digest::discover_sealed_sessions(store, fleet_id).await? {
        match digest::run_for_session(store, &marker, &thresholds).await? {
            digest::DigestOutcome::Written(_) => digests_written += 1,
            digest::DigestOutcome::Skipped => digests_skipped += 1,
        }
    }

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
    use object_store::memory::InMemory;
    use object_store::{ObjectStoreExt, PutPayload};
    use std::sync::Arc;

    async fn seal_a_session(store: &dyn ObjectStore, session_id: &str) {
        let mut e = Envelope::new(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            session_id,
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
            session_id,
            0,
        );
        store.put(&seg, PutPayload::from(bytes)).await.unwrap();
        let sealed = ctxlake_store::layout::session_sealed(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            session_id,
        );
        store
            .put(&sealed, PutPayload::from_static(b"{}"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_full_run_compacts_digests_and_publishes() {
        let store = InMemory::new();
        seal_a_session(&store, "sess-1").await;

        let report = run(&store, "oxidant", true).await.unwrap();
        assert_eq!(report.dates_compacted.len(), 1);
        assert_eq!(report.dates_compacted[0].sealed_session_count, 1);
        assert_eq!(report.digests_written, 1);
        assert_eq!(report.digests_skipped, 0);
        assert_eq!(report.snapshot.claim_count, 0);
    }

    #[tokio::test]
    async fn a_second_run_right_after_is_a_clean_no_op_not_a_duplication() {
        let store = InMemory::new();
        seal_a_session(&store, "sess-1").await;

        run(&store, "oxidant", true).await.unwrap();
        let second = run(&store, "oxidant", true).await.unwrap();
        assert!(
            second.dates_compacted[0].skipped,
            "compaction must recognize the partition as unchanged"
        );
        assert_eq!(second.digests_skipped, 1, "the digest must already exist");
        assert_eq!(second.digests_written, 0);
    }

    #[tokio::test]
    async fn a_run_that_finds_nothing_still_exits_cleanly() {
        let store = InMemory::new();
        let report = run(&store, "oxidant", true).await.unwrap();
        assert!(report.dates_compacted.is_empty());
        assert_eq!(report.digests_written, 0);
        assert_eq!(report.snapshot.claim_count, 0);
    }

    /// `run`'s `agent_reads_enabled` parameter must actually reach `snapshot::publish`
    /// — not get lost or hardcoded somewhere in the chain — since it is the only
    /// thing standing between a shadow-mode fleet and a servable snapshot. See
    /// `snapshot`'s own test suite for what the flag does once it arrives there.
    #[tokio::test]
    async fn agent_reads_enabled_threads_through_to_the_published_snapshot() {
        let store = InMemory::new();
        let report = run(&store, "oxidant", false).await.unwrap();
        assert!(
            !report.snapshot.agent_reads_enabled,
            "run(..., agent_reads_enabled: false) must not silently publish a \
             reads-enabled snapshot"
        );
    }

    /// This is the test that replaces the maintenance lease: two hosts calling
    /// `run` over the exact same sealed sessions, genuinely concurrently (real OS
    /// threads, not just two futures on one executor — `InMemory`'s operations
    /// never actually suspend, so a single-threaded `join!` would just run them
    /// back to back and prove nothing about contention), must converge on one
    /// compaction generation and one coherent, readable snapshot — never two
    /// different generations for the same input, never a pointer naming a blob
    /// that doesn't exist.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_runs_over_the_same_input_converge_on_one_generation_and_one_snapshot() {
        for _ in 0..10 {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            seal_a_session(store.as_ref(), "sess-1").await;
            seal_a_session(store.as_ref(), "sess-2").await;

            let store_a = store.clone();
            let store_b = store.clone();
            let (a, b) = tokio::join!(
                tokio::spawn(async move { run(store_a.as_ref(), "oxidant", true).await }),
                tokio::spawn(async move { run(store_b.as_ref(), "oxidant", true).await }),
            );
            let report_a = a.unwrap().unwrap();
            let report_b = b.unwrap().unwrap();

            assert_eq!(
                report_a.dates_compacted[0].generation, report_b.dates_compacted[0].generation,
                "two hosts compacting the same sealed-session set must compute the \
                 identical generation, never two different directories for the same input"
            );
            assert_eq!(
                report_a.snapshot.content_hash, report_b.snapshot.content_hash,
                "two hosts publishing from the same claim history must compute the \
                 identical content-addressed snapshot"
            );

            // One coherent snapshot at the end: the pointer names a blob that
            // actually exists and matches what both runs computed.
            let pointer = store
                .get(&ctxlake_store::layout::snapshot_latest())
                .await
                .unwrap();
            let pointer: serde_json::Value =
                serde_json::from_slice(&pointer.bytes().await.unwrap()).unwrap();
            assert_eq!(
                pointer["content_hash"].as_str().unwrap(),
                report_a.snapshot.content_hash
            );
            assert!(
                store
                    .get(&ctxlake_store::layout::snapshot(
                        &report_a.snapshot.content_hash
                    ))
                    .await
                    .is_ok(),
                "the pointer must name a blob that actually exists"
            );
        }
    }
}
