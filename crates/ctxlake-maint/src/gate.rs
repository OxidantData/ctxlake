//! The promotion gate — `docs/memory.md`'s four checks, run in order, over
//! candidate claims read from `claims/events/`.
//!
//! **Failing a gate sends a claim to review, not to silent deletion.** Nothing in
//! this module deletes a candidate; a rejected candidate simply never gets a
//! `Promoted` event, so it stays `candidate` forever (visible to
//! `ctxlake claims --status candidate --explain`) until a human or a later run
//! with more evidence changes the outcome.
//!
//! **Nothing serializes [`run`] across hosts, on purpose.** An earlier version of
//! this module required a fleet-wide maintenance lease before running, on the
//! theory that `claims/fleet/`'s plain overwrite (`claims::publish_fleet_state`)
//! was only safe because the gate was single-writer. That lease is gone: two
//! concurrent gate runs must not double-promote, and they don't, because
//! promotion is idempotent by fold semantics rather than by exclusivity —
//! `claims::fold`'s handling of a repeated `Promoted` event for an
//! already-promoted claim is a no-op (first promotion wins; see its own doc and
//! test), so it doesn't matter which of two racing runs' events lands, or folds,
//! first. Do not reintroduce a lock here — if a future check turns out not to be
//! safe under concurrent runs, make *that check's* output idempotent by content,
//! the same way this one is.

use std::collections::HashMap;

use ctxlake_store::StoreError;
use object_store::ObjectStore;

use crate::claims::{self, ClaimEvent, ClaimState, ClaimStatus, ClaimType, Evidence};

/// Brute-force vector search operates over 256-dim embeddings — see the module
/// doc's arithmetic: 5k claims * 256 dims * 4 bytes = 5 MB, one linear SIMD-able
/// pass. No ANN index, no vector database; at this scale exact search is cheaper
/// to run *and* to reason about than anything approximate.
pub const EMBEDDING_DIM: usize = 256;

/// Cosine similarity above this, on the *same subject*, counts as "the same
/// topic" for the contradiction check. This is a coarse proxy for semantic
/// agreement/disagreement — ctxlake has no NLI model to tell "confirms" from
/// "contradicts" apart, so the design choice (see docs/memory.md) is to treat
/// "same subject, same topic, different claim text" as a conflict worth a
/// human's attention rather than silently assuming corroboration. Erring toward
/// too many contested claims is the safe direction; erring the other way is how a
/// fleet gaslights itself.
pub const CONTRADICTION_SIMILARITY_THRESHOLD: f32 = 0.85;
/// Same reasoning, for candidates with no embedding at all — a plain token
/// overlap ratio over the claim text (a minimal FTS proxy, per the task brief's
/// "cosine over embeddings plus FTS").
pub const CONTRADICTION_LEXICAL_OVERLAP_THRESHOLD: f32 = 0.6;

/// Cosine similarity between two equal-length vectors. Zero (not NaN, not a
/// panic) when either vector is all zeros — an embedding that failed to compute
/// should compare as "unrelated," not blow up the whole gate run.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "cosine: embeddings must be equal length");
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Exact top-`k` nearest neighbors of `query` in `corpus`, by cosine similarity,
/// highest first. `corpus` is `(id, embedding)` pairs. This is the whole
/// "vector search" this system has: one linear pass, no index to build or keep
/// warm, no approximation to validate — see the module doc and
/// `docs/memory.md`'s contradiction gate.
pub fn top_k_cosine(query: &[f32], corpus: &[(String, Vec<f32>)], k: usize) -> Vec<(String, f32)> {
    let mut scored: Vec<(String, f32)> = corpus
        .iter()
        .map(|(id, v)| (id.clone(), cosine(query, v)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(k);
    scored
}

/// A crude lexical-overlap ratio: fraction of `a`'s lowercased word set also
/// present in `b`'s. Not real FTS ranking (no term frequency, no stopwords) — a
/// deliberately minimal stand-in the task brief calls for alongside cosine, not a
/// search engine.
///
/// `pub(crate)`, not private: `crate::calibrate::resolve` reuses this exact
/// function (via [`cosine`] first, falling back to this) to decide whether an
/// outcome claim agrees with a hypothesis — the same "same subject, similar
/// text" proxy this gate uses for contradiction, not a second, possibly
/// drifting implementation of "do these two claims agree."
pub(crate) fn lexical_overlap(a: &str, b: &str) -> f32 {
    let words_a: std::collections::HashSet<String> = a
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    let words_b: std::collections::HashSet<String> = b
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    if words_a.is_empty() {
        return 0.0;
    }
    let shared = words_a.intersection(&words_b).count();
    shared as f32 / words_a.len() as f32
}

/// Everything the gate needs that isn't already on the candidate itself —
/// injected as plain data/closures rather than an async trait so the four checks
/// stay synchronous and trivially unit-testable without a runtime or a fake
/// store.
pub struct GateContext<'a> {
    /// Already-promoted claims, for the contradiction check.
    pub promoted: &'a [ClaimState],
    /// Agent ids the fleet actually knows about (roster history, or `"human"`
    /// for imported conventions) — the provenance check's "is this a real
    /// agent" test.
    pub known_agents: &'a std::collections::HashSet<String>,
    /// `session_id -> (window_start, window_end)`, both RFC3339-ish strings
    /// comparable by `<`/`>` (matching the envelope's own `emitted_at` format).
    /// Missing entry = unknown session = provenance failure, not "assume fine."
    pub session_windows: &'a HashMap<String, (String, String)>,
    /// `session_id -> claim_ids injected into that session's context` — the
    /// read-lineage the independence gate joins evidence against. See
    /// `compute_independent_count`.
    pub injected_context_by_session: &'a HashMap<String, std::collections::HashSet<String>>,
    /// Has a human approved this specific candidate? `preference` claims can
    /// never promote without one, regardless of evidence.
    pub human_approved: bool,
    /// Agents currently under quarantine — see `crate::calibrate`'s module
    /// doc and [`GateDecision::Quarantined`]. Checked before any of the four
    /// gates, because it isn't one of them: it is docs/memory.md's separate
    /// kill switch, and a quarantined agent's candidate must never even reach
    /// gate 1's evidence check, let alone pass it.
    pub quarantined_agents: &'a std::collections::HashSet<String>,
    /// Per-agent calibration, feeding [`derive_confidence`] — see
    /// `crate::calibrate::AgentScore`. An agent with no entry here (a `None`
    /// from `.get`) has no resolved track record yet, which
    /// `AgentScore::trust_multiplier`'s doc explains must read as neutral,
    /// never as a penalty for being new.
    pub agent_scores: &'a HashMap<String, crate::calibrate::AgentScore>,
}

/// The outcome of running a candidate through all four gates.
#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    Promote {
        independent_count: u32,
        confidence: f64,
    },
    /// Both this candidate and `conflicts_with` move to `contested` — see the
    /// module doc and `docs/memory.md`: newest never wins.
    Contest { conflicts_with: String },
    /// Failed one gate; stays `candidate`, visible to `--explain`.
    Review {
        failed_gate: &'static str,
        reason: String,
    },
    /// The observing agent is quarantined (`crate::calibrate`) — held back
    /// regardless of what the four gates above would otherwise say, and
    /// deliberately not reported as `Review { failed_gate: ... }`: an
    /// operator seeing "rejected by the evidence gate" would go looking for
    /// more evidence, which fixes nothing here. Produces no event (like
    /// `Review`): the `AgentQuarantined` event already on the log is the
    /// audit trail for "why," this is just the gate obeying it.
    Quarantined { agent_id: String },
}

