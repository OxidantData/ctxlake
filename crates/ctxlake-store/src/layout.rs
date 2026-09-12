//! Typed paths for the bucket layout — see `docs/layout.md`.
//!
//! Every key any part of ctxlake writes or reads is constructed here and nowhere
//! else. That is not a style preference: a hand-rolled `format!("live/agents/{id}")`
//! elsewhere in the tree is how two call sites drift (a trailing slash, a missing
//! `.json`) and end up addressing two different objects that were meant to be one.
//!
//! Every dynamic segment (`agent_id`, `resource_hash`, `session_id`, ...) is appended
//! with [`Path::join`], never folded into a `format!` string that is then re-split on
//! `/`. `join` treats its argument as *one* path segment and percent-encodes anything
//! that would otherwise act as a separator or a traversal marker (`/`, `.`, `..`), so
//! a malformed or adversarial value (an agent id containing `../../secret`, say)
//! cannot escape the directory it was placed under. See
//! `layout_segments_cannot_escape_their_directory` below.

use ctxlake_core::Runtime;
use object_store::path::Path;

/// Fleet-wide configuration, written once by `ctxlake init`.
pub fn fleet_meta() -> Path {
    Path::from("_meta").join("fleet.json")
}

/// The `live/agents/` prefix, for listing (the roster's direct-list fallback and the
/// probe both need to enumerate every agent's intent without going through the
/// roster fan-in).
pub fn agents_prefix() -> Path {
    Path::from("live").join("agents")
}

/// One agent's live intent. Overwritten in place by that agent alone — see the
/// `intent` module doc for why this key needs no CAS.
pub fn agent_intent(agent_id: &str) -> Path {
    agents_prefix().join(format!("{agent_id}.json"))
}

/// The fan-in of every agent's intent, built by whoever holds
/// [`lease_maintenance`]. See the `roster` module doc.
pub fn roster() -> Path {
    Path::from("live").join("roster.json")
}

/// The `live/leases/` prefix.
pub fn leases_prefix() -> Path {
    Path::from("live").join("leases")
}

/// A lease keyed by resource hash (see `ctxlake_core::hash::resource_key`). The
/// object always exists once first touched; its *contents* say free or held — see
/// the `lease` module doc and AGENTS.md invariant 4.
pub fn lease(resource_hash: &str) -> Path {
    leases_prefix().join(format!("{resource_hash}.json"))
}

/// The lease arbitrating who builds [`roster`]. Not resource-hash-keyed — there is
/// exactly one of these per fleet — so it gets a fixed name instead of running
/// through [`lease`].
pub fn lease_maintenance() -> Path {
    leases_prefix().join("_maintenance")
}

/// The directory a sealed session's segments live under, without the segment
/// filename — shared by [`session_segment`] and [`session_sealed`] so the two can
/// never drift apart on the partition scheme.
fn session_dir(date: &str, fleet: &str, runtime: Runtime, agent: &str, session_id: &str) -> Path {
    Path::from("sessions")
        .join(format!("dt={date}"))
        .join(format!("fleet={fleet}"))
        .join(format!("runtime={}", runtime.as_str()))
        .join(format!("agent={agent}"))
        .join(format!("session={session_id}"))
}

/// One appended segment of a session's Parquet log. `seg` is zero-padded so a plain
/// lexicographic listing (what every backend's `list` gives you) is also numeric
/// order — compaction and any other reader that lists segments needs that to hold
/// without re-deriving a sort key from each segment's contents.
pub fn session_segment(
    date: &str,
    fleet: &str,
    runtime: Runtime,
    agent: &str,
    session_id: &str,
    seg: u32,
) -> Path {
    session_dir(date, fleet, runtime, agent, session_id).join(format!("seg-{seg:06}.parquet"))
}

/// The marker a session is done being appended to. Compaction must not touch a
/// session directory until this exists, or it can race the single writer still
/// appending segments to it.
pub fn session_sealed(
    date: &str,
    fleet: &str,
    runtime: Runtime,
    agent: &str,
    session_id: &str,
) -> Path {
    session_dir(date, fleet, runtime, agent, session_id).join("_SEALED")
}

/// One claim proposal (`memory_propose`, never `memory_write` — AGENTS.md invariant
/// 9). `ulid` is expected to already be a ULID string, which is why it is not itself
/// escaped further here beyond the standard segment encoding every dynamic value
/// gets.
pub fn claim_event(date: &str, agent: &str, ulid: &str) -> Path {
    Path::from("claims")
        .join("events")
        .join(format!("dt={date}"))
        .join(format!("agent={agent}"))
        .join(format!("{ulid}.json"))
}

/// Claims already extracted out of a given session, so re-running extraction is
/// idempotent instead of re-proposing the same claim twice.
pub fn claims_extracted(session_id: &str) -> Path {
    Path::from("claims").join("extracted").join(session_id)
}

/// A published, content-addressed snapshot. Immutable once written: the hash in the
/// key is the whole point — two processes computing the same snapshot write the same
/// key with the same bytes, so a "collision" here is a no-op, not a conflict.
pub fn snapshot(content_hash: &str) -> Path {
    Path::from("snapshot").join(format!("{content_hash}.sqlite"))
}

/// The CAS pointer to the current snapshot. This is the one object in `snapshot/`
/// that is ever overwritten, and it is overwritten via CAS — publish is
/// write-then-swap, never swap-then-write.
pub fn snapshot_latest() -> Path {
    Path::from("snapshot").join("latest.json")
}

/// Where withheld content lands — see `ctxlake_core::redact`.
pub fn quarantine_prefix() -> Path {
    Path::from("quarantine")
}

