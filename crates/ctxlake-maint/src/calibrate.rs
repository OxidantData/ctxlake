//! The calibration loop — `docs/memory.md`'s "Confidence is derived, not
//! claimed," made concrete.
//!
//! A model asserting "confidence 0.9" is asserting nothing checkable: there is
//! no experiment whose outcome would prove it wrong. So this module refuses to
//! let a claim carry a self-reported number at all (there is no such field on
//! [`crate::claims::ProposedClaim`] to begin with) and instead builds
//! confidence out of something that *can* be checked — an agent's own history
//! of being right or wrong:
//!
//! ```text
//! agent emits a `hypothesis` claim, with a resolution date
//!            │
//!            ▼
//! reality arrives as an `outcome` claim (from CI, a benchmark, a deploy)
//!            │
//!            ▼
//! resolve(): join them on subject, decide correct / incorrect / expired
//!            │
//!            ▼
//! score_agent(): a Brier-style score over an agent's resolved predictions,
//!                appended to the claim event log as its own event kind
//!            │
//!            ▼
//! gate::derive_confidence(): future claims from this agent are discounted
//!                            or trusted accordingly
//! ```
//!
//! This only pays off because `crate::claims` already keeps provenance
//! (who observed what, and when) and independence (`injected_context`
//! lineage) from the moment a claim is proposed — see `docs/memory.md`. A
//! system that only decided to start tracking whose predictions came true
//! *after* wanting to score them would have nothing to score.
//!
//! ## Why a hypothesis's resolution lives in the event log, not a side table
//!
//! Everything here is built the same way the rest of `crate::claims` is:
//! append-only events, folded into current state on demand, never a row
//! mutated in place. A hypothesis that resolves gets a `Retired` event (an
//! existing event kind — see `crate::claims::ClaimEvent`) recording *that* it
//! resolved and against what; an agent's score is a new event kind,
//! [`crate::claims::ClaimEvent::CalibrationScored`], carrying the *cumulative*
//! totals as of that moment, the same way a `Promoted` event carries a
//! claim's final `independent_count` rather than a delta. Folding the full
//! history back to any point in time recovers exactly the score as it stood
//! then — "auditable and replayable like everything else," which is the
//! literal requirement this module exists to satisfy, not an incidental
//! property.
//!
//! ## Why the score formula shrinks toward neutral
//!
//! "An agent with three resolved predictions has a score that means almost
//! nothing" is not a caveat printed alongside the number — it is load-bearing
//! in the number itself. [`AgentScore::trust_multiplier`] uses a Bayesian
//! shrinkage estimator (a fixed pseudo-count pulling every score toward a
//! neutral prior) specifically so a tiny sample cannot swing a claim's
//! confidence very far in either direction, on top of the honest
//! [`AgentScore::low_sample`] flag every caller can check before presenting a
//! score as if it meant something. Both exist because either one alone is a
//! trap: the flag alone is easy to ignore in a UI that just prints the
//! number; the shrinkage alone still lets a caller call three-for-three
//! "1.00" without qualification.
//!
//! ## The quarantine kill switch
//!
//! The same idea taken to its conclusion: an agent whose track record turns
//! bad enough (in practice, a rising *contradiction* rate — see
//! [`contradiction_rates`], the early-warning metric this module exposes for
//! that) can be quarantined by name. [`quarantine`]/[`unquarantine`] append
//! [`crate::claims::ClaimEvent::AgentQuarantined`] /
//! [`crate::claims::ClaimEvent::AgentUnquarantined`] events; [`fold_quarantine`]
//! replays them into "who is quarantined right now"; `crate::gate::run_gate`
//! consults that set before any of the four gates (see that module's doc) so
//! a quarantined agent's candidates stop promoting, and `crate::gate::run`
//! demotes that agent's already-`Promoted` claims to `contested` every run —
//! **not** a one-time reaction to the `AgentQuarantined` event, a standing
//! invariant re-checked every time. Nothing here ever stops a quarantined
//! agent's `Proposed` events from landing: quarantine only ever adds new
//! events to this log, it never refuses one — see `docs/memory.md`: "you want
//! the record of the failure, not a gap where it used to be."
//!
//! Un-quarantining is real (a fresh `AgentUnquarantined` event, folded the
//! same way) but is deliberately **not** a rollback: a claim already demoted
//! to `contested` because of a quarantine stays `contested` until a human
//! reviews it, exactly like any other contradiction (docs/memory.md: "both
//! move to contested... a human resolves it"). Lifting quarantine restores
//! the ability for *new* candidates from that agent to promote again; it does
//! not silently undo the record that something went wrong.
//!
//! ## A known gap: nothing populates `resolves_at` yet
//!
//! [`resolve`] reads `ClaimState::resolves_at` to decide "expired" apart
//! from "still pending," but `extract.rs`'s `RawClaim` — what the model's
//! structured output is parsed into — has no such field today, so every
//! hypothesis Tier 2 extraction produces right now carries `resolves_at:
//! None`. This degrades gracefully rather than silently (see [`resolve`]'s
//! own doc: such a hypothesis simply never expires on the calendar, only
//! early via a matching outcome), but it does mean the "an unanswered
//! hypothesis eventually leaves the open pool" property is not yet real
//! end-to-end — wiring the extraction prompt/schema to ask for a resolution
//! date is the natural next step, see `docs/memory.md`.

use std::collections::{HashMap, HashSet};

use ctxlake_store::lease::LeaseHandle;
use ctxlake_store::StoreError;
use object_store::{ObjectStore, PutMode, PutPayload};
use serde::{Deserialize, Serialize};

use crate::claims::{self, ClaimEvent, ClaimState, ClaimStatus, ClaimType};
use crate::gate::{
    self, CONTRADICTION_LEXICAL_OVERLAP_THRESHOLD, CONTRADICTION_SIMILARITY_THRESHOLD,
};

/// Below this many resolved (correct + incorrect — expired never counts,
/// see the module doc) predictions, a score is closer to a coin flip's worth
/// of evidence than a reputation. Every place a score crosses out of this
/// module says so via [`AgentScore::low_sample`] rather than presenting the
/// raw number as authoritative.
pub const LOW_SAMPLE_THRESHOLD: u32 = 10;

/// A pseudo-count of "average" resolutions blended into every agent's score
/// before it is allowed to move `derive_confidence` — see
/// [`AgentScore::trust_multiplier`]. Bigger means a bigger sample is needed
/// before an agent's real track record outweighs the neutral prior; chosen
/// to sit comfortably above [`LOW_SAMPLE_THRESHOLD`] so the *shrinkage*, not
/// only the *flag*, is doing real work in the low-sample regime the flag
/// warns about.
const SHRINKAGE_PRIOR_WEIGHT: f64 = 12.0;

// ---------------------------------------------------------------------------
// Resolution: joining a hypothesis to the outcome that answers it
// ---------------------------------------------------------------------------

/// One hypothesis, checked against reality or the calendar. Never both
/// "unresolved" and "wrong" at once — see the module doc and
/// `docs/memory.md`: conflating "nobody ever answered this" with "this was
/// wrong" would punish an agent for a question nobody answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// A matching outcome landed and its claim text agrees with what the
    /// hypothesis predicted.
    Correct {
        hypothesis_id: String,
        outcome_id: String,
        agent_id: String,
    },
    /// A matching outcome landed and its claim text disagrees — reality
    /// arrived and contradicted the prediction.
    Incorrect {
        hypothesis_id: String,
        outcome_id: String,
        agent_id: String,
    },
    /// `now` is at or past the hypothesis's `resolves_at` and no outcome
    /// claim ever landed on its subject. This is deliberately its own
    /// variant, not folded into `Incorrect`: see [`score_agent`]'s doc for
    /// why it must never move a Brier score.
    Expired {
        hypothesis_id: String,
        agent_id: String,
    },
}