/// Gate 1 (coarse): can this claim type structurally ever promote right now,
/// judging only by the raw evidence on file — cheap enough to run before the
/// more expensive checks. Every claim type needs at least one evidence citation,
/// full stop — docs/memory.md's "No evidence, no claim" — and `hypothesis` and
/// un-approved `preference` fail unconditionally on top of that. The
/// authoritative *count* for the types that need more than one is enforced
/// later, in gate 4, using `independent_count` rather than this raw count —
/// docs/memory.md: "thresholds read independent_count, never evidence_count".
fn evidence_precheck(candidate: &ClaimState, human_approved: bool) -> Result<(), String> {
    // docs/memory.md's "No evidence, no claim" is stated as a floor beneath every
    // claim type, not a per-type option some rows of the promotion table happen to
    // repeat — "This single rule removes most hallucinated memory." A zero-evidence
    // `preference` used to slip past this function once `human_approved` was set,
    // because the empty-evidence check previously lived only inside the
    // Environment/Outcome/Convention arm below. Checking it here, before the
    // per-type match, makes the gate the authoritative enforcement point
    // docs/memory.md claims it is, rather than trusting extraction and
    // `memory_propose` to have already filtered — belt and braces, since this
    // function's whole job is to be the belt for every claim type at once.
    if candidate.evidence.is_empty() {
        return Err("no evidence at all".to_string());
    }
    match candidate.claim_type {
        ClaimType::Hypothesis => Err(
            "hypothesis claims never auto-promote beyond agent scope, at any evidence count"
                .to_string(),
        ),
        ClaimType::Preference => {
            if human_approved {
                Ok(())
            } else {
                Err("preference claims require human approval".to_string())
            }
        }
        ClaimType::Environment | ClaimType::Outcome | ClaimType::Convention => Ok(()),
    }
}

/// Gate 2: brute-force cosine (falling back to lexical overlap when either side
/// lacks an embedding) against promoted claims on the same subject. Returns the
/// conflicting claim's id on a hit.
fn find_contradiction(candidate: &ClaimState, promoted: &[ClaimState]) -> Option<String> {
    let same_subject: Vec<&ClaimState> = promoted
        .iter()
        .filter(|p| p.subject == candidate.subject && p.status == ClaimStatus::Promoted)
        .collect();
    for p in &same_subject {
        if claims::normalize_claim_text(&p.claim) == claims::normalize_claim_text(&candidate.claim)
        {
            // Same claim, not a conflict — this is corroboration, handled by
            // evidence accumulation on the same claim_id, not the gate.
            continue;
        }
        let sim = match (&candidate.embedding, &p.embedding) {
            (Some(a), Some(b)) if a.len() == b.len() => cosine(a, b),
            _ => 0.0,
        };
        let lex = lexical_overlap(&candidate.claim, &p.claim);
        if sim >= CONTRADICTION_SIMILARITY_THRESHOLD
            || lex >= CONTRADICTION_LEXICAL_OVERLAP_THRESHOLD
        {
            return Some(p.claim_id.clone());
        }
    }
    None
}

/// Gate 3: `observed_by` is a real agent, every evidence citation's *own*
/// `observed_at` falls inside *its own* session's window, and (delegated to the
/// caller via `excerpt_resolves`, since resolving a hash needs the actual
/// transcript) every excerpt hash checks out.
///
/// This checks each [`Evidence`]'s own `observed_at` against its own session's
/// window — never the claim-level `candidate.observed_at` against every cited
/// session's window. A claim's evidence can span sessions from different days (a
/// `convention` first proposed Tuesday, corroborated Friday, is the whole point
/// of the independence gate); `candidate.observed_at` only ever holds the
/// timestamp of whichever `Proposed` event created the claim (see `fold`, which
/// never updates it on later evidence merges), so checking it against a *later*
/// session's window would reject every multi-session claim as a matter of course,
/// which is a livelock the fixture data controlling test dates would happily hide
/// (every session in the old `windows_for` test helper shared the same day).
fn check_provenance(
    candidate: &ClaimState,
    ctx: &GateContext,
    excerpt_resolves: impl Fn(&Evidence) -> bool,
) -> Result<(), String> {
    if candidate.observed_by != "human" && !ctx.known_agents.contains(&candidate.observed_by) {
        return Err(format!(
            "observed_by {:?} is not a known agent",
            candidate.observed_by
        ));
    }
    for e in &candidate.evidence {
        let Some((start, end)) = ctx.session_windows.get(&e.session_id) else {
            return Err(format!("session {} has no known time window", e.session_id));
        };
        if e.observed_at.as_str() < start.as_str() || e.observed_at.as_str() > end.as_str() {
            return Err(format!(
                "evidence observed_at {} falls outside session {}'s window [{start}, {end}]",
                e.observed_at, e.session_id
            ));
        }
        if !excerpt_resolves(e) {
            return Err(format!(
                "evidence citation ({}, {}) does not resolve to a real excerpt",
                e.session_id, e.message_id
            ));
        }
    }
    Ok(())
}

/// Gate 4, and "the one that matters most": recompute `independent_count` from
/// the raw evidence and the injected-context read-lineage, then enforce the
/// *real* per-type threshold against that number — never against raw evidence
/// count. See `docs/memory.md`'s independence section and the echo-case test
/// below.
///
/// A session counts as independent for `claim_id` iff that session's own
/// `injected_context` (AGENTS.md invariant: captured from the first commit, on
/// every envelope) does not include `claim_id`. Two agents "agreeing" because one
/// read the other's claim injected into its context is one observation with two
/// reporters, not two independent ones.
pub fn compute_independent_count(
    claim_id: &str,
    evidence: &[Evidence],
    injected_context_by_session: &HashMap<String, std::collections::HashSet<String>>,
) -> u32 {
    let sessions: std::collections::BTreeSet<&str> =
        evidence.iter().map(|e| e.session_id.as_str()).collect();
    sessions
        .into_iter()
        .filter(|session_id| {
            !injected_context_by_session
                .get(*session_id)
                .is_some_and(|injected| injected.contains(claim_id))
        })
        .count() as u32
}

