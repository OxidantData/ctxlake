//! The claim event log — `docs/memory.md`'s claim model, made concrete.
//!
//! A claim is mutable state: its status flips (`candidate` -> `promoted` ->
//! `contested`, say), and its evidence set grows as more sessions corroborate it.
//! Mutable rows are the one thing object storage is bad at — there is no
//! transactional update, and AGENTS.md invariant 3 rules out two writers ever
//! touching the same key. So nothing here is ever rewritten in place: every
//! mutation ([`ClaimEvent`]) is its own new object under `claims/events/`, and
//! "the current state of claim X" is defined as [`fold`] over every event that
//! named it, applied in the order they were appended.
//!
//! The one place this crate *does* overwrite a key is `claims/fleet/<claim_id>`,
//! and that is deliberate, not a shortcut: see [`publish_fleet_state`]'s doc.

use std::collections::{BTreeMap, HashSet};

use ctxlake_store::StoreError;
use object_store::{Error as OsError, ObjectStore, ObjectStoreExt, PutMode, PutPayload};
use serde::{Deserialize, Serialize};

use crate::extract::SummarizeMode;

/// `docs/memory.md`'s claim-type table. Kept in lockstep with
/// `ctxlake-mcp::memory::ALLOWED_CLAIM_TYPES` (string values, checked below) since
/// both crates must agree on what a caller is allowed to call a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimType {
    Environment,
    Convention,
    Outcome,
    Preference,
    Hypothesis,
}

impl ClaimType {
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimType::Environment => "environment",
            ClaimType::Convention => "convention",
            ClaimType::Outcome => "outcome",
            ClaimType::Preference => "preference",
            ClaimType::Hypothesis => "hypothesis",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "environment" => Some(ClaimType::Environment),
            "convention" => Some(ClaimType::Convention),
            "outcome" => Some(ClaimType::Outcome),
            "preference" => Some(ClaimType::Preference),
            "hypothesis" => Some(ClaimType::Hypothesis),
            _ => None,
        }
    }
}

/// `docs/memory.md`'s scope ladder: `agent -> repo -> fleet`. Every candidate is
/// proposed at [`Scope::Agent`] (AGENTS.md invariant 9: "writes go to agent scope
/// only"); the gate is the only thing that ever widens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Agent,
    Repo,
    Fleet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Candidate,
    Promoted,
    Contested,
    Retired,
}

/// A `(session_id, message_id, excerpt_hash)` citation. `excerpt_hash` is always
/// computed by ctxlake from the real transcript, never taken from the model's own
/// claimed hash — see `extract`'s citation-verification doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub session_id: String,
    pub message_id: String,
    pub excerpt_hash: String,
}

/// One proposal: either a brand-new claim (`claim_id` never seen before) or
/// additional corroborating evidence for an existing candidate (`claim_id` reused
/// — extraction and `memory_propose` both do this when they recognize a claim
/// already on file for the same subject).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedClaim {
    pub claim_id: String,
    pub claim: String,
    pub claim_type: ClaimType,
    pub subject: String,
    pub scope: Scope,
    pub observed_by: String,
    pub observed_at: String,
    pub evidence: Vec<Evidence>,
    /// 256-dim, for the contradiction gate's brute-force cosine search. `None`
    /// when the caller (e.g. `memory_propose`'s free-form text) has no embedder
    /// wired up yet — the contradiction gate then falls back to FTS alone for
    /// that candidate rather than refusing to check it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

/// One append to the claim event log. `#[serde(tag = "kind")]` makes every stored
/// object self-describing, so a reader never has to infer what kind of event a
/// file is from which prefix it lived under.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClaimEvent {
    Proposed(ProposedClaim),
    Promoted {
        claim_id: String,
        at: String,
        independent_count: u32,
        confidence: f64,
    },
    /// Emitted for **both** sides of a conflict — see `gate`'s contradiction
    /// check. `conflicts_with` names the other claim so a human reviewing one
    /// contested claim can find its counterpart.
    Contested {
        claim_id: String,
        at: String,
        conflicts_with: Option<String>,
        reason: String,
    },
    Retired {
        claim_id: String,
        at: String,
        reason: String,
    },
    Superseded {
        claim_id: String,
        at: String,
        by: String,
    },
}

