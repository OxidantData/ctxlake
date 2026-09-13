//! `ctxlake claims` / `ctxlake quarantine` — the human review surface docs/
//! memory.md and docs/memory.md promise:
//!
//! ```text
//! ctxlake claims --status candidate --explain   which gate rejected what, and why
//! ctxlake claims --status contested             the human review queue
//! ctxlake claims --status promoted
//! ctxlake quarantine <agent_id>                 the kill switch
//! ```
//!
//! **What this module is not.** `ctxlake-maint` — the crate that owns compaction,
//! extraction, and the real four-gate promotion pipeline — is an empty scaffold as
//! of this wave (a separate wave-3 track owns it; see `maint_cmd.rs`'s module doc
//! and AGENTS.md invariant 9: only the gate promotes). Nothing here writes a
//! promoted claim, moves a claim through the real pipeline, or is the authority on
//! whether a claim *will* promote. `--explain` is a read-only, best-effort
//! evaluator applying docs/memory.md's own documented rules to real candidate data
//! — good enough to tell an operator *why a candidate looks stuck today*, not a
//! second implementation of the gate. The contradiction gate needs data this
//! schema does not carry yet — a promoted-claim contradiction index — and
//! [`explain`] says so plainly rather than guessing; it never reports
//! `Gate::Contradiction`. The independence gate is the opposite case: this
//! schema *cannot* carry what it needs (`injected_context` lineage), so rather
//! than skip it, [`explain`] reports it as failed-unresolved for every
//! `convention` candidate that clears the raw observer count — a raw count is
//! not evidence of independence, and docs/memory.md is explicit that thresholds
//! read `independent_count`, never `evidence_count`.
//!
//! **Where candidates live.** `memory_propose` (`ctxlake-mcp`) writes a
//! `claim_propose` record to the local spool; `ctxlake sync`'s upload loop ships
//! it into the object store under `claims/events/dt=.../agent=.../<ulid>.json`
//! (`ctxlake_store::layout::claim_event`). [`load_candidates`] lists and reads
//! that same prefix directly from the store — an operator-triggered, occasional
//! read, not a hook-path one, so AGENTS.md invariant 1 does not apply here any
//! more than it does to `ctxlake doctor`'s own store round trip.
//!
//! **Where promoted/contested claims live.** Nothing writes those to the object
//! store yet (no gate exists). The only place they can be read from today is the
//! local fleet cache mirror `ctxlake-mcp`'s `memory_search` already reads —
//! `<cache_root>/<fleet_id>/claims.json`, in `ctxlake_mcp::memory::ClaimRecord`
//! shape. `--status promoted`/`--status contested` read that same file so this
//! command and `memory_search` can never disagree about what a promoted claim
//! looks like.
//!
//! **The quarantine prefix.** `claims/quarantine/<agent_id>.json` is a new leaf
//! this wave introduces. It is not defined in `ctxlake_store::layout` — that
//! module is owned by a sibling wave-3 track for this wiring wave (see this
//! crate's task brief) — so [`quarantine_key`] mirrors `layout::claim_event`'s
//! *shape* (a `claims/` sub-prefix, one JSON object per dynamic id, built via
//! [`object_store::path::Path::join`] so an adversarial `agent_id` cannot escape
//! its directory — see `layout.rs`'s own
//! `layout_segments_cannot_escape_their_directory` test, which this module's
//! [`quarantine_key_cannot_escape_its_directory`] mirrors) without editing a file
//! outside this wave's scope. Folding this into `ctxlake_store::layout` proper is
//! the natural follow-up once this lands.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result};
use futures::StreamExt;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use serde::Deserialize;

use ctxlake_mcp::memory::ClaimRecord;

use crate::config::Config;
use crate::paths;
use crate::sanitize::sanitize;
use crate::store_ctx::{self, full_path, StoreCtx};

fn claims_events_prefix() -> StorePath {
    StorePath::from("claims").join("events")
}

fn claims_quarantine_prefix() -> StorePath {
    StorePath::from("claims").join("quarantine")
}

fn quarantine_key(agent_id: &str) -> StorePath {
    claims_quarantine_prefix().join(format!("{agent_id}.json"))
}

/// The `claim_propose` record shape `ctxlake_mcp::memory::propose` writes — see
/// that function's doc. `evidence` is deserialized (not dropped) specifically so
/// [`explain`] can enforce docs/memory.md's "no evidence, no claim" rule against
/// the record actually read from the store, rather than trusting that whatever
/// wrote it went through `memory_propose`'s own copy of that check — see
/// [`CandidateGroup::all_events_cite_evidence`]. `injected_context` lineage
/// (independence) and a contradiction index are still absent from a candidate
/// and not yet meaningful (there is no `subject` field on the wire today),
/// which is exactly why [`explain`] is explicit about which gates it cannot
/// evaluate.
#[derive(Debug, Clone, Deserialize)]
struct CandidateEvent {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    id: String,
    claim: String,
    claim_type: String,
    #[serde(default)]
    status: String,
    observed_by: String,
    /// `(session_id, message_id)` citations `memory_propose` requires at write
    /// time. Only its emptiness is used here — this evaluator never inspects a
    /// citation's shape or tries to resolve it against a real session (that
    /// would need transcript access this crate does not have).
    #[serde(default)]
    evidence: Vec<serde_json::Value>,
}

/// One logical claim, folded from every `claim_propose` event that asserts the
/// same `(claim_type, claim text)` pair. This is a coarser grouping than a real
/// gate would use (docs/memory.md's independence math wants session-level
/// evidence, not agent-level) — documented, not hidden, in [`explain`]'s own doc.
#[derive(Debug, Clone)]
pub struct CandidateGroup {
    pub claim_type: String,
    pub claim: String,
    pub observers: BTreeSet<String>,
    pub event_ids: Vec<String>,
    /// `false` when at least one `claim_propose` event folded into this group
    /// carried an empty `evidence` array. `memory_propose` (`ctxlake-mcp`)
    /// already refuses to queue such a record — this field exists for every
    /// record that reaches `claims/events/` some other way (a hand-written
    /// object, an older or future writer, a direct store edit), so `--explain`
    /// cannot be talked into `would_promote: true` for a zero-citation claim
    /// just because it bypassed the one process that checks at ingest time.
    pub all_events_cite_evidence: bool,
}

/// List and parse every `claim_propose` candidate currently in the object store,
/// grouped by claim. Malformed or unrelated objects under the prefix (a future
/// schema, a partial write) are skipped rather than failing the whole listing —
/// the same fail-open-on-one-bad-record choice `ctxlake_mcp::memory::search` makes
/// for its own cache file.
pub async fn load_candidates(ctx: &StoreCtx) -> Result<Vec<CandidateGroup>> {
    let prefix = full_path(ctx, &claims_events_prefix());
    let mut stream = ctx.store.list(Some(&prefix));
    let mut groups: HashMap<(String, String), CandidateGroup> = HashMap::new();

    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        let Ok(get_result) = ctx.store.get(&meta.location).await else {
            continue;
        };
        let Ok(bytes) = get_result.bytes().await else {
            continue;
        };
        let Ok(event) = serde_json::from_slice::<CandidateEvent>(&bytes) else {
            continue;
        };
        if event.kind != "claim_propose" || event.status != "candidate" {
            continue;
        }
        let key = (event.claim_type.clone(), event.claim.trim().to_lowercase());
        let group = groups.entry(key).or_insert_with(|| CandidateGroup {
            claim_type: event.claim_type.clone(),
            claim: event.claim.clone(),
            observers: BTreeSet::new(),
            event_ids: Vec::new(),
            all_events_cite_evidence: true,
        });
        group.observers.insert(event.observed_by.clone());
        group.event_ids.push(event.id.clone());
        if event.evidence.is_empty() {
            group.all_events_cite_evidence = false;
        }
    }
    Ok(groups.into_values().collect())
}

