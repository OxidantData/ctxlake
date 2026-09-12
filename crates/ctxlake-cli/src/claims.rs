//! `ctxlake claims` / `ctxlake quarantine` — the human review surface docs/
//! memory.md and docs/summarization.md promise:
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
//! second implementation of the gate. Two of the four gates (contradiction,
//! independence) need data this schema does not carry yet — `injected_context`
//! lineage, a promoted-claim contradiction index — and [`explain`] says so plainly
//! rather than guessing.
//!
//! **Where candidates live.** `memory_propose` (`ctxlake-mcp`) writes a
//! `claim_propose` record to the local spool; `ctxlake sync`'s upload loop ships
//! it into the object store under `claims/events/dt=.../agent=.../<ulid>.json`
//! (`ctxlake_store::layout::claim_event`). [`load_candidates`] lists and reads
//! that same prefix directly from the store — an operator-triggered, occasional
//! read, not a hook-path one, so AGENTS.md invariant 1 does not apply here any
//! more than it does to `ctxlake status`'s live lease listing.
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
/// that function's doc. Every other field on the wire is either absent from a
/// candidate (independence, contradiction data) or not yet meaningful (there is
/// no `subject` field on the wire today), which is exactly why [`explain`] is
/// explicit about which gates it cannot evaluate.
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
        });
        group.observers.insert(event.observed_by.clone());
        group.event_ids.push(event.id.clone());
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
        if let Some(name) = meta.location.filename() {
            if let Some(agent_id) = name.strip_suffix(".json") {
                set.insert(agent_id.to_string());
            }
        }
    }
    Ok(set)
}

/// Which of docs/memory.md's four gates rejected a candidate, or `None` for
/// "quarantined" (a fifth, separate kill switch — see [`Explanation::gate`]'s
/// doc) or "passes every gate this evaluator can check."
///
/// `Contradiction` and `Independence` are never produced by [`explain`] today —
/// see that function's doc for exactly why (no promoted-claim contradiction
/// index, no `injected_context` lineage in a `claim_propose` record) — but they
/// are real outcomes in docs/memory.md's model, kept here (rather than deleted)
/// so [`Gate::name`] and this module's callers already handle all four the day
/// `ctxlake-maint`'s real gate starts returning them. `#[allow(dead_code)]`
/// rather than silence via a wildcard match arm, which would hide exactly this
/// fact from anyone reading the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Evidence,
    #[allow(dead_code)]
    Contradiction,
    Provenance,
    #[allow(dead_code)]
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
/// then independence) and only the data a `claim_propose` record actually carries.
/// See the module doc for exactly which checks are real and which are
/// intentionally reported as unresolvable rather than faked.
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

    let live_observers: Vec<&String> = observers
        .iter()
        .filter(|o| !quarantined.contains(o.as_str()))
        .collect();

    // Gate 1: evidence, per docs/memory.md's promotion-threshold table.
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
                        "{} independent observer(s) meet the 2-observation threshold",
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

    // Gates 2 (contradiction) and 4 (independence) need data this evaluator does
    // not have: a promoted-claim contradiction index, and injected_context
    // lineage to compute true independence rather than a raw observer count. Said
    // plainly rather than silently treated as passed — see the module doc.
    Explanation {
        would_promote: true,
        blocked_by_quarantine: false,
        gate: None,
        reason: "passes every gate this evaluator can check locally; contradiction \
                  and independence need data only ctxlake-maint's real gate has \
                  (see docs/cli.md)"
            .to_string(),
    }
}

pub async fn run_claims(cfg: &Config, status: &str, explain_flag: bool) -> Result<()> {
    match status {
        "candidate" => print_candidates(cfg, explain_flag).await,
        "promoted" | "contested" => print_cached(&paths::cache_dir(&cfg.fleet_id), status),
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
        // choice for lease holders/reasons).
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
                 produced any (see docs/summarization.md); nothing at {}",
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

#[cfg(test)]
mod tests {
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

    #[test]
    fn explain_rejects_hypothesis_at_the_evidence_gate() {
        let group = CandidateGroup {
            claim_type: "hypothesis".into(),
            claim: "the flake is a colima artifact".into(),
            observers: BTreeSet::from(["cc-01".to_string()]),
            event_ids: vec!["e1".into()],
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
        };
        let ex = explain(&group, &HashSet::new());
        assert_eq!(ex.gate, Some(Gate::Evidence));
        assert!(ex.reason.contains("2 independent observations"));
    }

    #[test]
    fn explain_passes_convention_with_two_independent_observers() {
        let group = CandidateGroup {
            claim_type: "convention".into(),
            claim: "use just, not make".into(),
            observers: BTreeSet::from(["cc-01".to_string(), "cc-02".to_string()]),
            event_ids: vec!["e1".into(), "e2".into()],
        };
        let ex = explain(&group, &HashSet::new());
        assert_eq!(ex.gate, None);
        assert!(ex.would_promote);
    }

    #[test]
    fn explain_names_provenance_for_an_empty_observer() {
        let group = CandidateGroup {
            claim_type: "environment".into(),
            claim: "staging listens on 2222".into(),
            observers: BTreeSet::from(["".to_string()]),
            event_ids: vec!["e1".into()],
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
        };
        let convention_alone = CandidateGroup {
            claim_type: "convention".into(),
            claim: "y".into(),
            observers: BTreeSet::from(["cc-01".to_string()]),
            event_ids: vec![],
        };
        let preference = CandidateGroup {
            claim_type: "preference".into(),
            claim: "z".into(),
            observers: BTreeSet::from(["cc-01".to_string()]),
            event_ids: vec![],
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
