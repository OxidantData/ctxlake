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
///
/// `observed_at` is the timestamp of *this specific message* — the envelope's own
/// `emitted_at`, computed the same untrusted-input-safe way as `excerpt_hash`, not
/// the claim-level `observed_at` on [`ProposedClaim`]/[`ClaimState`] (which is
/// "when the claim was first proposed" and does not move as more evidence
/// accrues). The provenance gate (`gate::check_provenance`) checks each citation's
/// *own* `observed_at` against *its own* session's window — checking the single
/// claim-level timestamp against every cited session's window instead would reject
/// any claim whose evidence spans more than one session, which is exactly the
/// multi-day corroboration the independence gate exists to reward, not punish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub session_id: String,
    pub message_id: String,
    pub excerpt_hash: String,
    pub observed_at: String,
}

/// Lowercase, trimmed claim text — the equality ctxlake-maint uses everywhere it
/// needs to decide "is this the same claim I already have on file," never a raw
/// `==` on the model's own casing/whitespace. Shared by [`fold`]'s callers
/// (extraction's claim_id-reuse, see `extract::find_existing_claim_id`) and the
/// contradiction gate (`gate::find_contradiction`), so the two places that ask
/// "same claim or not" can't drift into disagreeing definitions of "same."
pub(crate) fn normalize_claim_text(claim: &str) -> String {
    claim.trim().to_lowercase()
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
    /// RFC3339: when this prediction is due to be checked against reality.
    /// `None` for every claim type except `hypothesis` — see
    /// `docs/memory.md`'s "Confidence is derived, not claimed" and
    /// `crate::calibrate::resolve`, which reads this to decide "nobody ever
    /// answered this" (past due, no matching outcome) apart from "still
    /// pending" (not due yet). A hypothesis with no resolution date can still
    /// be resolved early by a matching outcome claim landing, it just never
    /// expires on the calendar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves_at: Option<String>,
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
    /// A per-agent calibration record — see `crate::calibrate`'s module doc.
    /// Not about any one claim (there is no `claim_id`, on purpose: this is
    /// the observing agent's aggregate track record, not a mutation of a
    /// claim's own state), so it lives in this same append-only log purely
    /// for the reason docs/memory.md gives: auditable and replayable like
    /// everything else here, rather than a number computed silently at read
    /// time with no history of how it moved. Every field is the *cumulative*
    /// total as of this event — the fold in `crate::calibrate::fold_scores`
    /// takes the latest one per agent, the same "current state is the last
    /// event" rule [`fold`] applies to a claim.
    CalibrationScored {
        agent_id: String,
        at: String,
        resolved_count: u32,
        correct_count: u32,
        incorrect_count: u32,
        /// Tracked for visibility only — an agent whose hypotheses keep
        /// expiring unresolved is a different (milder) signal than one whose
        /// hypotheses keep resolving wrong, and collapsing the two into one
        /// number would erase that distinction. Never enters `brier_score`.
        expired_count: u32,
        brier_score: f64,
        /// Mirrors `crate::calibrate::AgentScore::low_sample` at the moment
        /// this event was written — carried on the event itself (not just
        /// recomputed from `resolved_count` by every reader) so a reader
        /// folding history doesn't need to re-import the threshold constant
        /// to render an old score honestly.
        low_sample: bool,
    },
    /// The kill switch (docs/memory.md: "one flag per agent"). Its own event
    /// kind, not a boolean flipped in place, for the same append-only reason
    /// as everything else in this log: which agent was quarantined, when,
    /// and why must stay on the record even after it is reversed —
    /// `crate::calibrate::fold_quarantine` replays `AgentQuarantined` /
    /// [`AgentUnquarantined`] in order to answer "quarantined right now,"
    /// but neither event is ever deleted or overwritten to get there.
    AgentQuarantined {
        agent_id: String,
        at: String,
        reason: String,
    },
    /// Reverses the most recent [`AgentQuarantined`] for `agent_id`. Does
    /// **not** itself restore any claim this agent had already been demoted
    /// out of `promoted` because of the quarantine — see
    /// `crate::calibrate`'s module doc for why that stays a one-way ratchet
    /// requiring a human to actually re-review a contested claim, same as
    /// any other contradiction.
    AgentUnquarantined {
        agent_id: String,
        at: String,
    },
}