fn independent_threshold(claim_type: ClaimType) -> u32 {
    match claim_type {
        ClaimType::Environment | ClaimType::Outcome => 1,
        ClaimType::Convention => 2,
        // A hypothesis never reaches this point on its own account —
        // evidence_precheck already rejected every one of them unconditionally.
        // A defensive high bar here means a future refactor that skips gate 1 by
        // mistake still can't promote one by accident.
        ClaimType::Hypothesis => u32::MAX,
        // Preference promotion is gated on human approval (gate 1), not on an
        // independent-session count — docs/memory.md names no threshold for it.
        // Zero means "gate 4 imposes no additional bar" once gate 1 has already
        // required the approval flag.
        ClaimType::Preference => 0,
    }
}

/// Confidence for a newly-promoted claim: a structural component from
/// independent corroboration (unchanged from before calibration existed —
/// still monotonic in `independent_count` and bounded, which is all the rest
/// of this module depends on it for), scaled by how much to trust *this
/// observing agent's* assertions in general — see
/// `crate::calibrate::AgentScore::trust_multiplier`.
///
/// **This is the whole point of docs/memory.md's "Confidence is derived, not
/// claimed."** `calibration` is `None` for an agent with no resolved
/// hypotheses yet, which must multiply by `1.0` (neutral, see
/// `trust_multiplier`'s own doc) — a brand-new agent is not penalized for
/// lacking a track record, but a *proven-unreliable* one (a real, non-empty
/// history of resolving wrong) does get discounted here, and a
/// proven-reliable one gets a boost. Note there is no self-reported
/// confidence anywhere on [`ProposedClaim`](crate::claims::ProposedClaim) for
/// this to "fall back to" in the first place — the schema simply does not
/// carry one, which is itself the design: an agent has no channel to assert
/// "trust me" directly, only a channel to *earn* trust via
/// `crate::calibrate::resolve`.
fn derive_confidence(
    independent_count: u32,
    calibration: Option<&crate::calibrate::AgentScore>,
) -> f64 {
    let structural = (0.5 + 0.15 * independent_count as f64).min(0.95);
    let trust = calibration
        .map(crate::calibrate::AgentScore::trust_multiplier)
        .unwrap_or(1.0);
    (structural * trust).clamp(0.05, 0.95)
}

/// Run all four gates over one candidate. `excerpt_resolves` is a closure rather
/// than baked into `GateContext` so tests can supply a trivial one without
/// constructing a real session index.
pub fn run_gate(
    candidate: &ClaimState,
    ctx: &GateContext,
    excerpt_resolves: impl Fn(&Evidence) -> bool,
) -> GateDecision {
    // The kill switch, checked first because it is not one of the four gates
    // below (docs/memory.md) — a quarantined agent's candidate is held
    // regardless of how well it would otherwise score.
    if ctx.quarantined_agents.contains(&candidate.observed_by) {
        return GateDecision::Quarantined {
            agent_id: candidate.observed_by.clone(),
        };
    }
    if let Err(reason) = evidence_precheck(candidate, ctx.human_approved) {
        return GateDecision::Review {
            failed_gate: "evidence",
            reason,
        };
    }
    if let Some(conflicts_with) = find_contradiction(candidate, ctx.promoted) {
        return GateDecision::Contest { conflicts_with };
    }
    if let Err(reason) = check_provenance(candidate, ctx, excerpt_resolves) {
        return GateDecision::Review {
            failed_gate: "provenance",
            reason,
        };
    }
    let independent_count = compute_independent_count(
        &candidate.claim_id,
        &candidate.evidence,
        ctx.injected_context_by_session,
    );
    let threshold = independent_threshold(candidate.claim_type);
    if independent_count < threshold {
        return GateDecision::Review {
            failed_gate: "independence",
            reason: format!(
                "{} independent session(s), needs {} for {:?}",
                independent_count, threshold, candidate.claim_type
            ),
        };
    }
    GateDecision::Promote {
        independent_count,
        confidence: derive_confidence(
            independent_count,
            ctx.agent_scores.get(&candidate.observed_by),
        ),
    }
}

/// Turn a decision into the event(s) it produces. `Contest` yields two events —
/// one per side of the conflict — because both claims move to `contested`
/// together (docs/memory.md: "both move to contested"). `Review` yields no event
/// at all: the candidate's existing `Proposed` event(s) already say everything
/// that needs saying, and staying silent is what keeps it `candidate` rather than
/// inventing a fifth, undocumented status.
pub fn events_for_decision(
    candidate_id: &str,
    at: &str,
    decision: &GateDecision,
) -> Vec<ClaimEvent> {
    match decision {
        GateDecision::Promote {
            independent_count,
            confidence,
        } => vec![ClaimEvent::Promoted {
            claim_id: candidate_id.to_string(),
            at: at.to_string(),
            independent_count: *independent_count,
            confidence: *confidence,
        }],
        GateDecision::Contest { conflicts_with } => vec![
            ClaimEvent::Contested {
                claim_id: candidate_id.to_string(),
                at: at.to_string(),
                conflicts_with: Some(conflicts_with.clone()),
                reason: format!(
                    "conflicts with promoted claim {conflicts_with} on the same subject"
                ),
            },
            ClaimEvent::Contested {
                claim_id: conflicts_with.clone(),
                at: at.to_string(),
                conflicts_with: Some(candidate_id.to_string()),
                reason: format!(
                    "conflicts with candidate claim {candidate_id} on the same subject"
                ),
            },
        ],
        GateDecision::Review { .. } => Vec::new(),
        // Same reasoning as `Review`: the `AgentQuarantined` event already
        // says why, and this candidate stays `candidate` until either it is
        // un-quarantined and re-evaluated, or a human intervenes — no fifth
        // status to invent here either.
        GateDecision::Quarantined { .. } => Vec::new(),
    }
}

