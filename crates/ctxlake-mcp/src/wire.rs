//! The wire shape `memory_propose` spools — byte-for-byte the same JSON
//! `ctxlake_maint::claims::ClaimEvent::Proposed(ProposedClaim)` serializes to.
//!
//! **Mirrored, not imported — matching `ctxlake_maint::snapshot`'s own module
//! doc's reasoning, applied on the write side instead of the read side.** This
//! crate cannot depend on `ctxlake-maint` (it pulls `object_store` and `tokio`
//! transitively; see `snapshot.rs`'s module doc and AGENTS.md invariant 1). So the
//! types here are a second, independent definition of the same wire format —
//! `#[serde(tag = "kind", rename_all = "snake_case")]` producing exactly
//! `{"kind":"proposed","claim_id":...,...}`, no `Proposed` variant wrapper visible
//! in the JSON, because that is what a tuple-variant-of-a-struct internally-tagged
//! enum serializes to. Getting this wrong would mean a future daemon-side drain
//! (spooling `mcp/*.ndjson` into real `claims/events/*.json` objects — not yet
//! built anywhere in this codebase; see `spool.rs`'s module doc) either fails to
//! parse this crate's own output or, worse, parses it into something silently
//! different from what an agent actually proposed. `memory.rs`'s
//! `propose_record_matches_the_real_claim_event_wire_shape` test is what keeps the
//! two sides from drifting apart undetected.
//!
//! **Why this crate is even allowed to construct one of these at all**, despite
//! AGENTS.md invariant 9 ("agents propose; only the gate promotes"): a `Proposed`
//! event is not a promotion. It is the *input* to the gate, always landing at
//! [`Scope::Agent`] — this module has no `Promoted`/`Contested` counterpart and no
//! function that could ever produce one, so there is nothing here invariant 9 could
//! be violated by constructing.

use serde::Serialize;

/// Mirrors `ctxlake_maint::claims::ClaimType`'s five spellings exactly (both sides
/// are plain `#[serde(rename_all = "snake_case")]` unit-variant enums, so they
/// serialize identically) — kept as a plain string here rather than a re-declared
/// enum, the same choice `ctxlake_maint::snapshot::ProposedClaimRecord` makes for
/// its own `claim_type` field, and for the same reason: nothing is gained by a
/// second enum that could drift from the real one's variant list. Validity is
/// checked once, against [`super::memory::ALLOWED_CLAIM_TYPES`], before a value
/// ever reaches this struct.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireClaimEvent {
    Proposed(WireProposedClaim),
}

#[derive(Debug, Clone, Serialize)]
pub struct WireProposedClaim {
    pub claim_id: String,
    pub claim: String,
    pub claim_type: String,
    pub subject: String,
    /// Always `"agent"` — see this module's doc. There is no field or argument
    /// anywhere on the path from `tools.rs`'s dispatch to here that could set
    /// this to anything else.
    pub scope: &'static str,
    pub observed_by: String,
    pub observed_at: String,
    pub evidence: Vec<WireEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

/// Mirrors `ctxlake_maint::claims::Evidence` exactly. `excerpt_hash` and
/// `observed_at` are whatever the calling agent supplied — untrusted, and
/// deliberately never invented here. `ctxlake_maint::claims::Evidence`'s own doc is
/// explicit that a *real* `excerpt_hash` "is always computed by ctxlake from the
/// real transcript, never taken from the model's own claimed hash," which this
/// process has no access to (it never reads a session transcript — only the local
/// cache and whatever a tool call's own arguments contain). That verification is
/// exactly what `gate::check_provenance` exists to do once this event reaches the
/// gate; a citation whose `excerpt_hash` cannot be resolved against the real
/// session fails provenance there; it is not this crate's job to pre-verify a
/// transcript it does not have.
#[derive(Debug, Clone, Serialize)]
pub struct WireEvidence {
    pub session_id: String,
    pub message_id: String,
    pub excerpt_hash: String,
    pub observed_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_to_the_real_producers_internally_tagged_shape() {
        let event = WireClaimEvent::Proposed(WireProposedClaim {
            claim_id: "c1".into(),
            claim: "this repo uses just, not make".into(),
            claim_type: "convention".into(),
            subject: "tooling".into(),
            scope: "agent",
            observed_by: "cc-01".into(),
            observed_at: "2026-09-11T00:00:00Z".into(),
            evidence: vec![WireEvidence {
                session_id: "s1".into(),
                message_id: "m1".into(),
                excerpt_hash: "sha256:deadbeef".into(),
                observed_at: "2026-09-11T00:00:00Z".into(),
            }],
            embedding: None,
        });
        let v = serde_json::to_value(&event).unwrap();
        // No "Proposed" key anywhere — internally-tagged-enum-of-a-struct
        // flattens the variant's fields alongside `kind`, exactly like
        // `ClaimEvent::Proposed(ProposedClaim)` does on the real producer.
        assert_eq!(v["kind"], "proposed");
        assert_eq!(v["claim_id"], "c1");
        assert_eq!(v["scope"], "agent");
        assert_eq!(v["evidence"][0]["session_id"], "s1");
        assert!(v.get("Proposed").is_none());
        assert!(
            v.get("embedding").is_none(),
            "None must be omitted, not null"
        );
    }
}