/// Every agent id with a quarantine marker in the store.
pub async fn load_quarantined(ctx: &StoreCtx) -> Result<HashSet<String>> {
    let prefix = full_path(ctx, &claims_quarantine_prefix());
    let mut stream = ctx.store.list(Some(&prefix));
    let mut set = HashSet::new();
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        // `filename()` returns the raw path segment [`quarantine_key`] wrote —
        // still percent-encoded by `Path::join`'s `PathPart` machinery for any
        // `agent_id` that isn't already URL-safe (a `/`, a literal `%`, ...).
        // Comparing that encoded string against `observed_by` (plain text, never
        // encoded) would silently never match for exactly the agent ids most
        // likely to need quarantining — decode it back before stripping the
        // extension. `object_store::path::Path` exposes no public decode of its
        // own (see this crate's `Cargo.toml` for why `percent-encoding` is a
        // direct dependency here), so [`filename_decoded`] reverses `PathPart`'s
        // encoding itself, the same way `object_store` does internally.
        if let Some(name) = meta.location.filename() {
            if let Some(encoded_agent_id) = name.strip_suffix(".json") {
                if let Some(agent_id) = filename_decoded(encoded_agent_id) {
                    set.insert(agent_id);
                }
            }
        }
    }
    Ok(set)
}

/// Reverse the percent-encoding `object_store::path::Path::join` applies to a
/// dynamic segment (see [`load_quarantined`]'s doc for why this exists). Returns
/// `None` on invalid UTF-8 after decoding — that can only happen for a marker
/// this process never wrote (`quarantine` always encodes a valid `&str`), so
/// dropping it from the quarantine set is the fail-safe direction: a bogus
/// marker being *ignored* only means quarantine didn't apply, which is loud (the
/// next `--explain` run keeps promoting) rather than a corrupted id silently
/// matching the wrong agent.
fn filename_decoded(encoded: &str) -> Option<String> {
    percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()
        .map(|cow| cow.into_owned())
}

/// Which of docs/memory.md's four gates rejected a candidate, or `None` for
/// "quarantined" (a fifth, separate kill switch — see [`Explanation::gate`]'s
/// doc) or "passes every gate this evaluator can check."
///
/// `Contradiction` is never produced by [`explain`] today — there is no
/// promoted-claim contradiction index to check a candidate against yet, and that
/// gap is said plainly in [`explain`]'s doc rather than faked. `Independence`
/// *is* produced: docs/memory.md is explicit that "[t]hresholds read
/// `independent_count`, never `evidence_count`," and a `claim_propose` record
/// carries no `injected_context` lineage to compute that count from — so for the
/// one claim type whose promotion policy depends on it (`convention`), this
/// evaluator reports `Gate::Independence` as unresolved rather than letting a
/// raw observer count stand in for a property it cannot verify (see
/// [`explain`]'s doc for exactly why that substitution is the bug this gate
/// exists to not repeat). `#[allow(dead_code)]` on `Contradiction` only, rather
/// than silencing both via a wildcard match arm, which would hide from anyone
/// reading the enum which of the two is actually still unimplemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Evidence,
    #[allow(dead_code)]
    Contradiction,
    Provenance,
    Independence,
}