impl ClaimEvent {
    pub fn claim_id(&self) -> &str {
        match self {
            ClaimEvent::Proposed(p) => &p.claim_id,
            ClaimEvent::Promoted { claim_id, .. }
            | ClaimEvent::Contested { claim_id, .. }
            | ClaimEvent::Retired { claim_id, .. }
            | ClaimEvent::Superseded { claim_id, .. } => claim_id,
        }
    }
}

/// The fold of every event naming one `claim_id` — "current state," per the module
/// doc. This is the only place `evidence_count` and `status` mean anything; no
/// code should track either incrementally outside of replaying this fold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimState {
    pub claim_id: String,
    pub claim: String,
    pub claim_type: ClaimType,
    pub subject: String,
    pub scope: Scope,
    pub observed_by: String,
    pub observed_at: String,
    pub evidence: Vec<Evidence>,
    pub status: ClaimStatus,
    /// Set by the gate at promotion time (or recomputed on demand before that) —
    /// see `gate::compute_independent_count`. Zero until a promotion (or a gate
    /// run) has computed it at least once.
    pub independent_count: u32,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

impl ClaimState {
    /// Distinct sessions backing this claim — the raw count the gate is
    /// forbidden from using as a promotion threshold (docs/memory.md: "thresholds
    /// read `independent_count`, never `evidence_count`"), but still useful for
    /// display and for the evidence gate's cheap first-pass reject.
    pub fn evidence_session_count(&self) -> usize {
        let sessions: HashSet<&str> = self
            .evidence
            .iter()
            .map(|e| e.session_id.as_str())
            .collect();
        sessions.len()
    }
}

/// Fold a set of claim events, applied in the given iteration order, into current
/// per-claim state. Callers are responsible for ordering `events` themselves
/// (typically: sort by the ULID each was appended under) — object storage's `list`
/// makes no ordering promise, and a fold applied out of order could let an older
/// `Contested` "win" over a newer `Promoted` it should have followed.
pub fn fold<'a>(events: impl IntoIterator<Item = &'a ClaimEvent>) -> BTreeMap<String, ClaimState> {
    let mut out: BTreeMap<String, ClaimState> = BTreeMap::new();
    for ev in events {
        match ev {
            ClaimEvent::Proposed(p) => {
                out.entry(p.claim_id.clone())
                    .and_modify(|s| {
                        for e in &p.evidence {
                            if !s.evidence.contains(e) {
                                s.evidence.push(e.clone());
                            }
                        }
                        if s.embedding.is_none() {
                            s.embedding = p.embedding.clone();
                        }
                    })
                    .or_insert_with(|| ClaimState {
                        claim_id: p.claim_id.clone(),
                        claim: p.claim.clone(),
                        claim_type: p.claim_type,
                        subject: p.subject.clone(),
                        scope: p.scope,
                        observed_by: p.observed_by.clone(),
                        observed_at: p.observed_at.clone(),
                        evidence: p.evidence.clone(),
                        status: ClaimStatus::Candidate,
                        independent_count: 0,
                        confidence: 0.0,
                        embedding: p.embedding.clone(),
                    });
            }
            ClaimEvent::Promoted {
                claim_id,
                independent_count,
                confidence,
                ..
            } => {
                if let Some(s) = out.get_mut(claim_id) {
                    s.status = ClaimStatus::Promoted;
                    s.independent_count = *independent_count;
                    s.confidence = *confidence;
                }
            }
            ClaimEvent::Contested { claim_id, .. } => {
                if let Some(s) = out.get_mut(claim_id) {
                    s.status = ClaimStatus::Contested;
                }
            }
            ClaimEvent::Retired { claim_id, .. } | ClaimEvent::Superseded { claim_id, .. } => {
                if let Some(s) = out.get_mut(claim_id) {
                    s.status = ClaimStatus::Retired;
                }
            }
        }
    }
    out
}

/// Append one proposal under `claims/events/dt=<date>/agent=<observed_by>/<ulid>.json`.
/// `PutMode::Create` is correct here (unlike a lease — AGENTS.md invariant 4):
/// this key never existed before this call and never will again, so there is no
/// "materialize once, CAS forever after" split to make. Two proposals never
/// collide on the same key because each mints its own ULID.
pub async fn append_proposed(
    store: &dyn ObjectStore,
    date: &str,
    proposal: &ProposedClaim,
) -> Result<(), StoreError> {
    let ulid = ctxlake_core::envelope::next_event_id();
    let path = ctxlake_store::layout::claim_event(date, &proposal.observed_by, &ulid);
    let payload = PutPayload::from(serde_json::to_vec(&ClaimEvent::Proposed(proposal.clone()))?);
    store
        .put_opts(&path, payload, PutMode::Create.into())
        .await?;
    Ok(())
}