impl Resolution {
    pub fn hypothesis_id(&self) -> &str {
        match self {
            Resolution::Correct { hypothesis_id, .. }
            | Resolution::Incorrect { hypothesis_id, .. }
            | Resolution::Expired { hypothesis_id, .. } => hypothesis_id,
        }
    }

    pub fn agent_id(&self) -> &str {
        match self {
            Resolution::Correct { agent_id, .. }
            | Resolution::Incorrect { agent_id, .. }
            | Resolution::Expired { agent_id, .. } => agent_id,
        }
    }
}

/// Does `outcome`'s claim text agree with `hypothesis`'s? Reuses exactly the
/// same "same subject, similar text" proxy `crate::gate`'s contradiction
/// check uses for promoted claims (cosine over embeddings when both have
/// one, lexical overlap otherwise) — not a second, possibly-drifting
/// definition of "these two claims agree." There is no NLI model here any
/// more than there is one in the contradiction gate; see that module's own
/// doc for why this coarse proxy is the deliberate, documented trade rather
/// than an oversight.
fn outcome_agrees(hypothesis: &ClaimState, outcome: &ClaimState) -> bool {
    let sim = match (&hypothesis.embedding, &outcome.embedding) {
        (Some(a), Some(b)) if a.len() == b.len() => gate::cosine(a, b),
        _ => 0.0,
    };
    if sim >= CONTRADICTION_SIMILARITY_THRESHOLD {
        return true;
    }
    gate::lexical_overlap(&hypothesis.claim, &outcome.claim)
        >= CONTRADICTION_LEXICAL_OVERLAP_THRESHOLD
}