impl Gate {
    pub fn name(self) -> &'static str {
        match self {
            Gate::Evidence => "evidence",
            Gate::Contradiction => "contradiction",
            Gate::Provenance => "provenance",
            Gate::Independence => "independence",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Explanation {
    /// `true` only when every gate this evaluator can check passes AND the claim
    /// is not quarantined — see the module doc for what "can check" excludes.
    pub would_promote: bool,
    /// The quarantine kill switch, not one of the four gates — reported
    /// separately so an operator never mistakes "quarantined" for "failed the
    /// evidence gate," which would misdirect them toward gathering more evidence
    /// for a claim that can never promote no matter how much it gets.
    pub blocked_by_quarantine: bool,
    /// `None` when [`blocked_by_quarantine`](Self::blocked_by_quarantine) is true,
    /// or when the claim passes every gate this evaluator checks.
    pub gate: Option<Gate>,
    pub reason: String,
}

/// Evaluate one candidate group against docs/memory.md's rules, using only the
/// gate order the docs specify (evidence, then contradiction, then provenance,
/// then independence) and only the data a `claim_propose` record actually
/// carries.
///
/// **Independence is not a raw observer count, and this function must never
/// report it as passed without lineage data.** docs/memory.md: "[t]hresholds
/// read `independent_count`, never `evidence_count`" — `independent_count`
/// subtracts sessions that had a related claim *injected* into them, because two
/// agents agreeing is not corroboration when one of them is echoing what it was
/// just told. A `claim_propose` record carries no `injected_context` lineage, so
/// there is no way for this evaluator to tell "two agents independently noticed
/// this" apart from "agent B read agent A's claim via `memory_search` and
/// re-proposed it" — which is precisely the echo docs/memory.md's independence
/// gate exists to catch. So for `convention` (the one claim type whose
/// promotion depends on independence — `environment`/`outcome` promote on a
/// single observation regardless), clearing the raw 2-observer count is
/// necessary but never sufficient: [`Gate::Independence`] is returned as
/// unresolved rather than silently treated as passed. See the module doc for
/// which other checks are real and which are intentionally reported as
/// unresolvable.
pub fn explain(group: &CandidateGroup, quarantined: &HashSet<String>) -> Explanation {
    let observers: Vec<String> = group.observers.iter().cloned().collect();

    if !observers.is_empty() && observers.iter().all(|o| quarantined.contains(o)) {
        return Explanation {
            would_promote: false,
            blocked_by_quarantine: true,
            gate: None,
            reason: "every observer of this claim is quarantined — a quarantined \
                      agent's claims stop promoting regardless of gate outcome"
                .to_string(),
        };
    }

    // Gate 1, part A: "no evidence, no claim" (docs/memory.md). `memory_propose`
    // already refuses to queue a zero-citation record, but this evaluator reads
    // straight from `claims/events/` and must not trust that every record there
    // went through that check — see [`CandidateGroup::all_events_cite_evidence`].
    if !group.all_events_cite_evidence {
        return Explanation {
            would_promote: false,
            blocked_by_quarantine: false,
            gate: Some(Gate::Evidence),
            reason: "at least one observation backing this candidate cites no \
                      evidence — docs/memory.md's \"no evidence, no claim\" rule \
                      blocks promotion regardless of observer count"
                .to_string(),
        };
    }

    let live_observers: Vec<&String> = observers
        .iter()
        .filter(|o| !quarantined.contains(o.as_str()))
        .collect();

    // Gate 1, part B: the per-type observation-count threshold from docs/
    // memory.md's promotion table. For `convention` this checks only the raw
    // *count* of distinct observers — never call it "independent" here, that
    // word is reserved for the real check in Gate 4 below.
    let (passed, reason) = match group.claim_type.as_str() {
        "environment" | "outcome" => (
            true,
            "one observation is sufficient for this claim type".to_string(),
        ),
        "convention" => {
            if live_observers.len() >= 2 {
                (
                    true,
                    format!(
                        "{} distinct observer(s) meet the raw 2-observer count \
                          (independence is checked separately below)",
                        live_observers.len()
                    ),
                )
            } else {
                (
                    false,
                    format!(
                        "convention claims need 2 independent observations; only {} \
                          non-quarantined observer(s) so far",
                        live_observers.len()
                    ),
                )
            }
        }
        "preference" => (
            false,
            "preference claims require human approval, which has not been recorded".to_string(),
        ),
        "hypothesis" => (
            false,
            "hypotheses never auto-promote beyond agent scope".to_string(),
        ),
        other => (false, format!("unrecognized claim_type {other:?}")),
    };
    if !passed {
        return Explanation {
            would_promote: false,
            blocked_by_quarantine: false,
            gate: Some(Gate::Evidence),
            reason,
        };
    }

    // Gate 3: provenance. The one thing this evaluator can check locally without
    // a transcript to hash against is that every observation names a real,
    // non-empty agent.
    if live_observers.iter().any(|o| o.trim().is_empty()) {
        return Explanation {
            would_promote: false,
            blocked_by_quarantine: false,
            gate: Some(Gate::Provenance),
            reason: "an observation carries an empty observed_by — cannot attribute \
                      it to a real agent"
                .to_string(),
        };
    }

    // Gate 4: independence. Only `convention` claims need it (see this
    // function's own doc) — and this evaluator can never confirm it, because
    // confirming it needs `injected_context` lineage no `claim_propose` record
    // carries. Reported as an unresolved rejection, not a pass: docs/memory.md's
    // whole independence section exists to stop exactly the failure mode of
    // treating a raw observer count as if it proved independence.
    if group.claim_type == "convention" {
        return Explanation {
            would_promote: false,
            blocked_by_quarantine: false,
            gate: Some(Gate::Independence),
            reason: format!(
                "{} distinct observer(s) clear the raw count, but this evaluator \
                  has no injected_context lineage to confirm they are independent \
                  rather than one agent echoing another's claim back — \
                  ctxlake-maint's real gate must resolve this before it promotes \
                  (see docs/memory.md's independence section)",
                live_observers.len()
            ),
        };
    }

    // Gate 2 (contradiction) needs a promoted-claim contradiction index this
    // evaluator does not have. Said plainly rather than silently treated as
    // passed — see the module doc. (Gate 4 does not apply beyond this point:
    // every claim_type that reaches here promotes on observation count alone,
    // per docs/memory.md's table, so there is nothing left for independence to
    // gate.)
    Explanation {
        would_promote: true,
        blocked_by_quarantine: false,
        gate: None,
        reason: "passes every gate this evaluator can check locally; contradiction \
                  needs data only ctxlake-maint's real gate has (see docs/reference.md)"
            .to_string(),
    }
}

pub async fn run_claims(cfg: &Config, status: &str, explain_flag: bool) -> Result<()> {
    match status {
        "candidate" => print_candidates(cfg, explain_flag).await,
        // The snapshot first, the JSON mirror only as a fallback. On a lake with 117
        // promoted claims this command printed "no promoted claims yet", because it
        // read only `claims.json` — a mirror the maintenance chain stopped writing
        // once the snapshot gained a `claims` table. The same orphaned-reader shape as
        // `history.json`: every reference was a reader. The fallback stays for caches
        // written by an older ctxlake that has a mirror and no snapshot.
        "promoted" | "contested" => print_promoted(cfg, status),
        other => {
            anyhow::bail!("unknown --status {other:?} (expected candidate, contested, or promoted)")
        }
    }
}

async fn print_candidates(cfg: &Config, explain_flag: bool) -> Result<()> {
    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;
    let groups = load_candidates(&ctx).await?;
    let quarantined = load_quarantined(&ctx).await?;

    if groups.is_empty() {
        println!("no candidate claims yet");
        return Ok(());
    }

    for group in &groups {
        // `claim`/`observed_by` are free text another agent wrote — untrusted per
        // AGENTS.md's house rule, sanitized here at render time like every other
        // peer-authored field this crate prints (see `status.rs`'s identical
        // choice for roster tasks/paths).
        println!(
            "[{}] {}",
            sanitize(&group.claim_type),
            sanitize(&group.claim)
        );
        let observers: Vec<String> = group.observers.iter().map(|o| sanitize(o)).collect();
        println!("    observed by: {}", observers.join(", "));
        if explain_flag {
            let ex = explain(group, &quarantined);
            if ex.blocked_by_quarantine {
                println!("    -> BLOCKED (quarantine): {}", ex.reason);
            } else if let Some(gate) = ex.gate {
                println!("    -> rejected by the {} gate: {}", gate.name(), ex.reason);
            } else if ex.would_promote {
                println!("    -> would promote: {}", ex.reason);
            } else {
                println!("    -> {}", ex.reason);
            }
        }
    }
    Ok(())
}

/// `--status promoted` / `--status contested`, read from the published snapshot —
/// the same artifact the agent reads, so this cannot report a different fleet memory
/// than the one in use.
fn print_promoted(cfg: &Config, status: &str) -> Result<()> {
    let Some(conn) = ctxlake_mcp::snapshot::open(&ctxlake_core::paths::cache_root(), &cfg.fleet_id)
    else {
        return print_cached(&paths::cache_dir(&cfg.fleet_id), status);
    };
    let mut stmt = conn.prepare(
        "SELECT claim_id, claim, claim_type, subject, observed_by, updated_at, \
                independent_count, confidence, status \
         FROM claims WHERE status = ?1 ORDER BY claim_type, claim_id",
    )?;
    let rows: Vec<(String, ClaimRecord)> = stmt
        .query_map([status], |r| {
            Ok((
                r.get::<_, String>(0)?,
                ClaimRecord {
                    claim: r.get(1)?,
                    claim_type: r.get(2)?,
                    subject: r.get(3)?,
                    observed_by: r.get(4)?,
                    observed_at: r.get(5)?,
                    independent_count: r.get::<_, i64>(6)?.max(0) as u32,
                    confidence: r.get(7)?,
                    status: r.get(8)?,
                    sessions: Vec::new(),
                },
            ))
        })?
        .filter_map(Result::ok)
        .collect();
    if rows.is_empty() {
        println!("no {status} claims in this fleet's snapshot.");
        return Ok(());
    }
    println!("{} {status} claim(s)\n", rows.len());
    for (id, c) in &rows {
        // The id leads, because this listing is where grooming starts and `--retire`
        // needs an id. Without it the operator has to go and find one by hand.
        println!("{}", &id[..ID_DISPLAY_LEN.min(id.len())]);
        println!("{}\n", ctxlake_mcp::memory::render(c));
    }
    Ok(())
}

/// The pre-snapshot fallback: the `claims.json` mirror an older ctxlake wrote.
///
/// `cache_dir` is the caller's already-resolved `<cache_root>/<fleet_id>/`
/// (injected rather than derived internally from a `Config` so tests can point it
/// at a tempdir without mutating the process-wide `$CTXLAKE_CACHE_DIR` — see
/// `paths.rs`'s own module doc on why that mutation is a flakiness risk this
/// crate avoids everywhere else).
fn print_cached(cache_dir: &std::path::Path, status: &str) -> Result<()> {
    let path = cache_dir.join("claims.json");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            println!(
                "no {status} claims yet — the promotion gate (ctxlake maint) has not \
                 produced any (see docs/memory.md); nothing at {}",
                path.display()
            );
            return Ok(());
        }
    };
    let claims: Vec<ClaimRecord> =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let matching: Vec<&ClaimRecord> = claims.iter().filter(|c| c.status == status).collect();
    if matching.is_empty() {
        println!("no {status} claims");
        return Ok(());
    }
    for c in matching {
        println!("{}\n", ctxlake_mcp::memory::render(c));
    }
    Ok(())
}

