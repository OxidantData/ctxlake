//! `run` — the maintenance chain: compact, digest, extract, gate, publish. See
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
//! - [`extract::extract_session`] claims a session with a create-if-absent marker
//!   (`extract::mark_extracted_if_new`) before doing any work, so a re-run or a
//!   racing host extracts a given session at most once. A clean no-op end to end
//!   when `tier2` is `None` — no transcript is even fenced, let alone sent
//!   anywhere.
//! - [`gate::run`] promotes by fold semantics, not exclusivity: a repeated
//!   `Promoted` event for an already-promoted claim is a no-op (see that
//!   module's own doc), so two racing runs can't double-promote.
//! - [`snapshot::publish`] writes an immutable, content-addressed blob and then
//!   CAS-swaps `snapshot/latest.json` to point at it — the same write-then-CAS-
//!   swap pattern every other content-addressed publish in this design uses.
//!
//! So concurrent `ctxlake maint` runs, on any number of hosts, at any overlap, are
//! safe: a compaction generation is content-addressed, an extraction marker
//! (`ctxlake_maint::extract::mark_extracted_if_new`) is a genuine create-if-absent
//! claim so exactly one host extracts a given session, a promotion is idempotent
//! by fold, and the snapshot pointer only ever moves forward via CAS. **Do not
//! reintroduce a lock here.** If a genuinely new step is added to this chain and
//! it turns out not to be safe under concurrent runs, the fix is to make *that
//! step* idempotent by content — a new marker, a new content-addressed directory
//! — not to wrap the whole chain in exclusivity again.
//!
//! ## Why the gate always runs, even with `tier2: None`
//!
//! Claims don't only arrive from Tier 2 extraction — the `memory_propose` MCP
//! tool writes agent-scope candidates directly, with no LLM involved at all. A
//! fleet with no `[summarize.batch]` configured still has candidates sitting in
//! `claims/events/` that need gating; `gate::run` unconditionally runs (using
//! whatever [`gate_inputs::build`] can derive from the lake's sealed sessions) so
//! those candidates are not stranded forever just because this fleet has no LLM
//! wired up.
//!
//! ## Why the gate runs *before* `snapshot::publish`
//!
//! `snapshot::publish` folds whatever is in `claims/events/` at the moment it
//! runs into the artifact it writes. A claim the gate promotes *after* that fold
//! has already happened sits invisible — not wrong, just late by a full
//! maintenance cycle, since nothing else re-triggers a publish before the next
//! `ctxlake maint` run. Gating first means a claim promoted this run is visible
//! in the snapshot this run produces, not the next one.
//!
//! ## Why extraction and the gate each list and load sessions separately
//!
//! [`extract::run`] lists sealed sessions and loads only the transcripts of
//! whichever ones it actually extracts this call (bounded by
//! `max_sessions_per_run`, skipping anything [`extract::is_already_extracted`]
//! already marked done). [`gate_inputs::build`] needs the envelopes of *every*
//! sealed session, every run, regardless of whether extraction touched it —
//! a claim can cite a session from weeks ago, and `memory_propose` candidates
//! exist with no extraction involved at all. So this is two listing-and-loading
//! passes, not one: the small, budget-bounded set extraction just processed
//! gets its transcript loaded twice in the same run. That double-load is
//! deliberately accepted rather than threading a shared, pre-loaded transcript
//! list through `extract::run`'s own signature — `extract::run` is this crate's
//! one tested, complete entry point for Tier 2, and reaching around it here to
//! save a bounded, already-cheap-relative-to-the-LLM-call reload would trade a
//! real cost (a second, subtly different extraction code path to keep in sync)
//! for a small one.
//!
//! There is deliberately no fallback that does any of this chain's steps
//! *without* the chain actually running. If nothing is scheduled to call `run`,
//! nothing runs, and nothing breaks; that is correct, not a gap to paper over.

use object_store::ObjectStore;

use crate::error::MaintError;
use crate::{compact, digest, extract, gate, gate_inputs, snapshot};