/// Result of one full gate run over every current candidate.
#[derive(Debug, Default)]
pub struct GateRunSummary {
    pub promoted: usize,
    pub contested_pairs: usize,
    pub sent_to_review: usize,
    /// Candidates held by the quarantine kill switch this run — see
    /// `crate::calibrate`. Counted separately from `sent_to_review` so an
    /// operator reading this summary can tell "needs more evidence" apart
    /// from "this agent is quarantined, more evidence won't help."
    pub blocked_by_quarantine: usize,
    /// Already-`Promoted` claims demoted to `contested` this run because
    /// their observing agent is quarantined — the other real effect of the
    /// kill switch docs/memory.md promises ("its already-promoted claims
    /// move to contested"), applied every run (not only the run in which
    /// `AgentQuarantined` was written) so it is a standing invariant, not a
    /// one-time side effect that a differently-ordered maintenance run could
    /// miss.
    pub demoted_by_quarantine: usize,
}

/// Fold every event, run the gate over every still-`candidate` claim, append the
/// resulting events, and republish `claims/fleet/` for anything that changed
/// status.
///
/// Safe to call concurrently, from any number of hosts — see the module doc for
/// why double-promotion can't happen even without a lock.
pub async fn run(
    store: &dyn ObjectStore,
    ctx_at: &str,
    known_agents: &std::collections::HashSet<String>,
    session_windows: &HashMap<String, (String, String)>,
    injected_context_by_session: &HashMap<String, std::collections::HashSet<String>>,
    excerpt_resolves: impl Fn(&Evidence) -> bool,
) -> Result<GateRunSummary, StoreError> {
    let events = claims::list_events(store).await?;
    // Both folds read the exact same already-fetched `events` — no second
    // `list_events` round-trip just to learn who is quarantined or how an
    // agent has calibrated, and no risk of the two views disagreeing about
    // which events exist.
    let quarantined_agents = crate::calibrate::fold_quarantine(events.iter());
    let agent_scores = crate::calibrate::fold_scores(events.iter());
    let folded = claims::fold(events.iter());
    let mut promoted: Vec<ClaimState> = folded
        .values()
        .filter(|s| s.status == ClaimStatus::Promoted)
        .cloned()
        .collect();

    let mut summary = GateRunSummary::default();

    // The kill switch's other real effect: an already-`Promoted` claim whose
    // observing agent is quarantined moves to `contested` — every run, not
    // only the run right after `AgentQuarantined` was written, so this stays
    // a standing invariant of "no promoted claim from a quarantined agent"
    // rather than a one-time reaction. Contested here (never silently
    // retired or deleted): the demotion is the record of the failure, and a
    // human can still find and review it, same as any other contested claim.
    let to_demote: Vec<ClaimState> = promoted
        .iter()
        .filter(|s| quarantined_agents.contains(&s.observed_by))
        .cloned()
        .collect();
    for mut state in to_demote {
        let ulid = ctxlake_core::envelope::next_event_id();
        let ev = ClaimEvent::Contested {
            claim_id: state.claim_id.clone(),
            at: ctx_at.to_string(),
            conflicts_with: None,
            reason: format!(
                "observing agent {} is quarantined — promoted claims from a \
                 quarantined agent move to contested",
                state.observed_by
            ),
        };
        let path = ctxlake_store::layout::claim_event(
            &ctx_at[..10.min(ctx_at.len())],
            &state.observed_by,
            &ulid,
        );
        store
            .put_opts(
                &path,
                object_store::PutPayload::from(serde_json::to_vec(&ev)?),
                object_store::PutMode::Create.into(),
            )
            .await?;
        state.status = ClaimStatus::Contested;
        claims::publish_fleet_state(store, &state).await?;
        summary.demoted_by_quarantine += 1;
    }
    promoted.retain(|s| !quarantined_agents.contains(&s.observed_by));

    // Candidates only — already-decided claims (promoted/contested/retired) have
    // nothing left for this gate to do to them directly, though a *new*
    // candidate can still contest an already-promoted one (handled inside
    // find_contradiction, which reads `promoted` directly).
    let candidates: Vec<ClaimState> = folded
        .values()
        .filter(|s| s.status == ClaimStatus::Candidate)
        .cloned()
        .collect();

    for candidate in &candidates {
        let gctx = GateContext {
            promoted: &promoted,
            known_agents,
            session_windows,
            injected_context_by_session,
            human_approved: false,
            quarantined_agents: &quarantined_agents,
            agent_scores: &agent_scores,
        };
        let decision = run_gate(candidate, &gctx, &excerpt_resolves);
        let new_events = events_for_decision(&candidate.claim_id, ctx_at, &decision);
        for ev in &new_events {
            match ev {
                ClaimEvent::Promoted { .. } => {
                    let ulid = ctxlake_core::envelope::next_event_id();
                    let path = ctxlake_store::layout::claim_event(
                        &ctx_at[..10.min(ctx_at.len())],
                        &candidate.observed_by,
                        &ulid,
                    );
                    let payload = object_store::PutPayload::from(serde_json::to_vec(ev)?);
                    store
                        .put_opts(&path, payload, object_store::PutMode::Create.into())
                        .await?;
                }
                ClaimEvent::Contested { claim_id, .. } => {
                    let ulid = ctxlake_core::envelope::next_event_id();
                    // Use whichever agent proposed *this* side of the conflict
                    // when it's the candidate; the promoted counterpart's own
                    // observed_by when it's the other side.
                    let owner = if claim_id == &candidate.claim_id {
                        candidate.observed_by.clone()
                    } else {
                        promoted
                            .iter()
                            .find(|p| &p.claim_id == claim_id)
                            .map(|p| p.observed_by.clone())
                            .unwrap_or_else(|| "gate".to_string())
                    };
                    let path = ctxlake_store::layout::claim_event(
                        &ctx_at[..10.min(ctx_at.len())],
                        &owner,
                        &ulid,
                    );
                    let payload = object_store::PutPayload::from(serde_json::to_vec(ev)?);
                    store
                        .put_opts(&path, payload, object_store::PutMode::Create.into())
                        .await?;
                }
                _ => {}
            }
        }

        match &decision {
            GateDecision::Promote {
                independent_count,
                confidence,
            } => {
                let mut state = candidate.clone();
                state.status = ClaimStatus::Promoted;
                state.independent_count = *independent_count;
                state.confidence = *confidence;
                claims::publish_fleet_state(store, &state).await?;
                promoted.push(state);
                summary.promoted += 1;
            }
            GateDecision::Contest { conflicts_with } => {
                let mut contested_candidate = candidate.clone();
                contested_candidate.status = ClaimStatus::Contested;
                claims::publish_fleet_state(store, &contested_candidate).await?;
                if let Some(other) = promoted.iter_mut().find(|p| &p.claim_id == conflicts_with) {
                    other.status = ClaimStatus::Contested;
                    let other_clone = other.clone();
                    claims::publish_fleet_state(store, &other_clone).await?;
                }
                summary.contested_pairs += 1;
            }
            GateDecision::Review { .. } => {
                summary.sent_to_review += 1;
            }
            GateDecision::Quarantined { .. } => {
                summary.blocked_by_quarantine += 1;
            }
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::Scope;
    use std::collections::HashSet as StdHashSet;

    fn evidence(session: &str, msg: &str) -> Evidence {
        Evidence {
            session_id: session.into(),
            message_id: msg.into(),
            excerpt_hash: format!("hash-{session}-{msg}"),
            // Inside `windows_for`'s default 2026-09-09 window, so tests that
            // don't care about provenance timing at all don't have to think
            // about it. Tests that DO care (the provenance suite below)
            // override this per evidence item.
            observed_at: "2026-09-09T12:00:00Z".into(),
        }
    }

    fn candidate(
        claim_type: ClaimType,
        subject: &str,
        claim: &str,
        evidence_sessions: &[&str],
    ) -> ClaimState {
        ClaimState {
            claim_id: "cand-1".into(),
            claim: claim.into(),
            claim_type,
            subject: subject.into(),
            scope: Scope::Agent,
            observed_by: "cc-01".into(),
            observed_at: "2026-09-09T12:00:00Z".into(),
            evidence: evidence_sessions
                .iter()
                .map(|s| evidence(s, "m1"))
                .collect(),
            status: ClaimStatus::Candidate,
            independent_count: 0,
            confidence: 0.0,
            embedding: Some(vec![1.0; EMBEDDING_DIM]),
            resolves_at: None,
        }
    }

    /// `'static`, so `&EMPTY_QUARANTINE`/`&EMPTY_AGENT_SCORES` satisfy
    /// `GateContext<'a>` for any `'a` — every pre-calibration test in this
    /// module can keep calling `default_ctx` with its original four
    /// arguments unchanged, rather than every call site needing to also
    /// thread through two more temporaries it does not care about. Tests
    /// that DO care (quarantine, calibration) build their own real
    /// `HashSet`/`HashMap` and construct `GateContext` directly instead of
    /// going through this helper.
    static EMPTY_QUARANTINE: std::sync::LazyLock<StdHashSet<String>> =
        std::sync::LazyLock::new(StdHashSet::new);
    static EMPTY_AGENT_SCORES: std::sync::LazyLock<HashMap<String, crate::calibrate::AgentScore>> =
        std::sync::LazyLock::new(HashMap::new);

    fn default_ctx<'a>(
        promoted: &'a [ClaimState],
        known_agents: &'a StdHashSet<String>,
        windows: &'a HashMap<String, (String, String)>,
        injected: &'a HashMap<String, StdHashSet<String>>,
    ) -> GateContext<'a> {
        GateContext {
            promoted,
            known_agents,
            session_windows: windows,
            injected_context_by_session: injected,
            human_approved: false,
            quarantined_agents: &EMPTY_QUARANTINE,
            agent_scores: &EMPTY_AGENT_SCORES,
        }
    }

    fn always_resolves(_e: &Evidence) -> bool {
        true
    }

    fn windows_for(sessions: &[&str]) -> HashMap<String, (String, String)> {
        sessions
            .iter()
            .map(|s| {
                (
                    s.to_string(),
                    (
                        "2026-09-09T00:00:00Z".to_string(),
                        "2026-09-09T23:59:59Z".to_string(),
                    ),
                )
            })
            .collect()
    }

    // ---- vector search: brute force matches a naive reference ----

    #[test]
    fn top_k_cosine_matches_a_naive_reference_implementation() {
        let mut corpus = Vec::new();
        for i in 0..200u32 {
            let mut v = vec![0.0f32; EMBEDDING_DIM];
            v[0] = (i as f32).sin();
            v[1] = (i as f32).cos();
            v[i as usize % EMBEDDING_DIM] += 0.001 * i as f32;
            corpus.push((format!("id-{i}"), v));
        }
        let mut query = vec![0.0f32; EMBEDDING_DIM];
        query[0] = 0.7;
        query[1] = 0.3;

        let got = top_k_cosine(&query, &corpus, 5);

        // Reference: a completely separate, deliberately naive computation (f64
        // accumulation, manual loop, no shared helper) so a bug in `cosine`
        // itself would not silently agree with its own reference.
        fn naive_cosine(a: &[f32], b: &[f32]) -> f64 {
            let mut dot = 0.0f64;
            let mut na = 0.0f64;
            let mut nb = 0.0f64;
            for i in 0..a.len() {
                dot += a[i] as f64 * b[i] as f64;
                na += a[i] as f64 * a[i] as f64;
                nb += b[i] as f64 * b[i] as f64;
            }
            if na == 0.0 || nb == 0.0 {
                0.0
            } else {
                dot / (na.sqrt() * nb.sqrt())
            }
        }
        let mut reference: Vec<(String, f64)> = corpus
            .iter()
            .map(|(id, v)| (id.clone(), naive_cosine(&query, v)))
            .collect();
        reference.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let reference_top5: Vec<String> = reference.into_iter().take(5).map(|(id, _)| id).collect();
        let got_ids: Vec<String> = got.into_iter().map(|(id, _)| id).collect();
        assert_eq!(got_ids, reference_top5);
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        let mut a = vec![0.0f32; EMBEDDING_DIM];
        let mut b = vec![0.0f32; EMBEDDING_DIM];
        a[0] = 1.0;
        b[1] = 1.0;
        assert!(cosine(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_a_zero_vector_without_nan_or_panic() {
        let a = vec![0.0f32; EMBEDDING_DIM];
        let b = vec![1.0f32; EMBEDDING_DIM];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    // ---- THE ECHO CASE ----

    #[test]
    fn echo_case_two_sessions_agree_but_one_read_the_others_claim_independent_count_is_one() {
        // Session A observed the claim first. Session B's envelope had the SAME
        // claim_id injected into its context before B (independently, it
        // believes) proposed matching evidence — the exact "one observation,
        // two reporters" scenario docs/memory.md exists to catch.
        let mut injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        injected.insert("session-b".into(), StdHashSet::from(["cand-1".to_string()]));

        let evidence = vec![evidence("session-a", "m1"), evidence("session-b", "m1")];
        let count = compute_independent_count("cand-1", &evidence, &injected);
        assert_eq!(
            count, 1,
            "independent_count must discount the session that had this claim injected"
        );

        // And the raw evidence count the design forbids using directly:
        let raw_evidence_sessions: StdHashSet<&str> =
            evidence.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(
            raw_evidence_sessions.len(),
            2,
            "two sessions did support it"
        );
        assert_ne!(
            count as usize,
            raw_evidence_sessions.len(),
            "independent_count must differ from evidence_count in the echo case"
        );
    }

    #[test]
    fn echo_case_end_to_end_through_run_gate_holds_a_convention_claim_at_one_independent() {
        // A convention needs 2 independent observations. Two sessions "agree,"
        // but one had the claim injected — so the gate must NOT promote it, even
        // though naive evidence_count (2) would clear the threshold.
        let mut c = candidate(
            ClaimType::Convention,
            "build-tooling",
            "this repo uses just, not make",
            &["session-a", "session-b"],
        );
        c.embedding = None; // no embedding needed for this test
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&["session-a", "session-b"]);
        let mut injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        injected.insert("session-b".into(), StdHashSet::from([c.claim_id.clone()]));
        let promoted = Vec::new();
        let ctx = default_ctx(&promoted, &known_agents, &windows, &injected);

        let decision = run_gate(&c, &ctx, always_resolves);
        match decision {
            GateDecision::Review { failed_gate, .. } => assert_eq!(failed_gate, "independence"),
            other => {
                panic!("expected the independence gate to hold this claim back, got {other:?}")
            }
        }
    }

    #[test]
    fn convention_promotes_with_two_genuinely_independent_sessions() {
        let mut c = candidate(
            ClaimType::Convention,
            "build-tooling",
            "this repo uses just, not make",
            &["session-a", "session-b"],
        );
        c.embedding = None;
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&["session-a", "session-b"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = Vec::new();
        let ctx = default_ctx(&promoted, &known_agents, &windows, &injected);

        let decision = run_gate(&c, &ctx, always_resolves);
        assert!(matches!(
            decision,
            GateDecision::Promote {
                independent_count: 2,
                ..
            }
        ));
    }

    // ---- calibration: derived confidence, exercised through the REAL
    //      derive_confidence (via run_gate), not a reimplementation of its
    //      formula. This is the test `crate::calibrate`'s own
    //      `derived_confidence_differs_for_a_well_calibrated_vs_poorly_calibrated_agent`
    //      doc points to as the one that would catch a regression in the
    //      actual function — see that test's own doc for why it cannot, by
    //      itself, prove `derive_confidence` isn't silently ignoring
    //      calibration. ----

    #[test]
    fn structurally_identical_candidates_get_different_confidence_from_different_agent_calibration()
    {
        // Two candidates, same claim type, same independent_count (1 evidence
        // session each, `environment`'s threshold) — the ONLY thing that
        // differs between them is which agent observed it and that agent's
        // calibration record. A gate that silently fell back to a fixed
        // formula (or to a self-reported number, if one existed) would
        // produce identical confidence for both; this test fails if it does.
        let good_score = crate::calibrate::AgentScore {
            agent_id: "cc-good".into(),
            resolved_count: 40,
            correct_count: 38,
            incorrect_count: 2,
            expired_count: 0,
            brier_score: 2.0 / 40.0,
            low_sample: false,
        };
        let bad_score = crate::calibrate::AgentScore {
            agent_id: "cc-bad".into(),
            resolved_count: 40,
            correct_count: 4,
            incorrect_count: 36,
            expired_count: 0,
            brier_score: 36.0 / 40.0,
            low_sample: false,
        };
        let mut agent_scores: HashMap<String, crate::calibrate::AgentScore> = HashMap::new();
        agent_scores.insert("cc-good".to_string(), good_score);
        agent_scores.insert("cc-bad".to_string(), bad_score);

        let known_agents = StdHashSet::from([
            "cc-good".to_string(),
            "cc-bad".to_string(),
            "cc-neutral".to_string(),
        ]);
        let windows = windows_for(&["s1"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = Vec::new();

        let run_for =
            |agent: &str, scores: &HashMap<String, crate::calibrate::AgentScore>| -> f64 {
                let mut c = candidate(ClaimType::Environment, "staging", "SSH on 2222", &["s1"]);
                c.observed_by = agent.to_string();
                c.embedding = None;
                let ctx = GateContext {
                    promoted: &promoted,
                    known_agents: &known_agents,
                    session_windows: &windows,
                    injected_context_by_session: &injected,
                    human_approved: false,
                    quarantined_agents: &EMPTY_QUARANTINE,
                    agent_scores: scores,
                };
                match run_gate(&c, &ctx, always_resolves) {
                    GateDecision::Promote { confidence, .. } => confidence,
                    other => panic!("expected a promotion, got {other:?}"),
                }
            };

        let neutral_confidence = run_for("cc-neutral", &agent_scores); // no entry -> None -> neutral
        let good_confidence = run_for("cc-good", &agent_scores);
        let bad_confidence = run_for("cc-bad", &agent_scores);

        assert!(
            good_confidence > neutral_confidence,
            "a well-calibrated agent's REAL derived confidence must exceed the \
             no-track-record baseline: {good_confidence} vs {neutral_confidence}"
        );
        assert!(
            bad_confidence < neutral_confidence,
            "a poorly-calibrated agent's REAL derived confidence must fall below \
             the no-track-record baseline: {bad_confidence} vs {neutral_confidence}"
        );
        assert!(
            good_confidence > bad_confidence,
            "the gate must not silently fall back to a fixed or self-reported \
             confidence regardless of the observing agent's calibration: \
             good={good_confidence} bad={bad_confidence}"
        );
    }

    // ---- hypothesis never promotes ----

    #[test]
    fn hypothesis_never_promotes_beyond_agent_scope_at_any_evidence_count() {
        let sessions: Vec<String> = (0..10).map(|i| format!("session-{i}")).collect();
        let session_refs: Vec<&str> = sessions.iter().map(String::as_str).collect();
        let c = candidate(
            ClaimType::Hypothesis,
            "flake-theory",
            "the flake is a colima scheduling artifact",
            &session_refs,
        );
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&session_refs);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = Vec::new();
        let ctx = default_ctx(&promoted, &known_agents, &windows, &injected);

        let decision = run_gate(&c, &ctx, always_resolves);
        match decision {
            GateDecision::Review { failed_gate, .. } => assert_eq!(failed_gate, "evidence"),
            other => panic!("hypothesis must never promote, got {other:?}"),
        }
    }

    #[test]
    fn preference_requires_human_approval_regardless_of_evidence() {
        let c = candidate(
            ClaimType::Preference,
            "output-style",
            "prefers terse output",
            &["s1", "s2", "s3"],
        );
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&["s1", "s2", "s3"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = Vec::new();
        let mut ctx = default_ctx(&promoted, &known_agents, &windows, &injected);

        let decision = run_gate(&c, &ctx, always_resolves);
        assert!(matches!(
            decision,
            GateDecision::Review {
                failed_gate: "evidence",
                ..
            }
        ));

        ctx.human_approved = true;
        let decision = run_gate(&c, &ctx, always_resolves);
        assert!(matches!(decision, GateDecision::Promote { .. }));
    }

    // ---- contradiction sends both to contested ----

    #[test]
    fn contradiction_sends_both_claims_to_contested_and_never_replaces_the_older() {
        let older_promoted = ClaimState {
            claim_id: "old-1".into(),
            claim: "cargo test needs RUSTFLAGS=-D warnings".into(),
            claim_type: ClaimType::Convention,
            subject: "ci".into(),
            scope: Scope::Fleet,
            observed_by: "cc-02".into(),
            observed_at: "2026-09-01T00:00:00Z".into(),
            evidence: vec![evidence("s0", "m0"), evidence("s01", "m0")],
            status: ClaimStatus::Promoted,
            independent_count: 2,
            confidence: 0.8,
            embedding: Some(vec![1.0; EMBEDDING_DIM]),
            resolves_at: None,
        };
        let mut new_candidate = candidate(
            ClaimType::Convention,
            "ci",
            "cargo test does not need any special RUSTFLAGS",
            &["s1", "s2"],
        );
        new_candidate.embedding = Some(vec![1.0; EMBEDDING_DIM]); // identical embedding: "same topic"

        let known_agents = StdHashSet::from(["cc-01".to_string(), "cc-02".to_string()]);
        let windows = windows_for(&["s1", "s2"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = vec![older_promoted.clone()];
        let ctx = default_ctx(&promoted, &known_agents, &windows, &injected);

        let decision = run_gate(&new_candidate, &ctx, always_resolves);
        let conflicts_with = match &decision {
            GateDecision::Contest { conflicts_with } => conflicts_with.clone(),
            other => panic!("expected a contradiction, got {other:?}"),
        };
        assert_eq!(conflicts_with, "old-1");

        let events =
            events_for_decision(&new_candidate.claim_id, "2026-09-10T00:00:00Z", &decision);
        assert_eq!(events.len(), 2, "both sides get a Contested event");
        let ids: std::collections::HashSet<&str> =
            events.iter().filter_map(|e| e.claim_id()).collect();
        assert!(ids.contains(new_candidate.claim_id.as_str()));
        assert!(ids.contains("old-1"));
        for ev in &events {
            assert!(
                matches!(ev, ClaimEvent::Contested { .. }),
                "the older claim must be contested, not silently retired or overwritten: {ev:?}"
            );
        }
    }

    #[test]
    fn no_contradiction_when_subjects_differ() {
        let older_promoted = ClaimState {
            claim_id: "old-1".into(),
            claim: "cargo test needs RUSTFLAGS".into(),
            claim_type: ClaimType::Convention,
            subject: "ci".into(),
            scope: Scope::Fleet,
            observed_by: "cc-02".into(),
            observed_at: "2026-09-01T00:00:00Z".into(),
            evidence: vec![evidence("s0", "m0"), evidence("s01", "m0")],
            status: ClaimStatus::Promoted,
            independent_count: 2,
            confidence: 0.8,
            embedding: Some(vec![1.0; EMBEDDING_DIM]),
            resolves_at: None,
        };
        let mut new_candidate = candidate(
            ClaimType::Convention,
            "packaging", // different subject
            "cargo test does not need any special RUSTFLAGS",
            &["s1", "s2"],
        );
        new_candidate.embedding = Some(vec![1.0; EMBEDDING_DIM]);
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&["s1", "s2"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = vec![older_promoted];
        let ctx = default_ctx(&promoted, &known_agents, &windows, &injected);

        let decision = run_gate(&new_candidate, &ctx, always_resolves);
        assert!(!matches!(decision, GateDecision::Contest { .. }));
    }

    // ---- provenance ----

    #[test]
    fn provenance_rejects_an_unknown_agent() {
        let mut c = candidate(ClaimType::Environment, "staging", "SSH on 2222", &["s1"]);
        c.observed_by = "not-a-real-agent".into();
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&["s1"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = Vec::new();
        let ctx = default_ctx(&promoted, &known_agents, &windows, &injected);
        let decision = run_gate(&c, &ctx, always_resolves);
        assert!(matches!(
            decision,
            GateDecision::Review {
                failed_gate: "provenance",
                ..
            }
        ));
    }

    #[test]
    fn provenance_rejects_an_observed_at_outside_the_session_window() {
        let mut c = candidate(ClaimType::Environment, "staging", "SSH on 2222", &["s1"]);
        c.observed_by = "cc-01".into();
        // The check is against each evidence item's OWN observed_at, not the
        // claim-level one — see `check_provenance`'s doc.
        c.evidence[0].observed_at = "2099-01-01T00:00:00Z".into();
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&["s1"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = Vec::new();
        let ctx = default_ctx(&promoted, &known_agents, &windows, &injected);
        let decision = run_gate(&c, &ctx, always_resolves);
        assert!(matches!(
            decision,
            GateDecision::Review {
                failed_gate: "provenance",
                ..
            }
        ));
    }

    #[test]
    fn provenance_rejects_a_citation_that_does_not_resolve() {
        let c = candidate(ClaimType::Environment, "staging", "SSH on 2222", &["s1"]);
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&["s1"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = Vec::new();
        let ctx = default_ctx(&promoted, &known_agents, &windows, &injected);
        let decision = run_gate(&c, &ctx, |_e| false);
        assert!(matches!(
            decision,
            GateDecision::Review {
                failed_gate: "provenance",
                ..
            }
        ));
    }

    // ---- end-to-end run() against a real (in-memory) store ----

    #[tokio::test]
    async fn run_promotes_a_qualifying_candidate_and_publishes_fleet_state() {
        let store = object_store::memory::InMemory::new();
        let p1 = crate::claims::ProposedClaim {
            claim_id: "c1".into(),
            claim: "staging SSH listens on 2222".into(),
            claim_type: ClaimType::Environment,
            subject: "staging".into(),
            scope: Scope::Agent,
            observed_by: "cc-01".into(),
            observed_at: "2026-09-09T12:00:00Z".into(),
            evidence: vec![evidence("s1", "m1")],
            embedding: None,
            resolves_at: None,
        };
        claims::append_proposed(&store, "2026-09-09", &p1)
            .await
            .unwrap();

        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&["s1"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();

        let summary = run(
            &store,
            "2026-09-09T12:05:00Z",
            &known_agents,
            &windows,
            &injected,
            always_resolves,
        )
        .await
        .unwrap();
        assert_eq!(summary.promoted, 1);

        let fleet = claims::list_fleet_claims(&store).await.unwrap();
        assert_eq!(fleet.len(), 1);
        assert_eq!(fleet[0].status, ClaimStatus::Promoted);
    }

    #[tokio::test]
    async fn run_leaves_a_failing_candidate_as_candidate_not_deleted() {
        let store = object_store::memory::InMemory::new();
        let p1 = crate::claims::ProposedClaim {
            claim_id: "c1".into(),
            claim: "the flake is a colima scheduling artifact".into(),
            claim_type: ClaimType::Hypothesis,
            subject: "flake-theory".into(),
            scope: Scope::Agent,
            observed_by: "cc-01".into(),
            observed_at: "2026-09-09T12:00:00Z".into(),
            evidence: vec![evidence("s1", "m1")],
            embedding: None,
            resolves_at: None,
        };
        claims::append_proposed(&store, "2026-09-09", &p1)
            .await
            .unwrap();

        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = windows_for(&["s1"]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();

        let summary = run(
            &store,
            "2026-09-09T12:05:00Z",
            &known_agents,
            &windows,
            &injected,
            always_resolves,
        )
        .await
        .unwrap();
        assert_eq!(summary.sent_to_review, 1);
        assert_eq!(summary.promoted, 0);

        // Still on file, still a candidate: not silently dropped.
        let events = claims::list_events(&store).await.unwrap();
        let folded = claims::fold(events.iter());
        assert_eq!(folded.get("c1").unwrap().status, ClaimStatus::Candidate);
        assert!(claims::list_fleet_claims(&store).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_runs_promoting_the_same_candidate_do_not_double_promote() {
        // The test that replaces the maintenance lease at the `gate::run` level:
        // two hosts racing to promote the exact same qualifying candidate — real
        // OS threads, not just two futures on one executor (`InMemory`'s
        // operations never actually suspend, so a single-threaded `join!` would
        // just run them back to back and prove nothing about contention), and
        // repeated several times because a race is not guaranteed to manifest on
        // any one attempt. Whichever way it interleaves, the converged state must
        // show exactly one fleet-scope claim, Promoted, agreeing with a fresh
        // fold of the raw event log — never two, never neither.
        for _ in 0..20 {
            let store = std::sync::Arc::new(object_store::memory::InMemory::new());
            let p1 = crate::claims::ProposedClaim {
                claim_id: "c1".into(),
                claim: "staging SSH listens on 2222".into(),
                claim_type: ClaimType::Environment,
                subject: "staging".into(),
                scope: Scope::Agent,
                observed_by: "cc-01".into(),
                observed_at: "2026-09-09T12:00:00Z".into(),
                evidence: vec![evidence("s1", "m1")],
                embedding: None,
                resolves_at: None,
            };
            claims::append_proposed(store.as_ref(), "2026-09-09", &p1)
                .await
                .unwrap();

            let spawn_run = |store: std::sync::Arc<object_store::memory::InMemory>| {
                tokio::spawn(async move {
                    let known_agents = StdHashSet::from(["cc-01".to_string()]);
                    let windows = windows_for(&["s1"]);
                    let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
                    run(
                        store.as_ref(),
                        "2026-09-09T12:05:00Z",
                        &known_agents,
                        &windows,
                        &injected,
                        always_resolves,
                    )
                    .await
                })
            };

            let (a, b) = tokio::join!(spawn_run(store.clone()), spawn_run(store.clone()));
            a.unwrap().unwrap();
            b.unwrap().unwrap();

            let fleet = claims::list_fleet_claims(store.as_ref()).await.unwrap();
            assert_eq!(
                fleet.len(),
                1,
                "exactly one fleet-scope claim must exist after two racing promotions"
            );
            assert_eq!(fleet[0].status, ClaimStatus::Promoted);

            let events = claims::list_events(store.as_ref()).await.unwrap();
            let folded = claims::fold(events.iter());
            let c1 = folded.get("c1").unwrap();
            assert_eq!(
                c1.status,
                ClaimStatus::Promoted,
                "folding the raw event log independently must agree with claims/fleet/"
            );
        }
    }

    // ---- "no evidence, no claim" applies even to a human-approved preference ----

    #[test]
    fn preference_with_zero_evidence_never_promotes_even_with_human_approval() {
        // docs/memory.md's "No evidence, no claim" is stated as an absolute floor,
        // not a rule that only applies to types whose promotion table entry
        // happens to name a raw count. A preference with nothing cited must fail
        // the evidence gate regardless of `human_approved`.
        let c = candidate(
            ClaimType::Preference,
            "output-style",
            "prefers terse output",
            &[],
        );
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let windows = HashMap::new();
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = Vec::new();
        let mut ctx = default_ctx(&promoted, &known_agents, &windows, &injected);
        ctx.human_approved = true;

        let decision = run_gate(&c, &ctx, always_resolves);
        assert!(
            matches!(
                decision,
                GateDecision::Review {
                    failed_gate: "evidence",
                    ..
                }
            ),
            "a zero-evidence preference must not promote even when human-approved, got {decision:?}"
        );
    }

    // ---- provenance must check each citation against its OWN session, not the
    //      claim's single observed_at against every session ----

    #[test]
    fn provenance_accepts_evidence_spanning_two_different_session_windows() {
        // The exact scenario a `convention` needs to demonstrate independence:
        // first observed in session-a on 2026-09-09, corroborated three days
        // later in session-b. Each evidence item's OWN observed_at sits inside
        // its OWN session's window even though the two windows don't overlap —
        // checking the claim-level observed_at (fixed at first-proposal time)
        // against session-b's later window is exactly the bug this test guards.
        let mut c = candidate(
            ClaimType::Convention,
            "build-tooling",
            "this repo uses just, not make",
            &["session-a", "session-b"],
        );
        c.embedding = None;
        c.evidence[0].observed_at = "2026-09-09T12:00:00Z".into();
        c.evidence[1].observed_at = "2026-09-12T09:00:00Z".into();

        let mut windows = HashMap::new();
        windows.insert(
            "session-a".to_string(),
            (
                "2026-09-09T00:00:00Z".to_string(),
                "2026-09-09T23:59:59Z".to_string(),
            ),
        );
        windows.insert(
            "session-b".to_string(),
            (
                "2026-09-12T00:00:00Z".to_string(),
                "2026-09-12T23:59:59Z".to_string(),
            ),
        );
        let known_agents = StdHashSet::from(["cc-01".to_string()]);
        let injected: HashMap<String, StdHashSet<String>> = HashMap::new();
        let promoted = Vec::new();
        let ctx = default_ctx(&promoted, &known_agents, &windows, &injected);

        let decision = run_gate(&c, &ctx, always_resolves);
        assert!(
            matches!(
                decision,
                GateDecision::Promote {
                    independent_count: 2,
                    ..
                }
            ),
            "evidence spanning two genuinely different session windows must not be \
             rejected by provenance, got {decision:?}"
        );
    }
}