/// `ctxlake quarantine <agent_id>` — the kill switch (docs/memory.md). Two real
/// effects: (1) writes the quarantine marker to the store, which [`explain`]
/// consults to stop a quarantined agent's candidates from ever showing
/// `would_promote: true`; (2) best-effort, moves that agent's already-`promoted`
/// entries in the *local* fleet cache mirror to `contested` — the only place a
/// promoted claim exists at all today (see the module doc). Neither effect
/// touches `claims/events/` or the spool: capture continues, on purpose (docs/
/// memory.md: "you want the record of the failure, not a gap where it used to
/// be").
pub async fn quarantine(cfg: &Config, agent_id: &str) -> Result<()> {
    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;
    let key = full_path(&ctx, &quarantine_key(agent_id));
    let now = ctx.clock.now().await?;
    let marker = serde_json::json!({
        "agent_id": agent_id,
        "quarantined_at": time::OffsetDateTime::from(now)
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
    });
    ctx.store
        .put(&key, PutPayload::from(serde_json::to_vec(&marker)?))
        .await
        .with_context(|| format!("writing quarantine marker for {agent_id}"))?;

    let demoted = demote_cached_claims(&paths::cache_dir(&cfg.fleet_id), agent_id)?;

    println!(
        "quarantined {agent_id}: its candidates stop promoting; {demoted} previously \
         promoted claim(s) moved to contested in the local cache"
    );
    println!(
        "capture is unaffected — {agent_id}'s sessions and claim proposals keep \
         landing in the lake"
    );
    Ok(())
}

