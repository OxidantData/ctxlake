//! Build [`gate::run`]'s four context inputs from the lake itself, rather than
//! from a live roster or a hand-maintained list.
//!
//! `gate::run` needs to know, about the *whole* fleet's history, not just
//! whatever a single maintenance run happens to be extracting this time:
//!
//! - which agents have ever actually said anything (`known_agents`),
//! - when each session ran (`session_windows`),
//! - which claims were read into a session before it "observed" anything
//!   (`injected_context_by_session` — the independence gate's entire reason to
//!   exist), and
//! - whether a cited `(session_id, message_id)` is real (`excerpt_resolves`).
//!
//! All four are recomputable, cheap-relative-to-storage functions of the sealed
//! sessions' own envelopes — nothing here is state that must be maintained
//! incrementally or kept consistent across runs. [`build`] takes the same
//! [`extract::SessionTranscript`]s `run::run` already loaded to drive extraction
//! (or would load anyway even with extraction disabled, since claims can cite
//! any past session, not only ones freshly extracted this run) — see that
//! module's doc for why sharing one load matters.
//!
//! **This is the module that makes the independence gate real.** `gate::run`
//! only ever discounts a session that `injected_context_by_session` says had a
//! claim injected into it; if this module built that map wrong — empty, or
//! keyed by the wrong session — every echoed "observation" would silently count
//! as a second independent one. See
//! `injected_context_by_session_discounts_exactly_the_session_that_had_the_claim_injected`
//! below, which is the direct analogue of `gate`'s own echo-case test but at
//! this module's boundary: proving the *map*, not just what `gate` does with a
//! hand-built one.

use std::collections::{HashMap, HashSet};

use crate::claims::Evidence;
use crate::extract::SessionTranscript;

/// Everything [`gate::run`](crate::gate::run) needs about the lake, besides the
/// candidate claims themselves.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GateInputs {
    /// Every agent id ever seen on an envelope across every sealed session —
    /// "observed in the lake," not "currently live." An agent that has since
    /// gone quiet is still a known author of whatever it once observed.
    pub known_agents: HashSet<String>,
    /// `session_id -> (earliest emitted_at, latest emitted_at)` across that
    /// session's own envelopes.
    pub session_windows: HashMap<String, (String, String)>,
    /// `session_id -> claim_ids injected into that session's context` — read
    /// straight off each envelope's own `injected_context` (see
    /// `ctxlake_core::envelope::InjectedContext`), which is the only place this
    /// lineage is recorded and, per that type's own doc, cannot be
    /// reconstructed after the fact.
    pub injected_context_by_session: HashMap<String, HashSet<String>>,
    /// Every `(session_id, message_id)` pair that actually has an envelope
    /// behind it. Not `pub`: the only sanctioned way to consult it is
    /// [`GateInputs::excerpt_resolves`], so a caller can't accidentally reuse
    /// the raw set for something else and drift from what "resolves" means.
    resolvable: HashSet<(String, String)>,
}

impl GateInputs {
    /// Does this evidence citation name a `(session_id, message_id)` that
    /// really has an envelope behind it? A fabricated citation — one a model
    /// invented, or a candidate that cites the wrong session — resolves to
    /// `false` here regardless of what excerpt hash it claims.
    pub fn excerpt_resolves(&self, evidence: &Evidence) -> bool {
        self.resolvable
            .contains(&(evidence.session_id.clone(), evidence.message_id.clone()))
    }
}