/// Paths below this line are internal to `ctxlake-store` itself: scratch objects
/// nothing outside this crate ever reads, kept out of the documented bucket layout
/// on purpose. They still go through `layout` rather than being hand-rolled inline,
/// for the same reason everything else here does.
pub(crate) mod internal {
    use object_store::path::Path;

    /// A per-caller scratch object touched only to read back the `last_modified`
    /// the store assigns to the write — see `clock::ObjectStoreClock`. Keyed by
    /// `caller_id` so two processes never write the same key (AGENTS.md invariant
    /// 3), even though this key's *contents* are never read by anyone.
    pub fn clock_probe(caller_id: &str) -> Path {
        Path::from("_meta")
            .join("clock")
            .join(format!("{caller_id}.probe"))
    }

    /// Scratch prefix the capability probe (`ctxlake doctor`) writes into and
    /// deletes when it is done. Never left behind on a clean run.
    ///
    /// Keyed by `caller_id` for the same reason [`clock_probe`] is (AGENTS.md
    /// invariant 3): two `ctxlake doctor` runs against the same bucket — a fleet
    /// where every agent doctors itself at startup, say — must not write each
    /// other's scratch objects out from under them mid-probe. An unkeyed prefix
    /// makes `put-if-absent` see a real conflict and `list` see the wrong count,
    /// both misreported as the *backend* lacking a primitive it actually has.
    pub fn probe_prefix(caller_id: &str) -> Path {
        Path::from("_meta").join("probe").join(caller_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_documented_path() {
        assert_eq!(fleet_meta().as_ref(), "_meta/fleet.json");
        assert_eq!(agent_intent("cc-01").as_ref(), "live/agents/cc-01.json");
        assert_eq!(roster().as_ref(), "live/roster.json");
        assert_eq!(lease("abc123").as_ref(), "live/leases/abc123.json");
        assert_eq!(lease_maintenance().as_ref(), "live/leases/_maintenance");
        assert_eq!(
            session_segment("2026-09-11", "oxidant", Runtime::ClaudeCode, "cc-01", "sess-1", 3)
                .as_ref(),
            "sessions/dt=2026-09-11/fleet=oxidant/runtime=claude_code/agent=cc-01/session=sess-1/seg-000003.parquet"
        );
        assert_eq!(
            session_sealed("2026-09-11", "oxidant", Runtime::ClaudeCode, "cc-01", "sess-1")
                .as_ref(),
            "sessions/dt=2026-09-11/fleet=oxidant/runtime=claude_code/agent=cc-01/session=sess-1/_SEALED"
        );
        assert_eq!(
            claim_event("2026-09-11", "cc-01", "01J000000000000000000000").as_ref(),
            "claims/events/dt=2026-09-11/agent=cc-01/01J000000000000000000000.json"
        );
        assert_eq!(
            claims_extracted("sess-1").as_ref(),
            "claims/extracted/sess-1"
        );
        assert_eq!(
            snapshot("deadbeef1234").as_ref(),
            "snapshot/deadbeef1234.sqlite"
        );
        assert_eq!(snapshot_latest().as_ref(), "snapshot/latest.json");
        assert_eq!(quarantine_prefix().as_ref(), "quarantine");
    }

    #[test]
    fn layout_segments_cannot_escape_their_directory() {
        // An agent id (or any other dynamic segment) is attacker- or bug-controlled
        // input from another process's config, not a literal we wrote. If it could
        // introduce extra path separators, "../../_meta/fleet.json" as an agent id
        // would let one agent's intent write clobber fleet config instead of landing
        // harmlessly under live/agents/.
        let evil = "../../_meta/fleet";
        let p = agent_intent(evil);
        assert!(
            p.as_ref().starts_with("live/agents/"),
            "escaped its directory: {p}"
        );
        // The literal characters ".." can still appear in the encoded segment
        // (percent-encoding a "/" doesn't touch the letters on either side of it)
        // — what matters is that they stayed inert *inside one segment* instead of
        // being interpreted as traversal. Exactly two "/" (between "live",
        // "agents", and the one encoded segment) proves no extra directory level
        // was introduced.
        assert_eq!(
            p.as_ref().matches('/').count(),
            2,
            "an extra path separator survived encoding: {p}"
        );

        let evil_hash = "a/b/../c";
        let lp = lease(evil_hash);
        assert!(
            lp.as_ref().starts_with("live/leases/"),
            "escaped its directory: {lp}"
        );
        assert_eq!(
            lp.as_ref().matches('/').count(),
            2,
            "an extra path separator survived encoding: {lp}"
        );
    }

    #[test]
    fn distinct_agents_never_collide() {
        // A naive concatenation without a separating join could make agent "ab" +
        // suffix "c" collide with agent "a" + suffix "bc". join() makes each
        // dynamic value its own percent-encoded segment, so this cannot happen even
        // before the ".json" suffix is considered.
        assert_ne!(agent_intent("ab"), agent_intent("a/b"));
    }

    #[test]
    fn internal_clock_probes_are_keyed_per_caller() {
        assert_ne!(
            internal::clock_probe("agent-a"),
            internal::clock_probe("agent-b")
        );
        assert!(internal::clock_probe("agent-a")
            .as_ref()
            .starts_with("_meta/clock/"));
    }

    #[test]
    fn internal_probe_prefixes_are_keyed_per_caller() {
        // Same requirement as clock_probe, for the same reason (AGENTS.md
        // invariant 3): two concurrent `ctxlake doctor` runs must not share a
        // scratch prefix, or each one's cleanup and setup collide with the
        // other's, misreporting a real backend as missing a CAS primitive.
        assert_ne!(
            internal::probe_prefix("doctor-run-a"),
            internal::probe_prefix("doctor-run-b")
        );
        assert!(internal::probe_prefix("doctor-run-a")
            .as_ref()
            .starts_with("_meta/probe/"));
    }
}