/// The local-cache half of [`quarantine`]. Returns how many entries it demoted —
/// `0`, without error, when there is no cache file yet (nothing to demote) or
/// nothing by this agent was promoted. See [`print_cached`]'s doc for why
/// `cache_dir` is a parameter rather than derived internally.
fn demote_cached_claims(cache_dir: &std::path::Path, agent_id: &str) -> Result<usize> {
    let path = cache_dir.join("claims.json");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Ok(0),
    };
    let mut claims: Vec<ClaimRecord> =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let mut demoted = 0;
    for c in claims.iter_mut() {
        if c.observed_by == agent_id && c.status == "promoted" {
            c.status = "contested".to_string();
            demoted += 1;
        }
    }
    if demoted > 0 {
        std::fs::write(&path, serde_json::to_vec_pretty(&claims)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(demoted)
}

/// How much of a claim id to print when the id is meant to be copied.
///
/// Not cosmetic. Claim ids are ULIDs, so every claim minted in the same millisecond
/// range shares a long prefix — eight characters matched 94 claims on a real lake,
/// which made `--duplicates` print ids that `--retire` could not resolve. Twelve
/// covers the full timestamp plus randomness.
pub const ID_DISPLAY_LEN: usize = 12;

/// How many candidates an ambiguous `--retire` prefix lists before it stops.
const AMBIGUOUS_PREVIEW: usize = 10;

/// `ctxlake claims --retire <claim_id> --reason "..."` — the manual grooming path.
///
/// **Why this is an append, not a delete.** Retiring writes a `Retired` event into
/// `claims/events/` alongside the `Promoted` event that put the claim there. The claim
/// stops being agent-visible at the next maintenance pass (only `promoted` claims are
/// ever read), but the record of having believed it — and of your reason for stopping —
/// survives. The lake has no deletes by design; a claim that vanished without trace
/// would be indistinguishable from one that was never made, which is precisely the
/// history an operator needs when the same wrong belief shows up again.
///
/// Takes a claim id or any unambiguous prefix, so the 8 characters
/// `--duplicates` prints are enough to act on.
pub async fn run_retire(cfg: &Config, id_prefix: &str, reason: &str) -> Result<()> {
    let Some(conn) = ctxlake_mcp::snapshot::open(&ctxlake_core::paths::cache_root(), &cfg.fleet_id)
    else {
        println!("no snapshot synced locally yet for this fleet — nothing to retire against.");
        return Ok(());
    };
    let mut stmt = conn.prepare(
        "SELECT claim_id, claim_type, claim, status FROM claims \
         WHERE claim_id LIKE ?1 || '%' ORDER BY claim_id",
    )?;
    let matches: Vec<ClaimRow> = stmt
        .query_map([id_prefix], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(Result::ok)
        .collect();

    // Ambiguity is reported, never resolved by picking the first — retiring the wrong
    // claim is silent, and the operator cannot tell from the output that it happened.
    match matches.len() {
        0 => {
            println!("no claim in this fleet's snapshot starts with {id_prefix:?}.");
            return Ok(());
        }
        1 => {}
        n => {
            // Showing a few is help; showing ninety is the same as showing none.
            println!("{n} claims start with {id_prefix:?} — give more characters:");
            for (id, ty, text, _) in matches.iter().take(AMBIGUOUS_PREVIEW) {
                println!(
                    "  {} [{}] {}",
                    &id[..ID_DISPLAY_LEN.min(id.len())],
                    ty,
                    text
                );
            }
            if n > AMBIGUOUS_PREVIEW {
                println!("  ... and {} more", n - AMBIGUOUS_PREVIEW);
            }
            return Ok(());
        }
    }
    let (claim_id, claim_type, claim_text, status) = &matches[0];
    if status == "retired" {
        println!(
            "{} is already retired.",
            &claim_id[..ID_DISPLAY_LEN.min(claim_id.len())]
        );
        return Ok(());
    }

    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;
    let now = time::OffsetDateTime::from(ctx.clock.now().await?);
    let at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let date = at.get(..10).unwrap_or_default().to_string();
    let store = store_ctx::prefixed_store(&ctx);
    ctxlake_maint::claims::append_retired(
        store.as_ref(),
        &date,
        &cfg.agent_id,
        claim_id,
        &sanitize(reason),
        &at,
    )
    .await
    .with_context(|| format!("appending Retired event for {claim_id}"))?;

    // The event is the truth, but the local cache is what this machine's next briefing
    // reads. Without this the operator retires a claim and keeps being told it.
    let dropped = drop_cached_claim(&paths::cache_dir(&cfg.fleet_id), claim_text)?;

    println!(
        "retired {} [{}]",
        &claim_id[..ID_DISPLAY_LEN.min(claim_id.len())],
        claim_type
    );
    println!("  {claim_text}");
    println!("  reason: {reason}");
    if dropped > 0 {
        println!("  dropped from this machine's cached briefing immediately");
    }
    println!(
        "\nFleet-wide it disappears at the next `ctxlake maint` pass, which folds the \
         event into the published snapshot."
    );
    Ok(())
}

/// Remove a retired claim from the local cache mirror. Matches on claim text because
/// the mirror carries no claim id (see [`ClaimRecord`]); exact match only, so a claim
/// that merely resembles it is left alone. Returns how many entries it removed.
fn drop_cached_claim(cache_dir: &std::path::Path, claim_text: &str) -> Result<usize> {
    let path = cache_dir.join("claims.json");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Ok(0),
    };
    let mut claims: Vec<ClaimRecord> =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let before = claims.len();
    claims.retain(|c| c.claim != claim_text);
    let dropped = before - claims.len();
    if dropped > 0 {
        std::fs::write(&path, serde_json::to_vec_pretty(&claims)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(dropped)
}

/// How much word overlap makes two claims worth a human look.
///
/// Low on purpose. This reports rather than acts, so a false positive costs one line
/// of output and a false negative hides a duplicate that nothing else will catch.
/// Measured on a real lake, the true duplicates sit between 35% and 73% and the true
/// distinctions sit in the same band — which is exactly why this cannot decide for you.
const DUPLICATE_REVIEW_THRESHOLD: f32 = 0.35;

/// One promoted claim as this report reads it: `(claim_id, claim_type, subject, claim)`.
type ClaimRow = (String, String, String, String);

/// Symmetric word overlap (Jaccard).
///
/// Deliberately not `gate::lexical_overlap`, which divides by the *first* claim's word
/// count and so scores a short claim inside a long one very differently depending on
/// which is passed first. For "are these the same thing" the measure has to be
/// symmetric, or the answer depends on iteration order.
fn word_overlap(a: &str, b: &str) -> f32 {
    let words = |s: &str| -> std::collections::HashSet<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect()
    };
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    wa.intersection(&wb).count() as f32 / wa.union(&wb).count() as f32
}

/// `ctxlake claims --duplicates` — promoted claims that may be saying the same thing.
pub async fn run_duplicates(cfg: &Config) -> Result<()> {
    // `snapshot::open` joins the fleet id itself, so it takes the cache ROOT. Passing
    // `paths::cache_dir` (which already ends in the fleet id) looked for
    // `<cache>/<fleet>/<fleet>/snapshot.bin` and reported "no snapshot synced yet"
    // against a lake that had one — the same shape of path bug `paths.rs`'s module doc
    // warns about, caught here only by running it.
    let Some(conn) = ctxlake_mcp::snapshot::open(&ctxlake_core::paths::cache_root(), &cfg.fleet_id)
    else {
        println!("no snapshot synced locally yet for this fleet.");
        return Ok(());
    };
    let mut stmt = conn.prepare(
        "SELECT claim_id, claim_type, subject, claim FROM claims \
         WHERE status = 'promoted' ORDER BY claim_id",
    )?;
    let rows: Vec<ClaimRow> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .filter_map(Result::ok)
        .collect();

    let mut pairs: Vec<(f32, &ClaimRow, &ClaimRow)> = Vec::new();
    for (i, a) in rows.iter().enumerate() {
        for b in rows.iter().skip(i + 1) {
            let o = word_overlap(&a.3, &b.3);
            if o >= DUPLICATE_REVIEW_THRESHOLD {
                pairs.push((o, a, b));
            }
        }
    }
    pairs.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));

    if pairs.is_empty() {
        println!(
            "no likely duplicates among {} promoted claim(s).",
            rows.len()
        );
        return Ok(());
    }
    println!(
        "{} possible duplicate pair(s) among {} promoted claims.\n\
         Reported, not merged: at this overlap a real duplicate and a real distinction \
         look the same,\nso the call is yours: retire one side with `ctxlake claims \
         --retire <id> --reason \"...\"`, or leave both.\n",
        pairs.len(),
        rows.len()
    );
    for (o, a, b) in pairs {
        let same_subject = if a.2 == b.2 { " · same subject" } else { "" };
        let same_type = if a.1 == b.1 {
            ""
        } else {
            " · DIFFERENT TYPES"
        };
        println!("  [{:.0}% overlap{same_subject}{same_type}]", o * 100.0);
        println!(
            "    {} [{}] {}",
            &a.0[..ID_DISPLAY_LEN.min(a.0.len())],
            a.1,
            a.3
        );
        println!(
            "    {} [{}] {}",
            &b.0[..ID_DISPLAY_LEN.min(b.0.len())],
            b.1,
            b.3
        );
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn word_overlap_is_symmetric() {
        // `gate::lexical_overlap` divides by the first claim's word count, so a short
        // claim inside a long one scores very differently depending on argument order.
        // For "are these the same thing" that makes the answer depend on iteration
        // order, which is not an answer.
        let a = "the theme stylesheet must be byte-identical across both repos";
        let b = "stylesheet must be byte-identical";
        assert!((super::word_overlap(a, b) - super::word_overlap(b, a)).abs() < f32::EPSILON);
    }

    /// Grooming matches on claim text because the local mirror carries no id. Exact
    /// match only: a claim that merely *resembles* the retired one is a different
    /// belief, and silently dropping it would be the worst possible failure here.
    #[test]
    fn dropping_a_retired_claim_from_the_cache_matches_exactly_and_nothing_near_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claims.json");
        let rec = |claim: &str| {
            serde_json::json!({
                "claim": claim,
                "claim_type": "convention",
                "observed_by": "cc-01",
                "observed_at": "2026-09-09",
                "status": "promoted",
            })
        };
        std::fs::write(
            &path,
            serde_json::to_vec(&vec![
                rec("Dark theme is set via localStorage.setItem('oxidant.theme', 'dark')"),
                // Word-for-word a superset of the retired claim, and a *different*
                // belief — the qualifier is the whole content. A substring match
                // would take this one out too, silently.
                rec("Dark theme is set via localStorage.setItem('oxidant.theme', 'dark') only when no OS preference is set"),
            ])
            .unwrap(),
        )
        .unwrap();

        let dropped = drop_cached_claim(
            dir.path(),
            "Dark theme is set via localStorage.setItem('oxidant.theme', 'dark')",
        )
        .unwrap();
        assert_eq!(dropped, 1);

        let left: Vec<ClaimRecord> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(left.len(), 1);
        assert!(
            left[0].claim.contains("only when no OS preference"),
            "the near-duplicate survives: {}",
            left[0].claim
        );
    }

    /// No cache mirror is the normal state on a machine whose snapshot superseded it.
    /// That must be a quiet zero, not an error that aborts a retire whose event has
    /// already been written to the lake.
    #[test]
    fn dropping_from_a_cache_that_does_not_exist_is_zero_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(drop_cached_claim(dir.path(), "anything").unwrap(), 0);
    }

    /// Eight characters of a ULID matched 94 claims on a real lake, which made
    /// `--duplicates` print ids `--retire` could not resolve. This is the guard on
    /// the two staying in step.
    #[test]
    fn the_printed_id_is_long_enough_to_separate_claims_minted_together() {
        let a = "01M2C81811Q7W4XRZ80GCH1XD1";
        let b = "01M2C8181MT7RW4XRZ80GCH1XD";
        assert_ne!(
            &a[..ID_DISPLAY_LEN],
            &b[..ID_DISPLAY_LEN],
            "two ULIDs from the same millisecond must not print identically"
        );
    }

    #[test]
    fn the_duplicate_report_separates_nothing_it_cannot_separate() {
        // Verbatim from a live lake — not paraphrased, because the whole point is the
        // measured numbers, and an abbreviated version scores differently. The first
        // pair is two genuinely different TLS facts; the second is one stylesheet fact
        // stated twice. Any rule that merged the second would merge the first.
        let tls_a = "In manual mode, the TLS Secret is mounted read-only at \
                     /etc/oxidant-platform/tls; OXIDANT_PLATFORM_TLS_CERT points to \
                     tls.crt and OXIDANT_PLATFORM_TLS_KEY points to tls.key";
        let tls_b = "TLS certificate mounting for manual mode uses a kubernetes.io/tls \
                     Secret type with tls.crt and tls.key mounted read-only at \
                     /etc/oxidant-platform/tls";
        let css_a = "The shared theme stylesheet (site/.vitepress/theme/oxidant.css) \
                     must be byte-identical in both the Oxidant Platform (soapfish) and \
                     ctxlake repos";
        let css_b = "oxidantdata.css theme stylesheet must be byte-identical across \
                     ctxlake and Oxidant Platform docs repos to maintain unified brand";

        let distinct = super::word_overlap(tls_a, tls_b);
        let duplicate = super::word_overlap(css_a, css_b);

        assert!(
            distinct >= super::DUPLICATE_REVIEW_THRESHOLD
                && duplicate >= super::DUPLICATE_REVIEW_THRESHOLD,
            "both must surface for review: distinct={distinct} duplicate={duplicate}"
        );
        assert!(
            distinct >= duplicate,
            "the DISTINCT pair scores at least as high as the duplicate one \
             ({distinct} vs {duplicate}) — which is precisely why this reports instead \
             of merging. If that ever inverts durably, revisit automating it."
        );
    }

    #[test]
    fn identical_claims_score_one_and_unrelated_ones_score_low() {
        assert!((super::word_overlap("a b c", "a b c") - 1.0).abs() < f32::EPSILON);
        assert!(super::word_overlap("cargo test workspace", "nginx tls certificate") < 0.1);
        assert_eq!(super::word_overlap("", "anything"), 0.0);
    }
    use super::*;
    use ctxlake_store::clock::SystemClock;
    use std::sync::Arc;

    /// `fleet` is a parameter (not hardcoded) because `quarantine()`'s public
    /// signature (`cfg: &Config, agent_id: &str`) derives its own local cache
    /// directory from `cfg.fleet_id` via `paths::cache_dir`, which is **not**
    /// injectable the way `print_cached`/`demote_cached_claims` are (see their
    /// own doc) — a test that exercises `quarantine`'s cache-mutating side needs a
    /// fleet id nothing else in this test binary (or a developer's real `~/
    /// .ctxlake/cache`) could plausibly already be using, rather than mutating
    /// `$CTXLAKE_CACHE_DIR` for the whole process, which every *other* test that
    /// reads it (`paths.rs`'s own, notably) would then be exposed to under
    /// `cargo test`'s default parallelism.
    fn cfg(dir: &std::path::Path, fleet: &str, agent: &str) -> Config {
        Config::new(format!("file://{}", dir.display()), fleet, agent)
    }

    async fn propose_candidate(
        ctx: &StoreCtx,
        date: &str,
        agent: &str,
        ulid: &str,
        claim: &str,
        claim_type: &str,
    ) {
        let key = full_path(ctx, &ctxlake_store::layout::claim_event(date, agent, ulid));
        let body = serde_json::json!({
            "kind": "claim_propose",
            "id": ulid,
            "fleet_id": "myteam",
            "claim": claim,
            "claim_type": claim_type,
            "status": "candidate",
            "observed_by": agent,
            "evidence": [{"session_id": "s1", "message_id": "m1"}],
            "evidence_count": 1,
        });
        ctx.store
            .put(&key, PutPayload::from(serde_json::to_vec(&body).unwrap()))
            .await
            .unwrap();
    }

    /// Writes a `claim_propose` record with an empty `evidence` array — the
    /// shape `memory_propose` itself refuses to queue (docs/memory.md's "no
    /// evidence, no claim"), reachable here only because this test writes
    /// directly to the store the way a hand-written record or a future/older
    /// writer might, bypassing that ingest-time check entirely.
    async fn propose_candidate_with_no_evidence(
        ctx: &StoreCtx,
        date: &str,
        agent: &str,
        ulid: &str,
        claim: &str,
        claim_type: &str,
    ) {
        let key = full_path(ctx, &ctxlake_store::layout::claim_event(date, agent, ulid));
        let body = serde_json::json!({
            "kind": "claim_propose",
            "id": ulid,
            "fleet_id": "myteam",
            "claim": claim,
            "claim_type": claim_type,
            "status": "candidate",
            "observed_by": agent,
            "evidence": [],
            "evidence_count": 0,
        });
        ctx.store
            .put(&key, PutPayload::from(serde_json::to_vec(&body).unwrap()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn load_candidates_groups_by_claim_type_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = store_ctx::connect(&cfg(dir.path(), "myteam", "cc-01"), "cc-01").unwrap();

        propose_candidate(
            &ctx,
            "2026-09-11",
            "cc-01",
            "01J0000000000000000000AAAA",
            "this repo uses just, not make",
            "convention",
        )
        .await;
        propose_candidate(
            &ctx,
            "2026-09-11",
            "cc-02",
            "01J0000000000000000000BBBB",
            "This Repo Uses Just, Not Make",
            "convention",
        )
        .await;

        let groups = load_candidates(&ctx).await.unwrap();
        assert_eq!(groups.len(), 1, "same claim text/type must fold together");
        assert_eq!(groups[0].observers.len(), 2);
        assert!(groups[0].observers.contains("cc-01"));
        assert!(groups[0].observers.contains("cc-02"));
    }

    /// The exact repro from the adversarial review: a `claim_propose` record
    /// written straight to the store (bypassing `memory_propose`'s own "no
    /// evidence, no claim" check) with `"evidence": []` must not read back as
    /// `would_promote: true`. Exercises the real `load_candidates` -> `explain`
    /// path end to end, not just the unit-constructed `CandidateGroup` in
    /// `explain_rejects_a_claim_where_any_event_cites_no_evidence` — this is the
    /// path an operator's `ctxlake claims --explain` actually runs.
    #[tokio::test]
    async fn load_candidates_and_explain_reject_a_zero_evidence_record_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = store_ctx::connect(&cfg(dir.path(), "myteam", "cc-01"), "cc-01").unwrap();

        propose_candidate_with_no_evidence(
            &ctx,
            "2026-09-11",
            "cc-01",
            "01J0000000000000000000EEEE",
            "staging listens on 2222",
            "environment",
        )
        .await;

        let groups = load_candidates(&ctx).await.unwrap();
        let group = groups
            .iter()
            .find(|g| g.claim.contains("2222"))
            .expect("the zero-evidence record must still be readable as a candidate");
        assert!(
            !group.all_events_cite_evidence,
            "a record with an empty evidence array must be flagged"
        );

        let ex = explain(group, &HashSet::new());
        assert_eq!(ex.gate, Some(Gate::Evidence), "{ex:?}");
        assert!(
            !ex.would_promote,
            "a zero-citation claim must never read as would_promote: true: {ex:?}"
        );
    }

    #[tokio::test]
    async fn quarantine_does_not_prevent_new_candidate_events_from_landing() {
        // Both halves of docs/memory.md's kill-switch contract in one test:
        // quarantine stops promotion (checked via `explain` below) AND capture
        // continues (checked by writing a fresh candidate for the quarantined
        // agent *after* quarantining it and confirming it still lands).
        let dir = tempfile::tempdir().unwrap();
        // A fleet id unique to this test — see `cfg`'s own doc for why: this test
        // calls `quarantine`, which touches the *real* ambient
        // `paths::cache_dir(fleet_id)` (best-effort; absent is fine, per
        // `demote_cached_claims`'s doc), and must not collide with anything else
        // that could be running concurrently.
        let cfg_a = cfg(dir.path(), "quarantine-capture-test-fleet", "cc-01");
        let ctx = store_ctx::connect(&cfg_a, "cc-01").unwrap();

        propose_candidate(
            &ctx,
            "2026-09-11",
            "cc-99",
            "01J0000000000000000000CCCC",
            "staging listens on 2222",
            "environment",
        )
        .await;

        quarantine(&cfg_a, "cc-99").await.unwrap();

        // Half 1: promotion stops. Re-fetch quarantine state and re-evaluate.
        let quarantined = load_quarantined(&ctx).await.unwrap();
        assert!(quarantined.contains("cc-99"));
        let groups = load_candidates(&ctx).await.unwrap();
        let group = groups
            .iter()
            .find(|g| g.claim.contains("2222"))
            .expect("the pre-quarantine candidate must still be readable");
        let ex = explain(group, &quarantined);
        assert!(ex.blocked_by_quarantine, "{ex:?}");
        assert!(!ex.would_promote, "{ex:?}");

        // Half 2: capture continues. A brand new event from the same
        // now-quarantined agent, written exactly the way `ctxlake sync` would
        // ship one, must still show up.
        propose_candidate(
            &ctx,
            "2026-09-11",
            "cc-99",
            "01J0000000000000000000DDDD",
            "a second observation after quarantine",
            "environment",
        )
        .await;
        let groups_after = load_candidates(&ctx).await.unwrap();
        assert!(
            groups_after
                .iter()
                .any(|g| g.claim.contains("second observation")),
            "capture must continue for a quarantined agent — the record of its \
             failure must not have a gap where it used to be"
        );
    }

    /// The adversarial review's repro: `quarantine_key` percent-encodes any
    /// `agent_id` `object_store::path::Path::join` considers unsafe (a literal
    /// `/`, a literal `%`), and until [`filename_decoded`] existed,
    /// [`load_quarantined`] read that encoded segment back verbatim — so the
    /// quarantine marker was written, `quarantine()` printed success, and
    /// `explain` kept comparing the *encoded* id against a plain-text
    /// `observed_by` that could never match. An agent id containing `/` is not
    /// exotic here: hook adapters and MCP client ids are free text (AGENTS.md's
    /// house rule that anything from a peer is untrusted), so this is exactly
    /// the kind of id the kill switch most needs to work for.
    #[tokio::test]
    async fn quarantine_stops_promotion_for_an_agent_id_needing_percent_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let evil_agent = "cc/with-a-slash";
        let cfg_a = cfg(dir.path(), "quarantine-encoding-test-fleet", "cc-01");
        let ctx = store_ctx::connect(&cfg_a, "cc-01").unwrap();

        propose_candidate(
            &ctx,
            "2026-09-11",
            evil_agent,
            "01J0000000000000000000FFFF",
            "a claim from an agent id that needs percent-encoding",
            "environment",
        )
        .await;

        quarantine(&cfg_a, evil_agent).await.unwrap();

        let quarantined = load_quarantined(&ctx).await.unwrap();
        assert!(
            quarantined.contains(evil_agent),
            "the decoded agent id must be recoverable from the quarantine \
             marker's filename, not left percent-encoded: {quarantined:?}"
        );

        let groups = load_candidates(&ctx).await.unwrap();
        let group = groups
            .iter()
            .find(|g| g.claim.contains("percent-encoding"))
            .expect("the pre-quarantine candidate must still be readable");
        let ex = explain(group, &quarantined);
        assert!(
            ex.blocked_by_quarantine,
            "an agent id needing percent-encoding must still be recognized as \
             quarantined: {ex:?}"
        );
        assert!(!ex.would_promote, "{ex:?}");
    }

    #[test]
    fn explain_rejects_hypothesis_at_the_evidence_gate() {
        let group = CandidateGroup {
            claim_type: "hypothesis".into(),
            claim: "the flake is a colima artifact".into(),
            observers: BTreeSet::from(["cc-01".to_string()]),
            event_ids: vec!["e1".into()],
            all_events_cite_evidence: true,
        };
        let ex = explain(&group, &HashSet::new());
        assert_eq!(ex.gate, Some(Gate::Evidence));
        assert!(!ex.blocked_by_quarantine);
        assert!(!ex.would_promote);
        assert!(ex.reason.contains("never auto-promote"));
    }

    #[test]
    fn explain_rejects_convention_with_only_one_observer_at_the_evidence_gate() {
        let group = CandidateGroup {
            claim_type: "convention".into(),
            claim: "use just, not make".into(),
            observers: BTreeSet::from(["cc-01".to_string()]),
            event_ids: vec!["e1".into()],
            all_events_cite_evidence: true,
        };
        let ex = explain(&group, &HashSet::new());
        assert_eq!(ex.gate, Some(Gate::Evidence));
        assert!(ex.reason.contains("2 independent observations"));
    }

    /// The echo scenario docs/memory.md's independence gate exists to catch:
    /// agent B reads agent A's candidate (or promoted claim) via `memory_search`
    /// and re-proposes the same convention, so the raw observer count clears the
    /// evidence gate's "2" threshold even though there is only one real
    /// observation with two reporters. A `claim_propose` record carries no
    /// `injected_context` lineage, so this evaluator has no way to tell that
    /// scenario apart from two truly independent observers — and docs/memory.md
    /// is explicit that a threshold must read `independent_count`, never
    /// `evidence_count`. The only correct behavior with no lineage data is to
    /// refuse to say "would promote," not to guess yes.
    ///
    /// This is deliberately the single most load-bearing test in this module: a
    /// version of it that passes whether or not the independence gate is wired
    /// up would prove nothing. If a future wave adds real `injected_context`
    /// lineage and only *then* rejects an echo, this exact test (an echo with no
    /// lineage data available at all) must still reject — "no data" and "data
    /// proving non-independence" are both not-yet-promotable, never promotable.
    #[test]
    fn explain_refuses_to_promote_a_convention_echo_with_no_lineage_data() {
        let group = CandidateGroup {
            claim_type: "convention".into(),
            claim: "use just, not make".into(),
            observers: BTreeSet::from(["cc-01".to_string(), "cc-02".to_string()]),
            event_ids: vec!["e1".into(), "e2".into()],
            all_events_cite_evidence: true,
        };
        let ex = explain(&group, &HashSet::new());
        assert_eq!(
            ex.gate,
            Some(Gate::Independence),
            "two observers clearing the raw count must be rejected at the \
             independence gate, not treated as a pass: {ex:?}"
        );
        assert!(
            !ex.would_promote,
            "would_promote must never be true without injected_context lineage \
             to confirm independence: {ex:?}"
        );
        // The reason must not describe the observers as "independent" — that is
        // exactly the word this evaluator is not entitled to use about a raw
        // count it cannot verify.
        assert!(
            !ex.reason.contains("independent observer"),
            "reason must not claim independence it cannot verify: {:?}",
            ex.reason
        );
    }

    #[test]
    fn explain_names_provenance_for_an_empty_observer() {
        let group = CandidateGroup {
            claim_type: "environment".into(),
            claim: "staging listens on 2222".into(),
            observers: BTreeSet::from(["".to_string()]),
            event_ids: vec!["e1".into()],
            all_events_cite_evidence: true,
        };
        let ex = explain(&group, &HashSet::new());
        assert_eq!(ex.gate, Some(Gate::Provenance));
    }

    #[test]
    fn explain_names_a_distinct_gate_per_rejection_reason() {
        // The load-bearing property from the task brief: every rejection names
        // WHICH gate, and the gate differs by reason rather than collapsing to one
        // generic "rejected."
        let hypothesis = CandidateGroup {
            claim_type: "hypothesis".into(),
            claim: "x".into(),
            observers: BTreeSet::from(["cc-01".to_string()]),
            event_ids: vec![],
            all_events_cite_evidence: true,
        };
        let convention_alone = CandidateGroup {
            claim_type: "convention".into(),
            claim: "y".into(),
            observers: BTreeSet::from(["cc-01".to_string()]),
            event_ids: vec![],
            all_events_cite_evidence: true,
        };
        let preference = CandidateGroup {
            claim_type: "preference".into(),
            claim: "z".into(),
            observers: BTreeSet::from(["cc-01".to_string()]),
            event_ids: vec![],
            all_events_cite_evidence: true,
        };
        let empty = HashSet::new();
        let a = explain(&hypothesis, &empty);
        let b = explain(&convention_alone, &empty);
        let c = explain(&preference, &empty);
        assert_eq!(a.gate, Some(Gate::Evidence));
        assert_eq!(b.gate, Some(Gate::Evidence));
        assert_eq!(c.gate, Some(Gate::Evidence));
        // Same gate, but the *reason* must be specific to each claim type — an
        // operator reading "evidence" alone with three identical reasons would
        // learn nothing about what to do differently for each.
        assert_ne!(a.reason, b.reason);
        assert_ne!(b.reason, c.reason);
        assert_ne!(a.reason, c.reason);
    }

    #[test]
    fn explain_rejects_a_claim_where_any_event_cites_no_evidence() {
        // docs/memory.md: "No evidence, no claim." `memory_propose` already
        // enforces this at ingest, but `explain` reads straight from
        // `claims/events/`, which a hand-written or future-writer record could
        // reach without going through that check — see
        // `CandidateGroup::all_events_cite_evidence`'s doc. Uses `environment`
        // (1-observation policy) specifically to prove this gate fires
        // independently of the per-type observer-count check, not as a side
        // effect of it.
        let group = CandidateGroup {
            claim_type: "environment".into(),
            claim: "staging listens on 2222".into(),
            observers: BTreeSet::from(["cc-01".to_string()]),
            event_ids: vec!["e1".into()],
            all_events_cite_evidence: false,
        };
        let ex = explain(&group, &HashSet::new());
        assert_eq!(ex.gate, Some(Gate::Evidence), "{ex:?}");
        assert!(!ex.would_promote, "{ex:?}");
        assert!(ex.reason.contains("no evidence"), "{:?}", ex.reason);
    }

    #[tokio::test]
    async fn quarantine_demotes_promoted_claims_in_the_local_cache_to_contested() {
        let dir = tempfile::tempdir().unwrap();
        // A fleet id unique to this test (see `cfg`'s doc): `quarantine` derives
        // its cache path from `cfg.fleet_id` via the real, ambient
        // `paths::cache_dir` — never mutate `$CTXLAKE_CACHE_DIR` for that, since
        // it's a single process-wide value every test in this crate that reads it
        // would then race.
        let cfg_a = cfg(dir.path(), "quarantine-demote-test-fleet", "cc-01");
        let cache_dir = paths::cache_dir(&cfg_a.fleet_id);
        std::fs::create_dir_all(&cache_dir).unwrap();
        let claims = vec![
            ClaimRecord {
                claim: "cargo test needs RUSTFLAGS".into(),
                claim_type: "convention".into(),
                subject: None,
                observed_by: "cc-99".into(),
                observed_at: "2026-09-09".into(),
                independent_count: 2,
                confidence: 0.9,
                status: "promoted".into(),
                sessions: vec![],
            },
            ClaimRecord {
                claim: "unrelated, from a different agent".into(),
                claim_type: "convention".into(),
                subject: None,
                observed_by: "cc-01".into(),
                observed_at: "2026-09-09".into(),
                independent_count: 2,
                confidence: 0.9,
                status: "promoted".into(),
                sessions: vec![],
            },
        ];
        std::fs::write(
            cache_dir.join("claims.json"),
            serde_json::to_vec(&claims).unwrap(),
        )
        .unwrap();

        quarantine(&cfg_a, "cc-99").await.unwrap();

        let text = std::fs::read_to_string(cache_dir.join("claims.json")).unwrap();
        let after: Vec<ClaimRecord> = serde_json::from_str(&text).unwrap();
        let cc99 = after.iter().find(|c| c.observed_by == "cc-99").unwrap();
        assert_eq!(cc99.status, "contested");
        let cc01 = after.iter().find(|c| c.observed_by == "cc-01").unwrap();
        assert_eq!(
            cc01.status, "promoted",
            "quarantining one agent must never touch another agent's claims"
        );

        std::fs::remove_dir_all(&cache_dir).ok();
    }

    #[test]
    fn quarantine_key_cannot_escape_its_directory() {
        // Mirrors `ctxlake_store::layout`'s own
        // `layout_segments_cannot_escape_their_directory` test — see this
        // module's doc for why this key isn't defined in `layout.rs` itself yet.
        let evil = "../../_meta/fleet";
        let key = quarantine_key(evil);
        assert!(
            key.as_ref().starts_with("claims/quarantine/"),
            "escaped its directory: {key}"
        );
        assert_eq!(
            key.as_ref().matches('/').count(),
            2,
            "an extra path separator survived encoding: {key}"
        );
    }

    #[test]
    fn print_cached_reports_absence_honestly_rather_than_fabricating() {
        let dir = tempfile::tempdir().unwrap();
        // No claims.json has ever been written under this (never-touched) dir.
        let result = print_cached(dir.path(), "promoted");
        assert!(result.is_ok());
    }

    #[test]
    fn clock_is_reachable_for_quarantine_marker_timestamps() {
        // Not a behavioral assertion — just pins that `SystemClock` (used by every
        // file:// store, per `store_ctx::connect`) implements `Clock` the way
        // `quarantine` expects, so a refactor of the trait surface fails a fast,
        // obvious test here rather than a confusing one inside an async command.
        let _clock: Arc<dyn ctxlake_store::clock::Clock> = Arc::new(SystemClock);
    }
}