impl ClaimEvent {
    /// `None` for the two agent-scoped event kinds ([`ClaimEvent::CalibrationScored`],
    /// [`ClaimEvent::AgentQuarantined`], [`ClaimEvent::AgentUnquarantined`]) —
    /// they are not about any single claim, so returning a fake or empty
    /// `claim_id` for them would let a caller silently misattribute a
    /// per-agent record to a claim. See `crate::calibrate`'s own folds for
    /// how those three are read instead.
    pub fn claim_id(&self) -> Option<&str> {
        match self {
            ClaimEvent::Proposed(p) => Some(&p.claim_id),
            ClaimEvent::Promoted { claim_id, .. }
            | ClaimEvent::Contested { claim_id, .. }
            | ClaimEvent::Retired { claim_id, .. }
            | ClaimEvent::Superseded { claim_id, .. } => Some(claim_id),
            ClaimEvent::CalibrationScored { .. }
            | ClaimEvent::AgentQuarantined { .. }
            | ClaimEvent::AgentUnquarantined { .. } => None,
        }
    }

    /// This event's own declared timestamp — every variant carries one
    /// (`observed_at` on [`ProposedClaim`], `at` on everything else). [`list_events`]
    /// sorts on this, not on the storage key, precisely because the storage key
    /// (a ULID) is generated by a per-process, per-thread counter
    /// (`ctxlake_core::envelope::next_event_id`) that gives no cross-host ordering
    /// guarantee once more than one host may write concurrently — see
    /// `list_events`'s doc for why that stopped being safe to assume here.
    pub fn at(&self) -> &str {
        match self {
            ClaimEvent::Proposed(p) => &p.observed_at,
            ClaimEvent::Promoted { at, .. }
            | ClaimEvent::Contested { at, .. }
            | ClaimEvent::Retired { at, .. }
            | ClaimEvent::Superseded { at, .. }
            | ClaimEvent::CalibrationScored { at, .. }
            | ClaimEvent::AgentQuarantined { at, .. }
            | ClaimEvent::AgentUnquarantined { at, .. } => at,
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
    /// See [`ProposedClaim::resolves_at`]; carried through the fold the same
    /// way `embedding` is — set from whichever `Proposed` event created the
    /// claim, never overwritten by a later corroborating one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves_at: Option<String>,
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
                        if s.resolves_at.is_none() {
                            s.resolves_at = p.resolves_at.clone();
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
                        resolves_at: p.resolves_at.clone(),
                    });
            }
            ClaimEvent::Promoted {
                claim_id,
                independent_count,
                confidence,
                ..
            } => {
                if let Some(s) = out.get_mut(claim_id) {
                    // Idempotent: once a claim is Promoted, a second Promoted
                    // event for it is a no-op, not a second overwrite. This
                    // matters now that nothing serializes `gate::run` across
                    // hosts (see that module's doc) — two concurrent runs can
                    // each independently decide to promote the same still-
                    // Candidate claim from slightly different views of
                    // `claims/events/`, and each appends its own event. Without
                    // this check, whichever event happened to fold later would
                    // arbitrarily overwrite `independent_count`/`confidence`
                    // with its own numbers; with it, the first promotion to
                    // fold wins and every later one changes nothing, so the
                    // final state is the same no matter which run's event
                    // landed, or was folded, first.
                    if s.status != ClaimStatus::Promoted {
                        s.status = ClaimStatus::Promoted;
                        s.independent_count = *independent_count;
                        s.confidence = *confidence;
                    }
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
            // Agent-scoped, not claim-scoped — see `ClaimEvent`'s own doc on
            // these three variants. Folding a single claim's state is not
            // what they're for; `crate::calibrate::fold_scores` and
            // `fold_quarantine` fold this same event stream for those.
            ClaimEvent::CalibrationScored { .. }
            | ClaimEvent::AgentQuarantined { .. }
            | ClaimEvent::AgentUnquarantined { .. } => {}
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

/// Parses [`ClaimEvent::at`] as an actual instant rather than comparing it as
/// text. Every real writer (`ctxlake-mcp`'s `now_rfc3339`, `ctxlake-maint`'s
/// own) formats via `time`'s RFC3339 well-known description, but that
/// description does not pin fractional-second precision — one caller emits
/// whole seconds (`...12:00:00Z`), another variable-length microseconds
/// (`...12:00:00.976363Z`) — so two well-formed timestamps can disagree in
/// length, and therefore in lexicographic order, even though one is plainly
/// later than the other (`.` is `0x2E`, `Z` is `0x5A`, so a fractional
/// timestamp text-sorts *before* a whole-second one for the same second).
/// `None` on anything that fails to parse; [`list_events`] falls back to the
/// old text comparison for those so a malformed record degrades no worse than
/// it did before this function existed.
fn parse_at(at: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()
}

/// List and parse every event under `claims/events/`, sorted so the fold in
/// [`fold`] sees events in the right order.
///
/// Sorted by each event's own declared [`ClaimEvent::at`], **not** by the storage
/// key. An earlier version of this function sorted by key, on the theory that
/// ULIDs are lexicographically sortable and therefore a good proxy for "append
/// order" — true within one writer, since
/// `ctxlake_core::envelope::next_event_id`'s generator is monotonic *per thread*.
/// It stopped being true across writers the moment concurrent `ctxlake maint`
/// runs became sanctioned (no more maintenance lease serializing them, see
/// `crate::run`'s module doc): two hosts' independent generators can hand out
/// ULIDs for the same millisecond in either order, so a `Promoted` event minted
/// on a fast host can sort *before* the `Proposed` event that created the claim,
/// minted moments earlier on a slow one — folding that order silently drops the
/// promotion (`fold`'s `get_mut` finds no claim yet to promote). `at` is
/// caller-declared business time (`ctx_at` in `gate::run`, `observed_at` on a
/// proposal) rather than a per-thread physical write timestamp, so it orders a
/// promotion after the proposal it necessarily followed regardless of which
/// thread or host physically wrote which object first.
///
/// `at` is compared via [`parse_at`] as a real instant, not as text — an
/// earlier version compared the raw RFC3339 strings directly, which happens to
/// agree with real time when both events share the same timestamp precision
/// (every fixture in this crate's own tests, which is exactly why the bug went
/// unnoticed here) but silently disagrees across precisions: a proposal
/// stamped to the whole second and a promotion stamped moments later with
/// fractional seconds text-sort with the fraction *first* (see [`parse_at`]'s
/// doc), dropping the promotion in `fold` even on a single host writing keys
/// in the correct order. Parsing to an instant makes the comparison agree with
/// real time regardless of which precision either writer chose. Ties (equal
/// parsed instants, e.g. every event one `gate::run` call produces, or two
/// values that fail to parse) fall back to the storage key, which is a real
/// per-writer-thread order for events that actually share a writer. A single
/// malformed or half-written object is skipped rather than failing the whole
/// read, matching `roster::list_intents_directly`'s reasoning: a transient
/// partial write should not take down every other claim's fold.
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
    entries.sort_by(|a, b| {
        match (parse_at(a.1.at()), parse_at(b.1.at())) {
            (Some(ta), Some(tb)) => ta.cmp(&tb),
            // Unparseable on either side: no instant to compare against, so
            // fall back to the previous text comparison rather than an
            // arbitrary order.
            _ => a.1.at().cmp(b.1.at()),
        }
        .then_with(|| a.0.cmp(&b.0))
    });
    Ok(entries.into_iter().map(|(_, ev)| ev).collect())
}

/// Overwrite `claims/fleet/<claim_id>.json` with this claim's current folded
/// state. A plain [`ObjectStore::put`] (no CAS) is correct, not a shortcut: the
/// content written here is a claim's folded state, which is deterministic in the
/// fields that matter (status, `independent_count`, `confidence`) given the same
/// view of `claims/events/` — two `gate::run` calls computing the same promotion,
/// even from two different hosts with no lock between them, write the same bytes.
/// [`fold`]'s handling of a repeated `Promoted` event (first promotion wins, see
/// its own doc) is what keeps this key well-defined even on the rarer occasion two
/// runs' views briefly disagree. This key is always fully re-derivable from the
/// event log by re-running `fold`, the same "disposable, recomputed, plain
/// overwrite is fine" property `ctxlake_store::layout::sessions_compaction_marker`
/// relies on for the identical reason. See `layout::claim_fleet`.
///
/// `pub(crate)`, not `pub`: this is the one function in the crate that writes a
/// `Promoted`/`Contested` [`ClaimState`] straight to fleet scope, so AGENTS.md
/// invariant 9 ("nothing writes to fleet scope except the promotion gate") is
/// only as real as this function being unreachable from outside `gate::run`'s own
/// call sites. A `pub` fn here would have handed `ctxlake-cli` (which already
/// depends on this crate) — or any future crate that does — the identical
/// capability to skip every gate.
pub(crate) async fn publish_fleet_state(
    store: &dyn ObjectStore,
    state: &ClaimState,
) -> Result<(), StoreError> {
    let path = ctxlake_store::layout::claim_fleet(&state.claim_id);
    let payload = PutPayload::from(serde_json::to_vec(state)?);
    store.put(&path, payload).await?;
    Ok(())
}

/// Read every claim currently published to fleet scope, **with no shadow-mode
/// filtering at all** — this is the raw contents of `claims/fleet/`, promoted and
/// contested alike, in whatever mode the gate last ran in. `pub(crate)`, not
/// `pub`: it is an implementation detail of [`read_promoted_for_agents`] and this
/// module's own tests, not a sanctioned way for anything else to read fleet
/// claims. A `pub` version of exactly this function is what a briefing builder or
/// an MCP cache refresh would reach for first, and it would hand back promoted,
/// shadow-mode claim text with no mode argument to even pass — see
/// [`claims_visible_to_agents`]'s doc for the read path that is safe to expose.
/// `Ok(vec![])` covers both "nothing has ever been promoted" and "the prefix
/// doesn't exist yet" — neither is an error a caller needs to branch on
/// separately.
pub(crate) async fn list_fleet_claims(
    store: &dyn ObjectStore,
) -> Result<Vec<ClaimState>, StoreError> {
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
/// enforced *here*, not only where `ctxlake.toml` is parsed.
///
/// This is enforced structurally for the fleet-scope *publish* path:
/// [`list_fleet_claims`] and [`publish_fleet_state`] are both `pub(crate)`, so
/// nothing outside this crate can reach `claims/fleet/` except through this
/// function or [`read_promoted_for_agents`]. It is **not**, and cannot be, enforced
/// against [`list_events`]/[`fold`]: those are the general append-only-log
/// primitives the gate itself needs to run (including in shadow mode — shadow
/// mode still promotes claims, it just stops them from being served), so they stay
/// `pub` and shadow-unaware by design. A caller with `pub` access to this crate
/// that reaches for `fold(list_events(store).await?)` directly, instead of this
/// function, gets the same raw promoted/contested claim text shadow mode is meant
/// to hide — this function's guarantee covers callers going through the sanctioned
/// read path, not every possible way to reconstruct state from the log. Anything
/// that renders a claim into an agent's context window must go through this
/// function or [`read_promoted_for_agents`], never `fold`/`list_events` directly.
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
            observed_at: "2026-09-09T00:00:00Z".into(),
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
            resolves_at: None,
        })
    }

    #[test]
    fn normalize_claim_text_ignores_case_and_surrounding_whitespace() {
        assert_eq!(
            normalize_claim_text("  This repo uses Just, not Make  "),
            normalize_claim_text("this repo uses just, not make")
        );
        assert_ne!(
            normalize_claim_text("this repo uses just"),
            normalize_claim_text("this repo uses make")
        );
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
    fn a_repeated_promoted_event_for_an_already_promoted_claim_is_a_no_op() {
        // Nothing serializes `gate::run` across hosts any more (see that module's
        // doc) — two concurrent runs can each independently decide to promote the
        // same still-Candidate claim from slightly different views of
        // `claims/events/`, computing slightly different independent_count /
        // confidence numbers, and each appends its own Promoted event. This is
        // the property that replaces the old maintenance lease: the SECOND
        // Promoted event to fold must change nothing, so the final state is the
        // same regardless of which run's event happened to land, or fold, first.
        let events = vec![
            proposed("c1", "s1"),
            ClaimEvent::Promoted {
                claim_id: "c1".into(),
                at: "2026-09-10T00:00:00Z".into(),
                independent_count: 1,
                confidence: 0.65,
            },
            // A second, later-folded Promoted for the same claim — from a
            // concurrent run that computed different numbers — must be a no-op.
            ClaimEvent::Promoted {
                claim_id: "c1".into(),
                at: "2026-09-10T00:00:01Z".into(),
                independent_count: 2,
                confidence: 0.95,
            },
        ];
        let state = fold(events.iter());
        let c1 = state.get("c1").unwrap();
        assert_eq!(c1.status, ClaimStatus::Promoted);
        assert_eq!(
            c1.independent_count, 1,
            "the first Promoted event's numbers must win, not the second's"
        );
        assert_eq!(
            c1.confidence, 0.65,
            "a second Promoted event for an already-promoted claim must not \
             overwrite the first promotion's confidence"
        );
    }

    #[test]
    fn fold_order_matters_and_is_the_callers_responsibility() {
        // Applying events out of order (Contested before its Promoted) produces a
        // wrong-but-deterministic answer — this test documents that fold trusts
        // its caller's ordering rather than silently reordering by any field of
        // its own, which is why list_events sorts by each event's declared `at`
        // before folding (see that function's doc).
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

    #[tokio::test]
    async fn list_events_orders_by_declared_at_not_by_storage_key() {
        // Regression test for exactly the bug two concurrent `gate::run` calls
        // exposed once nothing serializes them any more (see `list_events`'s
        // doc): two hosts' independent, per-thread ULID generators
        // (`ctxlake_core::envelope::next_event_id`) can hand out storage keys in
        // an order that disagrees with which decision actually happened first.
        // Simulate that directly by writing a `Promoted` event under a key that
        // sorts *before* its own claim's `Proposed` event's key — the opposite
        // of real append order — and confirm the fold still promotes, because
        // ordering is by each event's own declared `at`, not by key.
        let store = object_store::memory::InMemory::new();
        let claim_id = "c1";
        let promoted = ClaimEvent::Promoted {
            claim_id: claim_id.into(),
            at: "2026-09-09T12:05:00Z".into(),
            independent_count: 1,
            confidence: 0.65,
        };
        let proposal = proposed(claim_id, "s1"); // observed_at: 2026-09-09T00:00:00Z, from evidence()'s default

        let early_key =
            object_store::path::Path::from("claims/events/dt=2026-09-09/agent=cc-01/00-early.json");
        let late_key =
            object_store::path::Path::from("claims/events/dt=2026-09-09/agent=cc-01/99-late.json");
        // The LATER decision (Promoted) gets the LEXICOGRAPHICALLY EARLIER key —
        // exactly what a fast host's generator can produce relative to a slow
        // host's, per the module doc.
        store
            .put(
                &early_key,
                PutPayload::from(serde_json::to_vec(&promoted).unwrap()),
            )
            .await
            .unwrap();
        store
            .put(
                &late_key,
                PutPayload::from(serde_json::to_vec(&proposal).unwrap()),
            )
            .await
            .unwrap();

        let events = list_events(&store).await.unwrap();
        let folded = fold(events.iter());
        assert_eq!(
            folded.get(claim_id).unwrap().status,
            ClaimStatus::Promoted,
            "a Promoted event minted with an earlier storage key than its own \
             Proposed event must still fold correctly, ordered by declared `at`"
        );
    }

    #[tokio::test]
    async fn list_events_orders_mixed_timestamp_precision_by_real_time_not_text() {
        // Regression test for a second bug in the same comparison: even with
        // storage keys in the CORRECT append order (single writer, single
        // host — no concurrency involved at all), comparing `at` as text
        // rather than as a parsed instant still mis-orders two otherwise
        // well-formed RFC3339 timestamps whenever they differ in
        // fractional-second precision. `ctxlake-mcp`'s `now_rfc3339` and
        // `ctxlake-maint`'s own both format via `time`'s RFC3339 well-known
        // description, which does not pin how many fractional digits come
        // out — so a whole-second proposal followed moments later by a
        // sub-second promotion is exactly what real production traffic
        // produces, not a contrived input.
        let store = object_store::memory::InMemory::new();
        let claim_id = "c1";
        let mut proposal = proposed(claim_id, "s1");
        if let ClaimEvent::Proposed(p) = &mut proposal {
            p.observed_at = "2026-09-09T12:00:00Z".into();
        }
        let promoted = ClaimEvent::Promoted {
            claim_id: claim_id.into(),
            // Chronologically later than the proposal above, but — because
            // '.' (0x2E) sorts before 'Z' (0x5A) — lexicographically EARLIER
            // as text, even though `00.json` < `01.json` already reflects the
            // true, correct append order.
            at: "2026-09-09T12:00:00.123456Z".into(),
            independent_count: 1,
            confidence: 0.65,
        };

        let proposal_key =
            object_store::path::Path::from("claims/events/dt=2026-09-09/agent=cc-01/00.json");
        let promoted_key =
            object_store::path::Path::from("claims/events/dt=2026-09-09/agent=cc-01/01.json");
        store
            .put(
                &proposal_key,
                PutPayload::from(serde_json::to_vec(&proposal).unwrap()),
            )
            .await
            .unwrap();
        store
            .put(
                &promoted_key,
                PutPayload::from(serde_json::to_vec(&promoted).unwrap()),
            )
            .await
            .unwrap();

        let events = list_events(&store).await.unwrap();
        let folded = fold(events.iter());
        assert_eq!(
            folded.get(claim_id).unwrap().status,
            ClaimStatus::Promoted,
            "a whole-second Proposed followed by a sub-second-precision \
             Promoted must fold in real-time order even though the two \
             timestamps text-sort the other way"
        );
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
            resolves_at: None,
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
            resolves_at: None,
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
            resolves_at: None,
        };
        append_proposed(&store, "2026-09-09", &p).await.unwrap();
        let events = list_events(&store).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].claim_id(), Some("c1"));
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