/// Fold every sealed session's transcript into the four gate inputs in one
/// pass. Pure and synchronous on purpose — the only I/O (`extract::
/// list_sealed_sessions` + `extract::load_transcript`) happens once, in
/// `run::run`, before this is called.
pub fn build(transcripts: &[SessionTranscript]) -> GateInputs {
    let mut known_agents = HashSet::new();
    let mut session_windows = HashMap::new();
    let mut injected_context_by_session: HashMap<String, HashSet<String>> = HashMap::new();
    let mut resolvable = HashSet::new();

    for transcript in transcripts {
        let mut earliest: Option<&str> = None;
        let mut latest: Option<&str> = None;
        for envelope in &transcript.envelopes {
            known_agents.insert(envelope.agent_id.clone());

            let at = envelope.emitted_at.as_str();
            if earliest.is_none_or(|e| at < e) {
                earliest = Some(at);
            }
            if latest.is_none_or(|l| at > l) {
                latest = Some(at);
            }

            if let Some(message_id) = &envelope.message_id {
                resolvable.insert((envelope.session_id.clone(), message_id.clone()));
            }

            for injected in &envelope.injected_context {
                injected_context_by_session
                    .entry(envelope.session_id.clone())
                    .or_default()
                    .insert(injected.claim_id.clone());
            }
        }
        if let (Some(start), Some(end)) = (earliest, latest) {
            session_windows.insert(
                transcript.session_id.clone(),
                (start.to_string(), end.to_string()),
            );
        }
    }

    GateInputs {
        known_agents,
        session_windows,
        injected_context_by_session,
        resolvable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::Evidence;
    use ctxlake_core::envelope::InjectedContext;
    use ctxlake_core::{Envelope, EventType, Runtime};

    fn envelope(agent_id: &str, session_id: &str, message_id: &str, emitted_at: &str) -> Envelope {
        let mut e = Envelope::new(
            "oxidant",
            agent_id,
            Runtime::ClaudeCode,
            session_id,
            EventType::Assistant,
            emitted_at.to_string(),
        );
        e.message_id = Some(message_id.to_string());
        e.content = Some("hello".into());
        e
    }

    fn transcript(session_id: &str, agent_id: &str, envelopes: Vec<Envelope>) -> SessionTranscript {
        SessionTranscript {
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            envelopes,
        }
    }

    fn evidence(session_id: &str, message_id: &str) -> Evidence {
        Evidence {
            session_id: session_id.to_string(),
            message_id: message_id.to_string(),
            excerpt_hash: "irrelevant-to-this-check".to_string(),
            observed_at: "2026-09-09T12:00:00Z".to_string(),
        }
    }

    // ---- known_agents: from observed envelopes, not a live roster ----

    #[test]
    fn known_agents_includes_an_agent_seen_only_in_a_past_session() {
        let transcripts = vec![
            transcript(
                "s1",
                "cc-01",
                vec![envelope("cc-01", "s1", "m1", "2026-09-09T12:00:00Z")],
            ),
            transcript(
                "s2",
                "cc-departed",
                vec![envelope("cc-departed", "s2", "m1", "2026-08-01T00:00:00Z")],
            ),
        ];
        let inputs = build(&transcripts);
        assert!(inputs.known_agents.contains("cc-01"));
        assert!(
            inputs.known_agents.contains("cc-departed"),
            "an agent that has since gone away is still a known author of what \
             it observed, not just whoever is currently live"
        );
    }

    // ---- session_windows: earliest/latest emitted_at per session ----

    #[test]
    fn session_windows_spans_the_earliest_and_latest_envelope_in_that_session() {
        let transcripts = vec![transcript(
            "s1",
            "cc-01",
            vec![
                envelope("cc-01", "s1", "m2", "2026-09-09T12:30:00Z"),
                envelope("cc-01", "s1", "m1", "2026-09-09T12:00:00Z"),
                envelope("cc-01", "s1", "m3", "2026-09-09T13:00:00Z"),
            ],
        )];
        let inputs = build(&transcripts);
        let (start, end) = inputs.session_windows.get("s1").unwrap();
        assert_eq!(start, "2026-09-09T12:00:00Z");
        assert_eq!(end, "2026-09-09T13:00:00Z");
    }

    #[test]
    fn session_windows_keeps_two_sessions_separate() {
        let transcripts = vec![
            transcript(
                "s1",
                "cc-01",
                vec![envelope("cc-01", "s1", "m1", "2026-09-09T00:00:00Z")],
            ),
            transcript(
                "s2",
                "cc-01",
                vec![envelope("cc-01", "s2", "m1", "2026-09-12T00:00:00Z")],
            ),
        ];
        let inputs = build(&transcripts);
        assert_eq!(inputs.session_windows.len(), 2);
        assert_ne!(
            inputs.session_windows.get("s1"),
            inputs.session_windows.get("s2")
        );
    }

    // ---- THE ECHO CASE, at this module's own boundary ----

    #[test]
    fn injected_context_by_session_discounts_exactly_the_session_that_had_the_claim_injected() {
        // session-a: a plain envelope, nothing injected.
        // session-b: an envelope that had claim "cand-1" injected into its
        // context — B is about to "independently" re-observe it.
        let mut echoed = envelope("cc-02", "session-b", "m1", "2026-09-12T09:00:00Z");
        echoed.injected_context.push(InjectedContext {
            claim_id: "cand-1".to_string(),
            source: "peer".to_string(),
            observed_by: "cc-02".to_string(),
        });
        let transcripts = vec![
            transcript(
                "session-a",
                "cc-01",
                vec![envelope("cc-01", "session-a", "m1", "2026-09-09T12:00:00Z")],
            ),
            transcript("session-b", "cc-02", vec![echoed]),
        ];
        let inputs = build(&transcripts);

        assert!(
            !inputs.injected_context_by_session.contains_key("session-a"),
            "a session with nothing injected must not appear in the map at all"
        );
        assert_eq!(
            inputs.injected_context_by_session.get("session-b"),
            Some(&HashSet::from(["cand-1".to_string()]))
        );

        // And feeding this straight into the real independence-gate arithmetic
        // reproduces gate.rs's own echo-case result: one independent session,
        // not two, even though both sessions cite the claim.
        let evidence = vec![evidence("session-a", "m1"), evidence("session-b", "m1")];
        let count = crate::gate::compute_independent_count(
            "cand-1",
            &evidence,
            &inputs.injected_context_by_session,
        );
        assert_eq!(
            count, 1,
            "gate_inputs::build must produce a map that discounts the echoed \
             session, the same way gate.rs's own hand-built fixture does"
        );
    }

    #[test]
    fn two_genuinely_separate_sessions_are_not_discounted() {
        let transcripts = vec![
            transcript(
                "session-a",
                "cc-01",
                vec![envelope("cc-01", "session-a", "m1", "2026-09-09T12:00:00Z")],
            ),
            transcript(
                "session-b",
                "cc-02",
                vec![envelope("cc-02", "session-b", "m1", "2026-09-12T09:00:00Z")],
            ),
        ];
        let inputs = build(&transcripts);
        let evidence = vec![evidence("session-a", "m1"), evidence("session-b", "m1")];
        let count = crate::gate::compute_independent_count(
            "cand-1",
            &evidence,
            &inputs.injected_context_by_session,
        );
        assert_eq!(
            count, 2,
            "two sessions with nothing injected must both count as independent"
        );
    }

    // ---- excerpt_resolves: real citations pass, fabricated ones don't ----

    #[test]
    fn excerpt_resolves_is_true_for_a_real_citation() {
        let transcripts = vec![transcript(
            "s1",
            "cc-01",
            vec![envelope("cc-01", "s1", "m1", "2026-09-09T12:00:00Z")],
        )];
        let inputs = build(&transcripts);
        assert!(inputs.excerpt_resolves(&evidence("s1", "m1")));
    }

    #[test]
    fn excerpt_resolves_is_false_for_a_fabricated_message_id() {
        let transcripts = vec![transcript(
            "s1",
            "cc-01",
            vec![envelope("cc-01", "s1", "m1", "2026-09-09T12:00:00Z")],
        )];
        let inputs = build(&transcripts);
        assert!(
            !inputs.excerpt_resolves(&evidence("s1", "m-does-not-exist")),
            "a citation to a message_id absent from the session must not resolve"
        );
    }

    #[test]
    fn excerpt_resolves_is_false_for_a_real_message_id_under_the_wrong_session() {
        let transcripts = vec![
            transcript(
                "s1",
                "cc-01",
                vec![envelope("cc-01", "s1", "m1", "2026-09-09T12:00:00Z")],
            ),
            transcript(
                "s2",
                "cc-01",
                vec![envelope("cc-01", "s2", "m2", "2026-09-09T12:00:00Z")],
            ),
        ];
        let inputs = build(&transcripts);
        assert!(
            !inputs.excerpt_resolves(&evidence("s1", "m2")),
            "m2 is real, but only under s2 — citing it under s1 must not resolve"
        );
    }

    #[test]
    fn excerpt_resolves_is_false_against_an_empty_lake() {
        let inputs = build(&[]);
        assert!(!inputs.excerpt_resolves(&evidence("s1", "m1")));
    }
}