/// What one full [`run`] call did.
#[derive(Debug)]
pub struct MaintenanceReport {
    pub dates_compacted: Vec<compact::CompactionOutcome>,
    pub digests_written: usize,
    pub digests_skipped: usize,
    /// `None` when `tier2` was `None` — extraction did not run at all, as
    /// distinct from running and finding nothing to do (`Some` with zero
    /// counts). An operator reading this can tell "no LLM configured" apart
    /// from "LLM configured, nothing new to extract."
    pub extraction: Option<extract::ExtractRunSummary>,
    pub gate: gate::GateRunSummary,
    pub snapshot: snapshot::SnapshotOutcome,
}

/// Tier 2 extraction's two real dependencies — an LLM config and a provider —
/// bundled so [`run`]'s signature carries one `Option` instead of two, and so
/// "no LLM configured" is `None` rather than a config whose own fields say the
/// same thing a different way.
pub struct Tier2<'a> {
    pub cfg: &'a extract::SummarizeConfig,
    pub provider: &'a dyn extract::Provider,
}

/// Run the full maintenance chain for `fleet_id`: compact -> digest ->
/// [extract, if `tier2` is `Some`] -> gate -> publish.
///
/// `agent_reads_enabled` is the boolean form of `docs/memory.md`'s
/// `[summarize] mode` (`shadow`/`none` -> `false`, everything else -> `true`) —
/// see `snapshot`'s module doc for exactly what it does and does not gate. It is
/// a plain `bool`, not that config enum, for the same reason `tier2` is a plain
/// `Option<Tier2>` rather than this crate reaching into `ctxlake.toml` itself:
/// no wave has wired config-reading into this crate, so whoever calls `run`
/// computes these from config and passes them in.
///
/// Safe to call concurrently, from any number of hosts, over the same input —
/// see the module doc for why.
pub async fn run(
    store: &dyn ObjectStore,
    fleet_id: &str,
    agent_reads_enabled: bool,
    tier2: Option<Tier2<'_>>,
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

    let extraction = match &tier2 {
        Some(t) => Some(extract::run(store, fleet_id, t.cfg, t.provider).await?),
        None => None,
    };

    // Every sealed session's transcript, for the gate's context inputs — see
    // the module doc for why this is a separate pass from whatever extraction
    // just did, rather than a list `extract::run` hands back.
    let session_refs = extract::list_sealed_sessions(store, fleet_id).await?;
    let mut transcripts = Vec::with_capacity(session_refs.len());
    for session_ref in &session_refs {
        transcripts.push(extract::load_transcript(store, session_ref).await?);
    }

    let inputs = gate_inputs::build(&transcripts);
    let ctx_at = crate::now_rfc3339();
    let gate_summary = gate::run(
        store,
        &ctx_at,
        &inputs.known_agents,
        &inputs.session_windows,
        &inputs.injected_context_by_session,
        |e| inputs.excerpt_resolves(e),
    )
    .await?;

    let snapshot = snapshot::publish(store, fleet_id, agent_reads_enabled).await?;

    Ok(MaintenanceReport {
        dates_compacted,
        digests_written,
        digests_skipped,
        extraction,
        gate: gate_summary,
        snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{ClaimType, Evidence, Scope};
    use ctxlake_core::envelope::{Envelope, EventType};
    use ctxlake_core::Runtime;
    use futures::future::BoxFuture;
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

    /// Like [`seal_a_session`], but with a real `message_id` on the envelope —
    /// needed by the extraction end-to-end tests below, whose provider double
    /// cites `("<session>", "m1")`: [`extract::claim_from_raw`] only resolves a
    /// citation against an envelope that actually carries that `message_id`, so
    /// a session sealed without one can never produce a citable claim.
    async fn seal_a_session_with_message(store: &dyn ObjectStore, session_id: &str) {
        let mut e = Envelope::new(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            session_id,
            EventType::Assistant,
            "2026-09-11T18:22:00.000Z",
        );
        e.message_id = Some("m1".to_string());
        e.content = Some("staging SSH listens on 2222".into());
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

        let report = run(&store, "oxidant", true, None).await.unwrap();
        assert_eq!(report.dates_compacted.len(), 1);
        assert_eq!(report.dates_compacted[0].sealed_session_count, 1);
        assert_eq!(report.digests_written, 1);
        assert_eq!(report.digests_skipped, 0);
        assert_eq!(report.snapshot.claim_count, 0);
        assert!(
            report.extraction.is_none(),
            "no tier2 config means extraction did not run at all"
        );
        assert_eq!(report.gate.promoted, 0);
    }

    #[tokio::test]
    async fn a_second_run_right_after_is_a_clean_no_op_not_a_duplication() {
        let store = InMemory::new();
        seal_a_session(&store, "sess-1").await;

        run(&store, "oxidant", true, None).await.unwrap();
        let second = run(&store, "oxidant", true, None).await.unwrap();
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
        let report = run(&store, "oxidant", true, None).await.unwrap();
        assert!(report.dates_compacted.is_empty());
        assert_eq!(report.digests_written, 0);
        assert_eq!(report.snapshot.claim_count, 0);
        assert_eq!(report.gate.promoted, 0);
    }

    /// `run`'s `agent_reads_enabled` parameter must actually reach `snapshot::publish`
    /// — not get lost or hardcoded somewhere in the chain — since it is the only
    /// thing standing between a shadow-mode fleet and a servable snapshot. See
    /// `snapshot`'s own test suite for what the flag does once it arrives there.
    #[tokio::test]
    async fn agent_reads_enabled_threads_through_to_the_published_snapshot() {
        let store = InMemory::new();
        let report = run(&store, "oxidant", false, None).await.unwrap();
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
                tokio::spawn(async move { run(store_a.as_ref(), "oxidant", true, None).await }),
                tokio::spawn(async move { run(store_b.as_ref(), "oxidant", true, None).await }),
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
                .get(&ctxlake_store::layout::snapshot_latest("oxidant"))
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

    // ---- the gate runs even with no LLM configured ----

    #[tokio::test]
    async fn a_memory_propose_style_candidate_is_gated_even_with_tier2_none() {
        // No extraction is configured at all (`tier2: None`), but a candidate
        // already sits in `claims/events/` — exactly what `memory_propose`
        // leaves behind. The gate must still run and evaluate it: claims are not
        // only ever produced by Tier 2.
        let store = InMemory::new();
        seal_a_session(&store, "sess-1").await;
        let p1 = crate::claims::ProposedClaim {
            claim_id: "c1".into(),
            claim: "staging SSH listens on 2222".into(),
            claim_type: ClaimType::Environment,
            subject: "staging".into(),
            scope: Scope::Agent,
            observed_by: "cc-01".into(),
            observed_at: "2026-09-11T18:22:00.000Z".into(),
            evidence: vec![Evidence {
                session_id: "sess-1".into(),
                message_id: "does-not-need-to-resolve".into(),
                excerpt_hash: "sha256:whatever".into(),
                observed_at: "2026-09-11T18:22:00.000Z".into(),
            }],
            embedding: None,
            resolves_at: None,
        };
        crate::claims::append_proposed(&store, "2026-09-11", &p1)
            .await
            .unwrap();

        let report = run(&store, "oxidant", true, None).await.unwrap();
        assert!(report.extraction.is_none());
        // The evidence's message_id is fabricated (no such envelope exists), so
        // provenance correctly holds it for review rather than promoting a claim
        // this gate cannot verify — proving the gate genuinely ran and genuinely
        // checked it, not that it promotes everything unconditionally.
        assert_eq!(report.gate.sent_to_review, 1);
        assert_eq!(report.gate.promoted, 0);
    }

    // ---- end-to-end: candidate -> gate -> promoted -> snapshot ----
    //
    // The guard against this project's recurring failure mode: pieces that are
    // each internally complete (extraction is tested, the gate is tested, the
    // snapshot is tested) with nothing actually wiring them together. This test
    // fails if any one link in that chain is missing.

    /// A test double modeled on `extract.rs`'s own `FixedClaimProvider` /
    /// `SessionMarkerProvider`: it always emits the same single evidence-backed
    /// claim, citing whichever session it was told about. No live API — this
    /// stands in for "the model observed a fact," which is all this test needs
    /// held constant.
    struct FixedEnvironmentClaimProvider {
        session_id: &'static str,
    }
    impl extract::Provider for FixedEnvironmentClaimProvider {
        fn complete<'a>(
            &'a self,
            _req: &'a extract::CompletionRequest,
        ) -> BoxFuture<'a, Result<String, extract::ExtractError>> {
            let body = format!(
                r#"{{"claims":[{{"claim":"staging SSH listens on 2222","claim_type":"environment","subject":"staging","evidence":[{{"session_id":"{}","message_id":"m1"}}]}}]}}"#,
                self.session_id
            );
            Box::pin(async move { Ok(body) })
        }
    }

    fn shadow_cfg() -> extract::SummarizeConfig {
        extract::SummarizeConfig {
            mode: extract::SummarizeMode::Shadow,
            batch: Some(extract::BatchConfig::default()),
        }
    }

    #[tokio::test]
    async fn a_promoted_claim_actually_reaches_the_published_snapshot() {
        let store = InMemory::new();
        seal_a_session_with_message(&store, "sess-1").await;

        let cfg = shadow_cfg();
        let provider = FixedEnvironmentClaimProvider {
            session_id: "sess-1",
        };
        let report = run(
            &store,
            "oxidant",
            true,
            Some(Tier2 {
                cfg: &cfg,
                provider: &provider,
            }),
        )
        .await
        .unwrap();

        let extraction = report
            .extraction
            .expect("tier2 was Some, extraction must have run");
        assert_eq!(extraction.sessions_processed, 1);
        assert_eq!(extraction.claims_proposed, 1);
        // `environment`'s independence threshold is 1: one real, resolving
        // citation is enough to promote outright.
        assert_eq!(
            report.gate.promoted, 1,
            "the extracted candidate must clear the gate, not sit at candidate"
        );

        let fleet_claims = crate::claims::list_fleet_claims(&store).await.unwrap();
        assert_eq!(fleet_claims.len(), 1);
        assert_eq!(fleet_claims[0].status, crate::claims::ClaimStatus::Promoted);

        // And the actual published artifact — not just the in-memory fold — has
        // to carry it, visible to agents, since this run had agent_reads_enabled
        // and the claim promoted.
        let blob = store
            .get(&ctxlake_store::layout::snapshot(
                &report.snapshot.content_hash,
            ))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &blob).unwrap();
        let conn = rusqlite::Connection::open(file.path()).unwrap();
        let (status, visible): (String, i64) = conn
            .query_row(
                "SELECT status, visible_to_agents FROM claims WHERE claim_id = ?1",
                rusqlite::params![fleet_claims[0].claim_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "promoted");
        assert_eq!(
            visible, 1,
            "a promoted claim published with agent_reads_enabled must be visible \
             in the snapshot agents actually query, not just folded in-memory"
        );
    }

    #[tokio::test]
    async fn shadow_mode_promotes_but_withholds_agent_visibility_in_the_snapshot() {
        // Same extraction and gate outcome as the test above, but
        // `agent_reads_enabled: false` — the claim still promotes (gating and
        // serving are separate concerns), it just must not be
        // FTS-searchable/visible in the published artifact.
        let store = InMemory::new();
        seal_a_session_with_message(&store, "sess-1").await;
        let cfg = shadow_cfg();
        let provider = FixedEnvironmentClaimProvider {
            session_id: "sess-1",
        };
        let report = run(
            &store,
            "oxidant",
            false,
            Some(Tier2 {
                cfg: &cfg,
                provider: &provider,
            }),
        )
        .await
        .unwrap();
        assert_eq!(report.gate.promoted, 1);

        let blob = store
            .get(&ctxlake_store::layout::snapshot(
                &report.snapshot.content_hash,
            ))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &blob).unwrap();
        let conn = rusqlite::Connection::open(file.path()).unwrap();
        let visible: i64 = conn
            .query_row("SELECT visible_to_agents FROM claims LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            visible, 0,
            "shadow mode must promote without ever making the claim agent-visible"
        );
    }
}