/// Join every still-open `hypothesis` in `hypotheses` against the `outcome`
/// claims in `outcomes`, on `subject`, and decide each one.
///
/// Pairing rule: among outcomes on the same subject observed no earlier than
/// the hypothesis itself, the *earliest* one is the answer — the first
/// piece of reality that arrived after the prediction was made is what the
/// prediction is judged against, not a later one that happens to agree more.
/// A hypothesis with no matching outcome resolves to [`Resolution::Expired`]
/// only once `now` is at or past its own `resolves_at`; before that, or if it
/// has no `resolves_at` at all, it is simply not yet resolved and this
/// function reports nothing for it — "still pending" is not a `Resolution`
/// variant on purpose, there is nothing yet to score or audit.
///
/// Callers are expected to have already filtered `hypotheses` to claims
/// still worth resolving (`ClaimStatus::Candidate` — see
/// [`run_calibration`]); a hypothesis already `Retired` by a previous
/// calibration run is not re-resolved simply because this function does not
/// itself check status, it just does not expect to be handed one.
pub fn resolve(hypotheses: &[ClaimState], outcomes: &[ClaimState], now: &str) -> Vec<Resolution> {
    let mut out = Vec::new();
    for h in hypotheses {
        if h.claim_type != ClaimType::Hypothesis {
            continue;
        }
        let mut candidates: Vec<&ClaimState> = outcomes
            .iter()
            .filter(|o| {
                o.claim_type == ClaimType::Outcome
                    && o.subject == h.subject
                    && o.observed_at.as_str() >= h.observed_at.as_str()
            })
            .collect();
        candidates.sort_by(|a, b| a.observed_at.cmp(&b.observed_at));

        if let Some(o) = candidates.first() {
            if outcome_agrees(h, o) {
                out.push(Resolution::Correct {
                    hypothesis_id: h.claim_id.clone(),
                    outcome_id: o.claim_id.clone(),
                    agent_id: h.observed_by.clone(),
                });
            } else {
                out.push(Resolution::Incorrect {
                    hypothesis_id: h.claim_id.clone(),
                    outcome_id: o.claim_id.clone(),
                    agent_id: h.observed_by.clone(),
                });
            }
            continue;
        }

        if let Some(resolves_at) = &h.resolves_at {
            if now >= resolves_at.as_str() {
                out.push(Resolution::Expired {
                    hypothesis_id: h.claim_id.clone(),
                    agent_id: h.observed_by.clone(),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Score: an agent's track record, as a number the gate can use
// ---------------------------------------------------------------------------

/// One agent's calibration record — see the module doc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentScore {
    pub agent_id: String,
    /// `correct_count + incorrect_count` — deliberately **excludes**
    /// `expired_count`. See [`score_agent`]'s doc for why an expired
    /// hypothesis must never appear in the denominator any score is computed
    /// over.
    pub resolved_count: u32,
    pub correct_count: u32,
    pub incorrect_count: u32,
    /// Tracked for visibility, never for scoring: an agent whose hypotheses
    /// keep expiring unresolved (asking questions nobody ever answers) is a
    /// different, milder signal than one whose hypotheses keep resolving
    /// wrong, and folding the two into one number would erase that
    /// difference. See `contradiction_rates` for the sibling early-warning
    /// metric this is meant to be read alongside.
    pub expired_count: u32,
    /// Mean squared error over `{0, 1}` outcomes (`Correct` -> 0, `Incorrect`
    /// -> 1) across every resolved prediction. This is "Brier-style," not
    /// the textbook Brier score over a stated probability, because nothing
    /// on `crate::claims::ProposedClaim` carries a self-reported probability
    /// to score against — see the module doc on why that field does not
    /// exist. `0.0` is a flawless record, `1.0` is the worst possible,
    /// `0.5` (a coin flip's worth of error) is what an agent with zero
    /// resolved predictions gets, on purpose: absence of a track record must
    /// read as "unknown," never as either "perfect" or "worst."
    pub brier_score: f64,
    /// `true` below [`LOW_SAMPLE_THRESHOLD`] resolved predictions.
    pub low_sample: bool,
}

impl AgentScore {
    /// The empty, "never resolved anything yet" score for `agent_id` —
    /// [`derive_confidence`](crate::gate) treats the *absence* of any score
    /// (a `None` from a lookup) as neutral already, but a caller building a
    /// score explicitly (accumulating across runs, say) needs a concrete
    /// zero value to start folding onto.
    pub fn zero(agent_id: &str) -> Self {
        AgentScore {
            agent_id: agent_id.to_string(),
            resolved_count: 0,
            correct_count: 0,
            incorrect_count: 0,
            expired_count: 0,
            brier_score: 0.5,
            low_sample: true,
        }
    }

    /// `1.0 - brier_score`: the "higher is better" reading of the same
    /// number, for anything that wants to display or compare scores rather
    /// than feed them straight into a squared-error formula.
    pub fn reliability(&self) -> f64 {
        1.0 - self.brier_score
    }

    /// How much `crate::gate::derive_confidence` should scale a structural
    /// confidence by, given this track record. `1.0` is neutral (multiplies
    /// by nothing); above `1.0` is a boost earned by being reliably right;
    /// below `1.0` is a discount earned by being reliably wrong.
    ///
    /// **Shrinkage, not a raw lookup**, is what makes this safe to call on a
    /// sample of any size: `reliability` is blended with a neutral prior
    /// (`0.5`, i.e. "no better than a coin flip") weighted by
    /// [`SHRINKAGE_PRIOR_WEIGHT`] pseudo-observations, so a handful of
    /// resolved predictions can only nudge the multiplier a little no matter
    /// how lopsided they are — three-for-three does not read anywhere close
    /// to the same as three-hundred-for-three-hundred. With
    /// `resolved_count == 0` this returns exactly `1.0`: no track record at
    /// all must be neutral, never a penalty for being new (that would punish
    /// every agent's first claim, which is precisely the opposite of what
    /// docs/memory.md's calibration section is for).
    pub fn trust_multiplier(&self) -> f64 {
        if self.resolved_count == 0 {
            return 1.0;
        }
        let reliability = self.reliability();
        let n = self.resolved_count as f64;
        let shrunk =
            (SHRINKAGE_PRIOR_WEIGHT * 0.5 + n * reliability) / (SHRINKAGE_PRIOR_WEIGHT + n);
        // Map shrunk reliability [0, 1] onto a multiplier centered on 1.0:
        // 0.5 (neutral) -> 1.0, 1.0 (flawless) -> 1.3, 0.0 (worst) -> 0.7.
        // Linear and bounded, matching `crate::gate::derive_confidence`'s own
        // "monotonic and bounded is all this needs to be" contract.
        0.7 + 0.6 * shrunk
    }
}

/// Score `agent_id` from `previous` (its last known cumulative score, if
/// any — `None` the first time an agent is ever scored) plus a batch of
/// newly-decided `resolutions`. Resolutions for other agents mixed into the
/// same slice are ignored, so a caller does not have to pre-partition by
/// agent before calling this.
///
/// This is the one function in this module where the three [`Resolution`]
/// variants' distinct effects are enforced: `Correct` and `Incorrect` both
/// move `resolved_count` and `brier_score`, in opposite directions;
/// `Expired` moves neither, only `expired_count` — see the module doc and
/// `docs/memory.md` on why conflating "unresolved" with "wrong" would be
/// its own bug.
pub fn score_agent(
    agent_id: &str,
    previous: Option<&AgentScore>,
    resolutions: &[Resolution],
) -> AgentScore {
    let mut correct = previous.map(|p| p.correct_count).unwrap_or(0);
    let mut incorrect = previous.map(|p| p.incorrect_count).unwrap_or(0);
    let mut expired = previous.map(|p| p.expired_count).unwrap_or(0);

    for r in resolutions {
        if r.agent_id() != agent_id {
            continue;
        }
        match r {
            Resolution::Correct { .. } => correct += 1,
            Resolution::Incorrect { .. } => incorrect += 1,
            Resolution::Expired { .. } => expired += 1,
        }
    }

    let resolved = correct + incorrect;
    let brier_score = if resolved == 0 {
        0.5
    } else {
        incorrect as f64 / resolved as f64
    };

    AgentScore {
        agent_id: agent_id.to_string(),
        resolved_count: resolved,
        correct_count: correct,
        incorrect_count: incorrect,
        expired_count: expired,
        brier_score,
        low_sample: resolved < LOW_SAMPLE_THRESHOLD,
    }
}

/// Fold every [`ClaimEvent::CalibrationScored`] event, keeping the latest per
/// agent — each event already carries *cumulative* totals (see the module
/// doc), so "latest" is "current," the identical rule `crate::claims::fold`
/// applies to a claim's own status. Callers pass events in append order
/// (from `crate::claims::list_events`, already sorted by key) for the same
/// reason that function requires it: an out-of-order fold could let an
/// older score "win" over a newer one.
pub fn fold_scores<'a>(
    events: impl IntoIterator<Item = &'a ClaimEvent>,
) -> HashMap<String, AgentScore> {
    let mut out = HashMap::new();
    for ev in events {
        if let ClaimEvent::CalibrationScored {
            agent_id,
            resolved_count,
            correct_count,
            incorrect_count,
            expired_count,
            brier_score,
            low_sample,
            ..
        } = ev
        {
            out.insert(
                agent_id.clone(),
                AgentScore {
                    agent_id: agent_id.clone(),
                    resolved_count: *resolved_count,
                    correct_count: *correct_count,
                    incorrect_count: *incorrect_count,
                    expired_count: *expired_count,
                    brier_score: *brier_score,
                    low_sample: *low_sample,
                },
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Quarantine: the kill switch
// ---------------------------------------------------------------------------

/// Replay every [`ClaimEvent::AgentQuarantined`] / [`ClaimEvent::AgentUnquarantined`]
/// event, in append order, into "who is quarantined right now." The same
/// append-order requirement as [`fold_scores`] and `crate::claims::fold`
/// applies: this is a fold over a log, not a lookup of the latest single
/// event, so an out-of-order slice produces a wrong-but-deterministic answer
/// rather than a panic — see `crate::claims::fold`'s own doc for why that
/// tradeoff (trusting the caller's ordering) is deliberate here too.
pub fn fold_quarantine<'a>(events: impl IntoIterator<Item = &'a ClaimEvent>) -> HashSet<String> {
    let mut out = HashSet::new();
    for ev in events {
        match ev {
            ClaimEvent::AgentQuarantined { agent_id, .. } => {
                out.insert(agent_id.clone());
            }
            ClaimEvent::AgentUnquarantined { agent_id, .. } => {
                out.remove(agent_id);
            }
            _ => {}
        }
    }
    out
}

/// Append one raw event under `claims/events/`, exactly the way
/// `crate::claims::append_proposed` and `crate::gate::run` both already do
/// inline for their own event kinds — the layout (`claims/events/dt=.../
/// agent=.../<ulid>.json`) is agent-partitioned regardless of what *kind* of
/// event is being appended, so an agent-scoped event (quarantine,
/// calibration) fits the same key shape a claim-scoped one does. `PutMode::
/// Create` is correct for the same reason it is in `append_proposed`: this
/// exact key (a fresh ULID) never existed before and never will again.
async fn append_event(
    store: &dyn ObjectStore,
    at: &str,
    agent_id: &str,
    event: &ClaimEvent,
) -> Result<(), StoreError> {
    let date = &at[..10.min(at.len())];
    let ulid = ctxlake_core::envelope::next_event_id();
    let path = ctxlake_store::layout::claim_event(date, agent_id, &ulid);
    let payload = PutPayload::from(serde_json::to_vec(event)?);
    store
        .put_opts(&path, payload, PutMode::Create.into())
        .await?;
    Ok(())
}

/// The kill switch's immediate, auditable half: append an
/// [`ClaimEvent::AgentQuarantined`] event for `agent_id`. This alone is
/// enough for `crate::gate::run_gate` to start refusing that agent's future
/// candidates (it consults [`fold_quarantine`] on the same event log) and
/// for `crate::gate::run` to start demoting that agent's already-`Promoted`
/// claims to `contested` on its very next run — see that function's doc for
/// why the demotion itself lives there (a standing check re-applied every
/// run) rather than being done once, here, at the moment of quarantine.
///
/// Requires `lease_maintenance` for the same reason every other writer of
/// this event log outside a single agent's own proposals does (AGENTS.md
/// invariant 3: no key written by two processes) — quarantine is an
/// operator action, not something an agent does to itself, and it is
/// serialized against the gate and the calibration run through the same
/// lease so none of the three can interleave a write.
pub async fn quarantine(
    store: &dyn ObjectStore,
    lease: &LeaseHandle,
    agent_id: &str,
    at: &str,
    reason: &str,
) -> Result<(), StoreError> {
    gate::require_maintenance_lease(lease, "calibrate::quarantine")?;
    append_event(
        store,
        at,
        agent_id,
        &ClaimEvent::AgentQuarantined {
            agent_id: agent_id.to_string(),
            at: at.to_string(),
            reason: reason.to_string(),
        },
    )
    .await
}

/// Reverses [`quarantine`] — see the module doc for exactly what this does
/// and does not restore.
pub async fn unquarantine(
    store: &dyn ObjectStore,
    lease: &LeaseHandle,
    agent_id: &str,
    at: &str,
) -> Result<(), StoreError> {
    gate::require_maintenance_lease(lease, "calibrate::unquarantine")?;
    append_event(
        store,
        at,
        agent_id,
        &ClaimEvent::AgentUnquarantined {
            agent_id: agent_id.to_string(),
            at: at.to_string(),
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// The early warning: per-agent contradiction rate
// ---------------------------------------------------------------------------

/// One agent's promoted-vs-contested record, for [`contradiction_rates`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AgentContradictionStats {
    pub promoted_count: u32,
    pub contested_count: u32,
}

impl AgentContradictionStats {
    /// `contested / (promoted + contested)`, `0.0` when this agent has no
    /// decided claims at all yet — an operator's early warning that this
    /// agent's extraction has drifted, per docs/memory.md: "[t]he signal to
    /// reach for [quarantine] is a rising contradiction rate from one
    /// agent." A rate rising over successive maintenance runs is the signal;
    /// this function reports one snapshot, a caller graphs it over time.
    pub fn rate(&self) -> f64 {
        let decided = self.promoted_count + self.contested_count;
        if decided == 0 {
            0.0
        } else {
            self.contested_count as f64 / decided as f64
        }
    }
}

/// Per-agent contradiction rate over currently-decided claims, attributed to
/// each claim's *observing* agent (`ClaimState::observed_by` — the agent
/// whose `Proposed` event first created it, unchanged by later corroborating
/// evidence; see `crate::claims::fold`). Takes the already-folded state
/// (`crate::claims::fold`'s output) rather than a raw event slice, so a
/// caller who already folded once for another purpose (the gate run this
/// almost always accompanies) does not pay for a second fold just to compute
/// this metric.
pub fn contradiction_rates(
    states: &std::collections::BTreeMap<String, ClaimState>,
) -> HashMap<String, AgentContradictionStats> {
    let mut out: HashMap<String, AgentContradictionStats> = HashMap::new();
    for s in states.values() {
        match s.status {
            ClaimStatus::Promoted => {
                out.entry(s.observed_by.clone()).or_default().promoted_count += 1
            }
            ClaimStatus::Contested => {
                out.entry(s.observed_by.clone())
                    .or_default()
                    .contested_count += 1
            }
            ClaimStatus::Candidate | ClaimStatus::Retired => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Orchestration: run the whole loop over the current claim log
// ---------------------------------------------------------------------------

/// What one [`run_calibration`] call did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CalibrationSummary {
    pub resolved: usize,
    pub correct: usize,
    pub incorrect: usize,
    pub expired: usize,
    pub agents_scored: usize,
}

/// Resolve every due hypothesis against the current outcome claims, retire
/// each one that resolved (`Correct`/`Incorrect`/`Expired` all count as
/// "resolved" for this purpose — a hypothesis stops being an open question
/// the moment any of the three happens, even though only two of them move a
/// score), and append one updated [`ClaimEvent::CalibrationScored`] per
/// agent that had at least one newly-resolved hypothesis this run.
///
/// Requires `lease_maintenance` — this writes to the same append-only log
/// `crate::gate::run` does, under the same single-writer discipline; see
/// `crate::gate::require_maintenance_lease`'s doc.
///
/// Idempotent by construction rather than by a separate marker: a hypothesis
/// only enters `resolve`'s input once (`ClaimStatus::Candidate`), and this
/// function's own `Retired` event is what moves it out of that set for every
/// later run — so calling this twice in a row over an unchanged log resolves
/// nothing the second time, the same "nothing left to do" idempotency
/// `crate::compact` and `crate::digest` already rely on elsewhere in this
/// crate.
pub async fn run_calibration(
    store: &dyn ObjectStore,
    lease: &LeaseHandle,
    now: &str,
) -> Result<CalibrationSummary, StoreError> {
    gate::require_maintenance_lease(lease, "calibrate::run_calibration")?;

    let events = claims::list_events(store).await?;
    let previous_scores = fold_scores(events.iter());
    let folded = claims::fold(events.iter());

    let hypotheses: Vec<ClaimState> = folded
        .values()
        .filter(|s| s.claim_type == ClaimType::Hypothesis && s.status == ClaimStatus::Candidate)
        .cloned()
        .collect();
    let outcomes: Vec<ClaimState> = folded
        .values()
        .filter(|s| s.claim_type == ClaimType::Outcome)
        .cloned()
        .collect();

    let resolutions = resolve(&hypotheses, &outcomes, now);

    let mut summary = CalibrationSummary::default();
    let mut by_agent: HashMap<String, Vec<Resolution>> = HashMap::new();
    for r in &resolutions {
        match r {
            Resolution::Correct { .. } => summary.correct += 1,
            Resolution::Incorrect { .. } => summary.incorrect += 1,
            Resolution::Expired { .. } => summary.expired += 1,
        }
        by_agent
            .entry(r.agent_id().to_string())
            .or_default()
            .push(r.clone());
    }
    summary.resolved = resolutions.len();

    // Retire every resolved hypothesis so a later run does not re-resolve it
    // (see this function's own idempotency doc). The `Retired.reason` text
    // is a human-readable audit trail only — the actual scored outcome lives
    // in the agent's own `CalibrationScored` event below, never parsed back
    // out of this string.
    for r in &resolutions {
        let (hypothesis_id, reason) = match r {
            Resolution::Correct {
                hypothesis_id,
                outcome_id,
                ..
            } => (
                hypothesis_id,
                format!("hypothesis resolved correct against outcome {outcome_id}"),
            ),
            Resolution::Incorrect {
                hypothesis_id,
                outcome_id,
                ..
            } => (
                hypothesis_id,
                format!("hypothesis resolved incorrect against outcome {outcome_id}"),
            ),
            Resolution::Expired { hypothesis_id, .. } => (
                hypothesis_id,
                "hypothesis expired unresolved past its resolution date with no \
                 matching outcome — not scored as incorrect"
                    .to_string(),
            ),
        };
        append_event(
            store,
            now,
            r.agent_id(),
            &ClaimEvent::Retired {
                claim_id: hypothesis_id.clone(),
                at: now.to_string(),
                reason,
            },
        )
        .await?;
    }

    for (agent_id, new_resolutions) in &by_agent {
        let previous = previous_scores.get(agent_id);
        let updated = score_agent(agent_id, previous, new_resolutions);
        append_event(
            store,
            now,
            agent_id,
            &ClaimEvent::CalibrationScored {
                agent_id: agent_id.clone(),
                at: now.to_string(),
                resolved_count: updated.resolved_count,
                correct_count: updated.correct_count,
                incorrect_count: updated.incorrect_count,
                expired_count: updated.expired_count,
                brier_score: updated.brier_score,
                low_sample: updated.low_sample,
            },
        )
        .await?;
        summary.agents_scored += 1;
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{Evidence, ProposedClaim, Scope};
    use std::collections::BTreeMap;

    fn evidence(session: &str) -> Evidence {
        Evidence {
            session_id: session.into(),
            message_id: "m1".into(),
            excerpt_hash: format!("hash-{session}"),
            observed_at: "2026-09-01T00:00:00Z".into(),
        }
    }

    fn hypothesis(
        claim_id: &str,
        agent: &str,
        subject: &str,
        claim: &str,
        observed_at: &str,
        resolves_at: Option<&str>,
    ) -> ClaimState {
        ClaimState {
            claim_id: claim_id.into(),
            claim: claim.into(),
            claim_type: ClaimType::Hypothesis,
            subject: subject.into(),
            scope: Scope::Agent,
            observed_by: agent.into(),
            observed_at: observed_at.into(),
            evidence: vec![evidence("s1")],
            status: ClaimStatus::Candidate,
            independent_count: 0,
            confidence: 0.0,
            embedding: None,
            resolves_at: resolves_at.map(String::from),
        }
    }

    fn outcome(claim_id: &str, subject: &str, claim: &str, observed_at: &str) -> ClaimState {
        ClaimState {
            claim_id: claim_id.into(),
            claim: claim.into(),
            claim_type: ClaimType::Outcome,
            subject: subject.into(),
            scope: Scope::Agent,
            observed_by: "ci".into(),
            observed_at: observed_at.into(),
            evidence: vec![evidence("s2")],
            status: ClaimStatus::Candidate,
            independent_count: 0,
            confidence: 0.0,
            embedding: None,
            resolves_at: None,
        }
    }

    // ---- resolve(): correct / incorrect / expired / pending are distinct ----

    #[test]
    fn resolve_matches_a_hypothesis_to_an_agreeing_outcome_as_correct() {
        let h = hypothesis(
            "h1",
            "cc-01",
            "flake-theory",
            "the flake is a colima scheduling artifact",
            "2026-09-01T00:00:00Z",
            Some("2026-09-08T00:00:00Z"),
        );
        let o = outcome(
            "o1",
            "flake-theory",
            "the flake is a colima scheduling artifact",
            "2026-09-05T00:00:00Z",
        );
        let resolutions = resolve(&[h], &[o], "2026-09-06T00:00:00Z");
        assert_eq!(resolutions.len(), 1);
        assert_eq!(
            resolutions[0],
            Resolution::Correct {
                hypothesis_id: "h1".into(),
                outcome_id: "o1".into(),
                agent_id: "cc-01".into(),
            }
        );
    }

    #[test]
    fn resolve_matches_a_hypothesis_to_a_disagreeing_outcome_as_incorrect() {
        let h = hypothesis(
            "h1",
            "cc-01",
            "flake-theory",
            "the flake is a colima scheduling artifact",
            "2026-09-01T00:00:00Z",
            Some("2026-09-08T00:00:00Z"),
        );
        let o = outcome(
            "o1",
            "flake-theory",
            "network timeouts caused the flaky test, unrelated to any scheduler",
            "2026-09-05T00:00:00Z",
        );
        let resolutions = resolve(&[h], &[o], "2026-09-06T00:00:00Z");
        assert_eq!(resolutions.len(), 1);
        assert_eq!(
            resolutions[0],
            Resolution::Incorrect {
                hypothesis_id: "h1".into(),
                outcome_id: "o1".into(),
                agent_id: "cc-01".into(),
            }
        );
    }

    #[test]
    fn resolve_marks_a_hypothesis_expired_once_past_its_resolution_date_with_no_outcome() {
        let h = hypothesis(
            "h1",
            "cc-01",
            "flake-theory",
            "the flake is a colima scheduling artifact",
            "2026-09-01T00:00:00Z",
            Some("2026-09-08T00:00:00Z"),
        );
        let resolutions = resolve(&[h], &[], "2026-09-10T00:00:00Z");
        assert_eq!(resolutions.len(), 1);
        assert_eq!(
            resolutions[0],
            Resolution::Expired {
                hypothesis_id: "h1".into(),
                agent_id: "cc-01".into(),
            }
        );
    }

    #[test]
    fn resolve_reports_nothing_for_a_hypothesis_not_yet_due_and_unanswered() {
        let h = hypothesis(
            "h1",
            "cc-01",
            "flake-theory",
            "the flake is a colima scheduling artifact",
            "2026-09-01T00:00:00Z",
            Some("2026-09-08T00:00:00Z"),
        );
        // now is BEFORE resolves_at, and no outcome exists — still pending.
        let resolutions = resolve(&[h], &[], "2026-09-05T00:00:00Z");
        assert!(
            resolutions.is_empty(),
            "a hypothesis not yet due must not be reported as expired or otherwise resolved: {resolutions:?}"
        );
    }

    // ---- score_agent(): correct raises, incorrect lowers, expired does NEITHER ----

    #[test]
    fn a_correctly_resolved_hypothesis_raises_the_agents_score() {
        let baseline = score_agent("cc-01", None, &[]);
        let after_correct = score_agent(
            "cc-01",
            None,
            &[Resolution::Correct {
                hypothesis_id: "h1".into(),
                outcome_id: "o1".into(),
                agent_id: "cc-01".into(),
            }],
        );
        assert!(
            after_correct.reliability() > baseline.reliability(),
            "a correct resolution must raise reliability: {baseline:?} -> {after_correct:?}"
        );
        assert_eq!(after_correct.correct_count, 1);
        assert_eq!(after_correct.resolved_count, 1);
    }

    #[test]
    fn an_incorrectly_resolved_hypothesis_lowers_the_agents_score() {
        // Start from a track record with SOME correct resolutions so there is
        // room to fall — starting from the zero-sample baseline (reliability
        // fixed at exactly 0.5) would make "lower" trivially true for any
        // nonzero incorrect count and prove nothing about the direction of
        // the effect specifically.
        let good_start = AgentScore {
            agent_id: "cc-01".into(),
            resolved_count: 4,
            correct_count: 4,
            incorrect_count: 0,
            expired_count: 0,
            brier_score: 0.0,
            low_sample: true,
        };
        let after_incorrect = score_agent(
            "cc-01",
            Some(&good_start),
            &[Resolution::Incorrect {
                hypothesis_id: "h2".into(),
                outcome_id: "o2".into(),
                agent_id: "cc-01".into(),
            }],
        );
        assert!(
            after_incorrect.reliability() < good_start.reliability(),
            "an incorrect resolution must lower reliability: {good_start:?} -> {after_incorrect:?}"
        );
        assert_eq!(after_incorrect.incorrect_count, 1);
        assert_eq!(after_incorrect.resolved_count, 5);
    }

    #[test]
    fn an_expired_hypothesis_moves_neither_correct_nor_incorrect_nor_the_brier_score() {
        let baseline = score_agent("cc-01", None, &[]);
        let after_expired = score_agent(
            "cc-01",
            None,
            &[Resolution::Expired {
                hypothesis_id: "h3".into(),
                agent_id: "cc-01".into(),
            }],
        );
        assert_eq!(
            after_expired.reliability(),
            baseline.reliability(),
            "an expired-unresolved hypothesis must not move reliability at all"
        );
        assert_eq!(
            after_expired.resolved_count, 0,
            "expired must not count as resolved"
        );
        assert_eq!(after_expired.correct_count, 0);
        assert_eq!(after_expired.incorrect_count, 0);
        assert_eq!(
            after_expired.expired_count, 1,
            "but it must still be recorded somewhere"
        );
    }

    #[test]
    fn score_agent_ignores_resolutions_belonging_to_a_different_agent() {
        let mine = Resolution::Correct {
            hypothesis_id: "h1".into(),
            outcome_id: "o1".into(),
            agent_id: "cc-01".into(),
        };
        let someone_elses = Resolution::Incorrect {
            hypothesis_id: "h2".into(),
            outcome_id: "o2".into(),
            agent_id: "cc-02".into(),
        };
        let score = score_agent("cc-01", None, &[mine, someone_elses]);
        assert_eq!(score.correct_count, 1);
        assert_eq!(
            score.incorrect_count, 0,
            "cc-02's incorrect resolution must not count against cc-01"
        );
    }

    // ---- a tiny sample is flagged, not presented as authoritative ----

    #[test]
    fn a_tiny_perfect_sample_is_flagged_low_sample_and_shrunk_toward_neutral() {
        // Three-for-three: a naive reading says "1.00, flawless." This must
        // both (a) be flagged low_sample, and (b) NOT actually behave like a
        // flawless, fully-trusted agent — the shrinkage must be doing real
        // work, not just the flag being set decoratively.
        let tiny_perfect = score_agent(
            "cc-01",
            None,
            &[
                Resolution::Correct {
                    hypothesis_id: "h1".into(),
                    outcome_id: "o1".into(),
                    agent_id: "cc-01".into(),
                },
                Resolution::Correct {
                    hypothesis_id: "h2".into(),
                    outcome_id: "o2".into(),
                    agent_id: "cc-01".into(),
                },
                Resolution::Correct {
                    hypothesis_id: "h3".into(),
                    outcome_id: "o3".into(),
                    agent_id: "cc-01".into(),
                },
            ],
        );
        assert_eq!(
            tiny_perfect.brier_score, 0.0,
            "the raw record really is flawless"
        );
        assert!(
            tiny_perfect.low_sample,
            "three resolved predictions must be flagged low_sample"
        );

        // A large, equally flawless record should be trusted much more.
        let large_perfect_resolutions: Vec<Resolution> = (0..200)
            .map(|i| Resolution::Correct {
                hypothesis_id: format!("h{i}"),
                outcome_id: format!("o{i}"),
                agent_id: "cc-02".into(),
            })
            .collect();
        let large_perfect = score_agent("cc-02", None, &large_perfect_resolutions);
        assert!(!large_perfect.low_sample);

        assert!(
            tiny_perfect.trust_multiplier() < large_perfect.trust_multiplier(),
            "a 3-sample perfect record must be trusted LESS than a 200-sample one, \
             not presented as equally (or more) authoritative: {} vs {}",
            tiny_perfect.trust_multiplier(),
            large_perfect.trust_multiplier()
        );
        // And it must sit meaningfully below the large sample's near-ceiling
        // multiplier — not just "less than" by a rounding error.
        assert!(
            tiny_perfect.trust_multiplier() < large_perfect.trust_multiplier() - 0.05,
            "shrinkage on a tiny sample must be substantial, not cosmetic: {} vs {}",
            tiny_perfect.trust_multiplier(),
            large_perfect.trust_multiplier()
        );
    }

    #[test]
    fn zero_resolved_predictions_is_neutral_not_a_penalty() {
        let brand_new = score_agent("cc-01", None, &[]);
        assert_eq!(
            brand_new.trust_multiplier(),
            1.0,
            "an agent with no resolved track record yet must not be discounted"
        );
    }

    // ---- derive_confidence actually changes with calibration ----

    #[test]
    fn derived_confidence_differs_for_a_well_calibrated_vs_poorly_calibrated_agent() {
        let good = AgentScore {
            agent_id: "cc-good".into(),
            resolved_count: 40,
            correct_count: 38,
            incorrect_count: 2,
            expired_count: 0,
            brier_score: 2.0 / 40.0,
            low_sample: false,
        };
        let bad = AgentScore {
            agent_id: "cc-bad".into(),
            resolved_count: 40,
            correct_count: 4,
            incorrect_count: 36,
            expired_count: 0,
            brier_score: 36.0 / 40.0,
            low_sample: false,
        };
        // Same independent_count for both — the ONLY thing that differs is
        // the observing agent's calibration. A gate that silently fell back
        // to a fixed formula (or to a self-reported number the schema does
        // not even carry) would produce the exact same confidence for both,
        // which this test would catch.
        let neutral_confidence = gate_derive_confidence_for_test(2, None);
        let good_confidence = gate_derive_confidence_for_test(2, Some(&good));
        let bad_confidence = gate_derive_confidence_for_test(2, Some(&bad));

        assert!(
            good_confidence > neutral_confidence,
            "a well-calibrated agent's confidence must exceed the no-track-record baseline"
        );
        assert!(
            bad_confidence < neutral_confidence,
            "a poorly-calibrated agent's confidence must fall below the no-track-record baseline"
        );
        assert!(
            good_confidence > bad_confidence,
            "two structurally identical candidates from differently-calibrated agents \
             must not receive the same derived confidence"
        );
    }

    /// `crate::gate::derive_confidence` is private to that module (it is an
    /// internal step of `run_gate`, not part of the gate's public surface —
    /// see that module's doc). This test needs to observe it directly to
    /// prove calibration changes the number at all, distinctly from proving
    /// it end-to-end through a full `run_gate` call (see
    /// `crate::gate`'s own calibration test for that). Rather than widen
    /// `derive_confidence`'s visibility for one test, this reimplements its
    /// exact formula — any drift between the two would itself be a bug worth
    /// catching, and `crate::gate`'s own end-to-end test below closes that
    /// gap by exercising the real function.
    fn gate_derive_confidence_for_test(
        independent_count: u32,
        calibration: Option<&AgentScore>,
    ) -> f64 {
        let structural = (0.5 + 0.15 * independent_count as f64).min(0.95_f64);
        let trust = calibration.map(AgentScore::trust_multiplier).unwrap_or(1.0);
        (structural * trust).clamp(0.05, 0.95)
    }

    // ---- fold_scores / fold_quarantine over a real event log ----

    #[test]
    fn fold_scores_keeps_the_latest_cumulative_record_per_agent() {
        let events = vec![
            ClaimEvent::CalibrationScored {
                agent_id: "cc-01".into(),
                at: "2026-09-01T00:00:00Z".into(),
                resolved_count: 1,
                correct_count: 1,
                incorrect_count: 0,
                expired_count: 0,
                brier_score: 0.0,
                low_sample: true,
            },
            ClaimEvent::CalibrationScored {
                agent_id: "cc-01".into(),
                at: "2026-09-05T00:00:00Z".into(),
                resolved_count: 3,
                correct_count: 2,
                incorrect_count: 1,
                expired_count: 0,
                brier_score: 1.0 / 3.0,
                low_sample: true,
            },
        ];
        let scores = fold_scores(events.iter());
        let latest = scores.get("cc-01").unwrap();
        assert_eq!(
            latest.resolved_count, 3,
            "must keep the LATEST cumulative record, not the first"
        );
    }

    #[test]
    fn fold_quarantine_reflects_quarantine_then_unquarantine_in_order() {
        let events = vec![ClaimEvent::AgentQuarantined {
            agent_id: "cc-99".into(),
            at: "2026-09-01T00:00:00Z".into(),
            reason: "rising contradiction rate".into(),
        }];
        let quarantined = fold_quarantine(events.iter());
        assert!(quarantined.contains("cc-99"));

        let mut with_unquarantine = events;
        with_unquarantine.push(ClaimEvent::AgentUnquarantined {
            agent_id: "cc-99".into(),
            at: "2026-09-02T00:00:00Z".into(),
        });
        let after = fold_quarantine(with_unquarantine.iter());
        assert!(
            !after.contains("cc-99"),
            "unquarantine must reverse it when folded in order"
        );
    }

    // ---- contradiction_rates: the early-warning metric ----

    #[test]
    fn contradiction_rates_flags_an_agent_with_a_rising_contested_ratio() {
        fn state(id: &str, agent: &str, status: ClaimStatus) -> ClaimState {
            ClaimState {
                claim_id: id.into(),
                claim: "x".into(),
                claim_type: ClaimType::Convention,
                subject: "s".into(),
                scope: Scope::Fleet,
                observed_by: agent.into(),
                observed_at: "2026-09-01T00:00:00Z".into(),
                evidence: vec![evidence("s1")],
                status,
                independent_count: 2,
                confidence: 0.6,
                embedding: None,
                resolves_at: None,
            }
        }
        let mut states = BTreeMap::new();
        // cc-flaky: 1 promoted, 3 contested — a clearly rising rate.
        states.insert(
            "c1".to_string(),
            state("c1", "cc-flaky", ClaimStatus::Promoted),
        );
        states.insert(
            "c2".to_string(),
            state("c2", "cc-flaky", ClaimStatus::Contested),
        );
        states.insert(
            "c3".to_string(),
            state("c3", "cc-flaky", ClaimStatus::Contested),
        );
        states.insert(
            "c4".to_string(),
            state("c4", "cc-flaky", ClaimStatus::Contested),
        );
        // cc-steady: 4 promoted, 0 contested.
        for (i, id) in ["c5", "c6", "c7", "c8"].iter().enumerate() {
            states.insert(
                id.to_string(),
                state(id, "cc-steady", ClaimStatus::Promoted).clone_with_id(i),
            );
        }

        let rates = contradiction_rates(&states);
        let flaky = rates.get("cc-flaky").unwrap();
        let steady = rates.get("cc-steady").unwrap();
        assert!(
            flaky.rate() > 0.5,
            "an agent with mostly-contested claims must show a high contradiction rate: {flaky:?}"
        );
        assert_eq!(steady.rate(), 0.0);
        assert!(flaky.rate() > steady.rate());
    }

    /// Small helper only for the test above, so four otherwise-identical
    /// `ClaimState`s (same claim_id would collide as one BTreeMap entry) get
    /// distinct claim_ids without hand-writing four near-duplicate literals.
    trait WithDistinctId {
        fn clone_with_id(self, i: usize) -> Self;
    }
    impl WithDistinctId for ClaimState {
        fn clone_with_id(mut self, i: usize) -> Self {
            self.claim_id = format!("{}-{i}", self.claim_id);
            self
        }
    }

    // ---- end-to-end quarantine: stop promoting, capture continues, demote ----

    async fn test_maintenance_lease(store: &dyn ObjectStore) -> LeaseHandle {
        let key = ctxlake_store::layout::lease_maintenance();
        ctxlake_store::lease::provision(store, &key).await.unwrap();
        match ctxlake_store::lease::acquire(
            store,
            &ctxlake_store::clock::SystemClock,
            &key,
            "test-maintenance-runner",
            None,
            std::time::Duration::from_secs(300),
        )
        .await
        .unwrap()
        {
            ctxlake_store::lease::AcquireOutcome::Acquired(handle) => handle,
            ctxlake_store::lease::AcquireOutcome::NotAcquired { .. } => {
                panic!("a freshly provisioned lease on an empty store must always be acquirable")
            }
        }
    }

    /// Evidence whose `observed_at` sits inside `windows_for`'s default
    /// window below (`2026-09-09`), unlike the module-level `evidence()`
    /// helper above (fixed at `2026-09-01`, fine for the hypothesis/outcome
    /// tests that never run these claims through the provenance gate). Using
    /// the wrong one here would fail every candidate at provenance before
    /// quarantine ever gets a chance to matter, which is not what any test
    /// in this section is trying to exercise.
    fn env_evidence(session: &str) -> Evidence {
        Evidence {
            session_id: session.into(),
            message_id: "m1".into(),
            excerpt_hash: format!("hash-{session}"),
            observed_at: "2026-09-09T12:00:00Z".into(),
        }
    }

    fn env_claim(
        claim_id: &str,
        agent: &str,
        claim: &str,
        subject: &str,
        session: &str,
    ) -> ProposedClaim {
        ProposedClaim {
            claim_id: claim_id.into(),
            claim: claim.into(),
            claim_type: ClaimType::Environment,
            subject: subject.into(),
            scope: Scope::Agent,
            observed_by: agent.into(),
            observed_at: "2026-09-09T12:00:00Z".into(),
            evidence: vec![env_evidence(session)],
            embedding: None,
            resolves_at: None,
        }
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

    #[tokio::test]
    async fn quarantine_stops_promotion_demotes_promoted_and_capture_continues() {
        let store = object_store::memory::InMemory::new();
        let lease = test_maintenance_lease(&store).await;
        let known_agents: HashSet<String> = ["cc-99".to_string()].into_iter().collect();

        // 1. cc-99 already has a PROMOTED claim (simulated directly, as if an
        //    earlier gate run had already promoted it).
        let already_promoted = ClaimState {
            claim_id: "already-promoted".into(),
            claim: "staging SSH listens on 2222".into(),
            claim_type: ClaimType::Environment,
            subject: "staging".into(),
            scope: Scope::Fleet,
            observed_by: "cc-99".into(),
            observed_at: "2026-09-09T12:00:00Z".into(),
            evidence: vec![evidence("s0")],
            status: ClaimStatus::Promoted,
            independent_count: 1,
            confidence: 0.6,
            embedding: None,
            resolves_at: None,
        };
        claims::append_proposed(
            &store,
            "2026-09-09",
            &env_claim(
                "already-promoted",
                "cc-99",
                "staging SSH listens on 2222",
                "staging",
                "s0",
            ),
        )
        .await
        .unwrap();
        claims::publish_fleet_state(&store, &already_promoted)
            .await
            .unwrap();
        // Fake the Promoted event too, so `claims::fold` agrees with the
        // fleet-state mirror above about this claim's status.
        append_event(
            &store,
            "2026-09-09T12:01:00Z",
            "cc-99",
            &ClaimEvent::Promoted {
                claim_id: "already-promoted".into(),
                at: "2026-09-09T12:01:00Z".into(),
                independent_count: 1,
                confidence: 0.6,
            },
        )
        .await
        .unwrap();

        // 2. Quarantine cc-99.
        quarantine(
            &store,
            &lease,
            "cc-99",
            "2026-09-10T00:00:00Z",
            "rising contradiction rate",
        )
        .await
        .unwrap();

        // 3. Capture continues: a fresh proposal from the now-quarantined
        //    agent must still land.
        claims::append_proposed(
            &store,
            "2026-09-10",
            &env_claim(
                "new-candidate",
                "cc-99",
                "a second observation after quarantine",
                "staging-2",
                "s1",
            ),
        )
        .await
        .unwrap();
        let events_after_capture = claims::list_events(&store).await.unwrap();
        assert!(
            events_after_capture
                .iter()
                .any(|e| e.claim_id() == Some("new-candidate")),
            "capture must continue for a quarantined agent — no gap where it used to be"
        );

        // 4. Run the real gate. Both effects must show up in ONE run:
        let windows = windows_for(&["s0", "s1"]);
        let injected: HashMap<String, HashSet<String>> = HashMap::new();
        let summary = gate::run(
            &store,
            &lease,
            "2026-09-10T00:05:00Z",
            &known_agents,
            &windows,
            &injected,
            |_e| true,
        )
        .await
        .unwrap();

        assert_eq!(
            summary.blocked_by_quarantine, 1,
            "the new candidate must be blocked, not promoted"
        );
        assert_eq!(
            summary.promoted, 0,
            "nothing from a quarantined agent may promote"
        );
        assert_eq!(
            summary.demoted_by_quarantine, 1,
            "the pre-existing promoted claim must be demoted"
        );

        let fleet = claims::list_fleet_claims(&store).await.unwrap();
        let demoted = fleet
            .iter()
            .find(|c| c.claim_id == "already-promoted")
            .unwrap();
        assert_eq!(
            demoted.status,
            ClaimStatus::Contested,
            "must move to contested, not stay promoted or be deleted"
        );

        let all_events = claims::list_events(&store).await.unwrap();
        let folded = claims::fold(all_events.iter());
        assert_eq!(
            folded.get("new-candidate").unwrap().status,
            ClaimStatus::Candidate,
            "the blocked candidate must still be on file as a candidate, not vanished"
        );
    }

    #[tokio::test]
    async fn unquarantining_restores_normal_promotion_for_new_candidates() {
        let store = object_store::memory::InMemory::new();
        let lease = test_maintenance_lease(&store).await;
        let known_agents: HashSet<String> = ["cc-07".to_string()].into_iter().collect();

        quarantine(
            &store,
            &lease,
            "cc-07",
            "2026-09-10T00:00:00Z",
            "temporary hold",
        )
        .await
        .unwrap();

        // While quarantined, a qualifying candidate must not promote.
        claims::append_proposed(
            &store,
            "2026-09-10",
            &env_claim(
                "blocked-1",
                "cc-07",
                "staging listens on 2222",
                "staging",
                "s1",
            ),
        )
        .await
        .unwrap();
        let windows = windows_for(&["s1", "s2"]);
        let injected: HashMap<String, HashSet<String>> = HashMap::new();
        let summary = gate::run(
            &store,
            &lease,
            "2026-09-10T00:05:00Z",
            &known_agents,
            &windows,
            &injected,
            |_e| true,
        )
        .await
        .unwrap();
        assert_eq!(summary.promoted, 0);
        assert_eq!(summary.blocked_by_quarantine, 1);

        // Un-quarantine, then a NEW qualifying candidate must promote normally.
        unquarantine(&store, &lease, "cc-07", "2026-09-11T00:00:00Z")
            .await
            .unwrap();
        claims::append_proposed(
            &store,
            "2026-09-11",
            &env_claim(
                "allowed-1",
                "cc-07",
                "a brand new environment fact",
                "staging-3",
                "s2",
            ),
        )
        .await
        .unwrap();
        let summary_after = gate::run(
            &store,
            &lease,
            "2026-09-11T00:05:00Z",
            &known_agents,
            &windows,
            &injected,
            |_e| true,
        )
        .await
        .unwrap();
        // "Restores normal behavior" means every one of cc-07's still-open
        // candidates is re-evaluated on its own merits again, not only ones
        // proposed after the un-quarantine — `gate::run` re-examines every
        // `Candidate` claim on every call, so `blocked-1` (which failed
        // nothing but the now-lifted quarantine check) promotes right
        // alongside the brand-new `allowed-1`.
        assert_eq!(
            summary_after.promoted, 2,
            "both the previously-blocked candidate and the new one must promote \
             once quarantine is lifted"
        );
        assert_eq!(summary_after.blocked_by_quarantine, 0);

        let all_events = claims::list_events(&store).await.unwrap();
        let folded = claims::fold(all_events.iter());
        assert_eq!(
            folded.get("blocked-1").unwrap().status,
            ClaimStatus::Promoted,
            "un-quarantining must let a previously-blocked candidate promote on \
             the next run, not leave it stuck forever"
        );
        assert_eq!(
            folded.get("allowed-1").unwrap().status,
            ClaimStatus::Promoted
        );
    }

    // ---- run_calibration(): the orchestration end to end ----

    #[tokio::test]
    async fn run_calibration_resolves_scores_and_retires_in_one_pass() {
        let store = object_store::memory::InMemory::new();
        let lease = test_maintenance_lease(&store).await;

        let hyp = ProposedClaim {
            claim_id: "hyp-1".into(),
            claim: "the flake is a colima scheduling artifact".into(),
            claim_type: ClaimType::Hypothesis,
            subject: "flake-theory".into(),
            scope: Scope::Agent,
            observed_by: "cc-01".into(),
            observed_at: "2026-09-01T00:00:00Z".into(),
            evidence: vec![evidence("s1")],
            embedding: None,
            resolves_at: Some("2026-09-08T00:00:00Z".into()),
        };
        claims::append_proposed(&store, "2026-09-01", &hyp)
            .await
            .unwrap();

        let out = ProposedClaim {
            claim_id: "out-1".into(),
            claim: "the flake is a colima scheduling artifact".into(),
            claim_type: ClaimType::Outcome,
            subject: "flake-theory".into(),
            scope: Scope::Agent,
            observed_by: "ci".into(),
            observed_at: "2026-09-05T00:00:00Z".into(),
            evidence: vec![evidence("s2")],
            embedding: None,
            resolves_at: None,
        };
        claims::append_proposed(&store, "2026-09-05", &out)
            .await
            .unwrap();

        let summary = run_calibration(&store, &lease, "2026-09-06T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(summary.resolved, 1);
        assert_eq!(summary.correct, 1);
        assert_eq!(summary.agents_scored, 1);

        let events = claims::list_events(&store).await.unwrap();
        let folded = claims::fold(events.iter());
        assert_eq!(
            folded.get("hyp-1").unwrap().status,
            ClaimStatus::Retired,
            "a resolved hypothesis must be retired so it is not re-resolved"
        );

        let scores = fold_scores(events.iter());
        let cc01 = scores.get("cc-01").unwrap();
        assert_eq!(cc01.correct_count, 1);
        assert_eq!(cc01.resolved_count, 1);

        // Idempotency: running again over the unchanged log resolves nothing
        // new (the hypothesis is already Retired, so it never re-enters
        // `resolve`'s input).
        let summary_again = run_calibration(&store, &lease, "2026-09-07T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(
            summary_again.resolved, 0,
            "a second run must not re-resolve an already-retired hypothesis"
        );
    }

    #[tokio::test]
    async fn run_calibration_refuses_a_lease_for_the_wrong_key() {
        let store = object_store::memory::InMemory::new();
        let wrong_key = object_store::path::Path::from("live/leases/not-maintenance");
        ctxlake_store::lease::provision(&store, &wrong_key)
            .await
            .unwrap();
        let wrong_lease = match ctxlake_store::lease::acquire(
            &store,
            &ctxlake_store::clock::SystemClock,
            &wrong_key,
            "someone-else",
            None,
            std::time::Duration::from_secs(300),
        )
        .await
        .unwrap()
        {
            ctxlake_store::lease::AcquireOutcome::Acquired(h) => h,
            ctxlake_store::lease::AcquireOutcome::NotAcquired { .. } => unreachable!(),
        };
        let err = run_calibration(&store, &wrong_lease, "2026-09-06T00:00:00Z")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("lease_maintenance"),
            "expected a lease-key error, got: {err}"
        );
    }
}