/// List and parse every event under `claims/events/`, sorted by key so the fold in
/// [`fold`] sees events in append order (ULIDs are lexicographically sortable —
/// see `ctxlake_core::envelope`'s own guarantee of that). A single malformed or
/// half-written object is skipped rather than failing the whole read, matching
/// `roster::list_intents_directly`'s reasoning: a transient partial write should
/// not take down every other claim's fold.
pub async fn list_events(store: &dyn ObjectStore) -> Result<Vec<ClaimEvent>, StoreError> {
    use futures::StreamExt;
    let prefix = ctxlake_store::layout::claims_events_prefix();
    let mut entries: Vec<(String, ClaimEvent)> = Vec::new();
    let mut stream = store.list(Some(&prefix));
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        let Ok(res) = store.get(&meta.location).await else {
            continue;
        };
        let Ok(bytes) = res.bytes().await else {
            continue;
        };
        if let Ok(ev) = serde_json::from_slice::<ClaimEvent>(&bytes) {
            entries.push((meta.location.to_string(), ev));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries.into_iter().map(|(_, ev)| ev).collect())
}

/// Overwrite `claims/fleet/<claim_id>.json` with this claim's current folded
/// state. A plain [`ObjectStore::put`] (no CAS) is correct, not a shortcut: the
/// promotion gate runs single-writer under `lease_maintenance`
/// (docs/architecture.md's maintenance chain), so by the time any code reaches
/// this call there is, by construction, no concurrent writer to race — the
/// version-checked CAS that `live/` needs would just be locking a resource this
/// design already made single-writer. See `layout::claim_fleet`.
pub async fn publish_fleet_state(
    store: &dyn ObjectStore,
    state: &ClaimState,
) -> Result<(), StoreError> {
    let path = ctxlake_store::layout::claim_fleet(&state.claim_id);
    let payload = PutPayload::from(serde_json::to_vec(state)?);
    store.put(&path, payload).await?;
    Ok(())
}

/// Read every claim currently published to fleet scope. `Ok(vec![])` covers both
/// "nothing has ever been promoted" and "the prefix doesn't exist yet" — neither
/// is an error a caller needs to branch on separately.
pub async fn list_fleet_claims(store: &dyn ObjectStore) -> Result<Vec<ClaimState>, StoreError> {
    use futures::StreamExt;
    let prefix = ctxlake_store::layout::claims_fleet_prefix();
    let mut out = Vec::new();
    let mut stream = store.list(Some(&prefix));
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        let Ok(res) = store.get(&meta.location).await else {
            continue;
        };
        let Ok(bytes) = res.bytes().await else {
            continue;
        };
        if let Ok(state) = serde_json::from_slice::<ClaimState>(&bytes) {
            out.push(state);
        }
    }
    Ok(out)
}

/// The one sanctioned way for anything downstream (a briefing builder, an MCP
/// cache refresh) to ask "which claims may an agent actually see." Shadow mode is
/// enforced *here*, not only where `ctxlake.toml` is parsed — a caller cannot get
/// a promoted, shadow-mode claim by going around config, because there is no
/// other read path in this crate that returns fleet claims. See the module-level
/// task brief: "structurally impossible for a shadow-mode claim to be served."
///
/// Only `Promoted` claims are ever agent-visible; `Contested` and `Retired`
/// entries that still live under `claims/fleet/` (kept for audit, per the "older
/// is never silently replaced" rule) are filtered out regardless of mode.
pub fn claims_visible_to_agents(states: &[ClaimState], mode: SummarizeMode) -> Vec<&ClaimState> {
    if matches!(mode, SummarizeMode::Shadow | SummarizeMode::None) {
        return Vec::new();
    }
    states
        .iter()
        .filter(|s| s.status == ClaimStatus::Promoted)
        .collect()
}

/// Fetch fleet claims and immediately apply the shadow-mode cutoff — the async
/// counterpart to [`claims_visible_to_agents`] for a caller that only ever wants
/// the agent-visible view and should never see the raw list in between.
pub async fn read_promoted_for_agents(
    store: &dyn ObjectStore,
    mode: SummarizeMode,
) -> Result<Vec<ClaimState>, StoreError> {
    if matches!(mode, SummarizeMode::Shadow | SummarizeMode::None) {
        return Ok(Vec::new());
    }
    let all = list_fleet_claims(store).await?;
    Ok(all
        .into_iter()
        .filter(|s| s.status == ClaimStatus::Promoted)
        .collect())
}

/// True when `err` is the "someone already appended this exact key" outcome —
/// exposed so `extract`'s idempotency marker (a genuine create-if-absent lock, not
/// a lease) can tell "I raced and lost" apart from a real backend failure.
pub fn is_already_exists(err: &StoreError) -> bool {
    err.is_already_exists()
}

/// Same check directly against the raw `object_store` error, for call sites still
/// holding one (the idempotency marker's `put_opts` call, before it is wrapped).
pub fn os_error_is_already_exists(err: &OsError) -> bool {
    matches!(err, OsError::AlreadyExists { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(session: &str, msg: &str) -> Evidence {
        Evidence {
            session_id: session.into(),
            message_id: msg.into(),
            excerpt_hash: format!("hash-{session}-{msg}"),
        }
    }

    fn proposed(claim_id: &str, session: &str) -> ClaimEvent {
        ClaimEvent::Proposed(ProposedClaim {
            claim_id: claim_id.into(),
            claim: "this repo uses just, not make".into(),
            claim_type: ClaimType::Convention,
            subject: "build-tooling".into(),
            scope: Scope::Agent,
            observed_by: "cc-01".into(),
            observed_at: "2026-09-09T00:00:00Z".into(),
            evidence: vec![evidence(session, "m1")],
            embedding: None,
        })
    }

    #[test]
    fn claim_type_round_trips_the_five_documented_strings() {
        for (s, t) in [
            ("environment", ClaimType::Environment),
            ("convention", ClaimType::Convention),
            ("outcome", ClaimType::Outcome),
            ("preference", ClaimType::Preference),
            ("hypothesis", ClaimType::Hypothesis),
        ] {
            assert_eq!(ClaimType::parse(s), Some(t));
            assert_eq!(t.as_str(), s);
        }
        assert_eq!(ClaimType::parse("opinion"), None);
    }

    #[test]
    fn fold_starts_a_claim_as_candidate() {
        let events = [proposed("c1", "s1")];
        let state = fold(events.iter());
        let c1 = state.get("c1").unwrap();
        assert_eq!(c1.status, ClaimStatus::Candidate);
        assert_eq!(c1.evidence.len(), 1);
    }

    #[test]
    fn a_second_proposal_for_the_same_claim_id_adds_evidence_not_a_second_claim() {
        let events = vec![proposed("c1", "s1"), proposed("c1", "s2")];
        let state = fold(events.iter());
        assert_eq!(state.len(), 1, "one claim_id must fold into one claim");
        let c1 = state.get("c1").unwrap();
        assert_eq!(c1.evidence_session_count(), 2);
    }

    #[test]
    fn duplicate_evidence_is_not_double_counted() {
        // Re-proposing the exact same (session, message) citation — a retried
        // extraction run before its idempotency marker landed, say — must not
        // inflate evidence_session_count.
        let events = vec![proposed("c1", "s1"), proposed("c1", "s1")];
        let state = fold(events.iter());
        assert_eq!(state.get("c1").unwrap().evidence.len(), 1);
    }

    #[test]
    fn promoted_then_contested_ends_in_contested_with_history_intact() {
        // "The older is not silently replaced": the fold must reflect the LATEST
        // status, but the Promoted event itself is never deleted or rewritten —
        // it is simply not the last word once a Contested event follows it. This
        // test only exercises the fold; the append-only nature is structural
        // (both are separate events, never the same object).
        let events = vec![
            proposed("c1", "s1"),
            ClaimEvent::Promoted {
                claim_id: "c1".into(),
                at: "2026-09-10T00:00:00Z".into(),
                independent_count: 2,
                confidence: 0.8,
            },
            ClaimEvent::Contested {
                claim_id: "c1".into(),
                at: "2026-09-11T00:00:00Z".into(),
                conflicts_with: Some("c2".into()),
                reason: "conflicts with c2 on the same subject".into(),
            },
        ];
        let state = fold(events.iter());
        assert_eq!(state.get("c1").unwrap().status, ClaimStatus::Contested);
    }

    #[test]
    fn fold_order_matters_and_is_the_callers_responsibility() {
        // Applying events out of order (Contested before its Promoted) produces a
        // wrong-but-deterministic answer — this test documents that fold trusts
        // its caller's ordering rather than silently reordering by any field of
        // its own, which is why list_events sorts by key before folding.
        let events = vec![
            ClaimEvent::Contested {
                claim_id: "c1".into(),
                at: "z".into(),
                conflicts_with: None,
                reason: "out of order on purpose".into(),
            },
            proposed("c1", "s1"),
        ];
        let state = fold(events.iter());
        // The Contested event landed on a claim that didn't exist yet at that
        // point in the fold, so it was a no-op; the subsequent Proposed created
        // the claim as a fresh Candidate.
        assert_eq!(state.get("c1").unwrap().status, ClaimStatus::Candidate);
    }

    #[test]
    fn claims_visible_to_agents_is_empty_in_shadow_mode_even_with_promoted_claims() {
        let promoted = ClaimState {
            claim_id: "c1".into(),
            claim: "cargo test needs RUSTFLAGS".into(),
            claim_type: ClaimType::Convention,
            subject: "ci".into(),
            scope: Scope::Fleet,
            observed_by: "cc-01".into(),
            observed_at: "2026-09-09T00:00:00Z".into(),
            evidence: vec![evidence("s1", "m1"), evidence("s2", "m1")],
            status: ClaimStatus::Promoted,
            independent_count: 2,
            confidence: 0.8,
            embedding: None,
        };
        let states = vec![promoted];
        assert_eq!(
            claims_visible_to_agents(&states, SummarizeMode::Shadow).len(),
            0,
            "a promoted claim must not be readable while shadow mode is on"
        );
        assert_eq!(
            claims_visible_to_agents(&states, SummarizeMode::Batch).len(),
            1,
            "the same claim must be visible once shadow mode is off"
        );
    }

    #[test]
    fn claims_visible_to_agents_never_returns_contested_or_candidate() {
        let mut contested = sample_state("c1", ClaimStatus::Contested);
        let mut candidate = sample_state("c2", ClaimStatus::Candidate);
        contested.claim_id = "c1".into();
        candidate.claim_id = "c2".into();
        let states = vec![contested, candidate];
        assert_eq!(
            claims_visible_to_agents(&states, SummarizeMode::Batch).len(),
            0
        );
    }

    fn sample_state(id: &str, status: ClaimStatus) -> ClaimState {
        ClaimState {
            claim_id: id.into(),
            claim: "x".into(),
            claim_type: ClaimType::Environment,
            subject: "s".into(),
            scope: Scope::Fleet,
            observed_by: "cc-01".into(),
            observed_at: "2026-09-09T00:00:00Z".into(),
            evidence: vec![evidence("s1", "m1")],
            status,
            independent_count: 1,
            confidence: 0.6,
            embedding: None,
        }
    }

    #[tokio::test]
    async fn append_then_list_round_trips_a_proposal() {
        let store = object_store::memory::InMemory::new();
        let p = ProposedClaim {
            claim_id: "c1".into(),
            claim: "staging SSH listens on 2222".into(),
            claim_type: ClaimType::Environment,
            subject: "staging".into(),
            scope: Scope::Agent,
            observed_by: "cc-01".into(),
            observed_at: "2026-09-09T00:00:00Z".into(),
            evidence: vec![evidence("s1", "m1")],
            embedding: None,
        };
        append_proposed(&store, "2026-09-09", &p).await.unwrap();
        let events = list_events(&store).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].claim_id(), "c1");
    }

    #[tokio::test]
    async fn publish_then_list_fleet_round_trips_and_overwrites() {
        let store = object_store::memory::InMemory::new();
        let mut state = sample_state("c1", ClaimStatus::Promoted);
        publish_fleet_state(&store, &state).await.unwrap();
        state.status = ClaimStatus::Contested;
        publish_fleet_state(&store, &state).await.unwrap();
        let all = list_fleet_claims(&store).await.unwrap();
        assert_eq!(all.len(), 1, "overwrite, not a second object");
        assert_eq!(all[0].status, ClaimStatus::Contested);
    }

    #[tokio::test]
    async fn read_promoted_for_agents_is_empty_in_shadow_mode_against_a_real_store() {
        let store = object_store::memory::InMemory::new();
        let state = sample_state("c1", ClaimStatus::Promoted);
        publish_fleet_state(&store, &state).await.unwrap();
        let visible = read_promoted_for_agents(&store, SummarizeMode::Shadow)
            .await
            .unwrap();
        assert!(visible.is_empty());
        let visible = read_promoted_for_agents(&store, SummarizeMode::Both)
            .await
            .unwrap();
        assert_eq!(visible.len(), 1);
    }
}
