//! Typed paths for the bucket layout — see `docs/storage.md`.
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

/// Everything `live/` holds for one fleet.
///
/// **`live/` is partitioned by fleet, and that is load-bearing rather than tidy.**
/// These keys used to be flat — `live/agents/<agent_id>.json` and
/// `live/roster.json` — which had two consequences on a bucket holding more than one
/// fleet, both seen on a real lake:
///
/// - **The roster listed every fleet's agents.** `docs/getting-started.md` calls
///   `--fleet` "the boundary of who sees whom", and `ctxlake status` was reporting
///   "fleet oxidantdata-dev · 3 agent(s) active" with one of the three belonging to a
///   different fleet entirely. It reached the briefing too, so another fleet's agents
///   were being described into agents' context windows.
/// - **Two fleets sharing an `agent_id` shared a key.** `cc-01` in one fleet and
///   `cc-01` in another overwrote each other's presence — silent, and not a display
///   bug but a data one.
///
/// Scoping the prefix also makes the listing cheaper: the roster build enumerates one
/// fleet rather than the whole bucket, which is what `docs/storage.md`'s O(N) fan-in
/// arithmetic assumed all along.
fn fleet_live_prefix(fleet_id: &str) -> Path {
    Path::from("live").join("fleets").join(fleet_id)
}

/// The prefix listing one fleet's agent intents (the roster's direct-list fallback
/// and the probe both enumerate this without going through the roster fan-in).
pub fn agents_prefix(fleet_id: &str) -> Path {
    fleet_live_prefix(fleet_id).join("agents")
}

/// One agent's live intent. Overwritten in place by that agent alone — see the
/// `intent` module doc for why this key needs no CAS.
pub fn agent_intent(fleet_id: &str, agent_id: &str) -> Path {
    agents_prefix(fleet_id).join(format!("{agent_id}.json"))
}

/// The fan-in of one fleet's agent intents, published with a CAS write — see the
/// `roster` module doc for why any number of daemons may build this concurrently
/// rather than one elected builder.
pub fn roster(fleet_id: &str) -> Path {
    fleet_live_prefix(fleet_id).join("roster.json")
}

/// The pre-fleet-scoping locations, for `ctxlake maint --prune` to clean up.
///
/// Nothing writes these any more. They are named here rather than spelled out at the
/// call site so the one place that still knows the old layout is the file that owns
/// the layout.
pub fn legacy_agents_prefix() -> Path {
    Path::from("live").join("agents")
}

/// See [`legacy_agents_prefix`].
pub fn legacy_roster() -> Path {
    Path::from("live").join("roster.json")
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

/// The `claims/events/` prefix, for listing every event ever appended (the gate's
/// fold needs to read all of them, across every date and agent partition).
pub fn claims_events_prefix() -> Path {
    Path::from("claims").join("events")
}

/// The Tier 0 structural digest for one sealed session — see `docs/memory.md`.
/// Colocated under the same session directory as its segments and `_SEALED` marker
/// (rather than a separate top-level prefix) because it is derived from, and only
/// ever meaningful alongside, that one session's own data; a reader who has found
/// `_SEALED` already knows exactly where to look for the digest next to it.
///
/// Single-writer, like the rest of this directory: only `ctxlake maint`'s digest
/// step ever writes this key, and it is safe to overwrite in place (recomputing a
/// digest from the same sealed segments is pure arithmetic and always reproduces the
/// same bytes) — see `ctxlake_maint::digest`.
pub fn session_digest(
    date: &str,
    fleet: &str,
    runtime: Runtime,
    agent: &str,
    session_id: &str,
) -> Path {
    session_dir(date, fleet, runtime, agent, session_id).join("digest.json")
}

/// What the session transcript knows that the hook could not capture.
///
/// Written once at seal time by the agent's own daemon, because the transcript is a
/// local file that only that machine can read — and read back by `ctxlake maint` on any
/// host, which is why it lives in the lake beside the segments rather than staying on
/// disk.
///
/// It exists because the capture path cannot see this data at all. Tool results, exit
/// status, token usage, git branch and Bash-driven file edits are absent from every hook
/// payload; `docs/memory.md` promised all of them and the digest delivered duration
/// alone. A sibling object rather than a rewrite of the segments: `sessions/` is
/// append-only, and enrichment arriving later must not mean rewriting immutable bronze.
pub fn session_enrichment(
    date: &str,
    fleet: &str,
    runtime: Runtime,
    agent: &str,
    session_id: &str,
) -> Path {
    session_dir(date, fleet, runtime, agent, session_id).join("transcript.json")
}

/// One output file of a compaction run over a `(date, fleet)` partition — see
/// `ctxlake_maint::compact`. Deliberately a *sibling* prefix to `sessions/dt=.../`
/// rather than a rewrite of it in place: bronze (the per-session `seg-*.parquet`
/// files under [`session_segment`]) is immutable — compaction only ever *adds* a
/// derived, queryable artifact, it never deletes or overwrites the small files it
/// was built from. `part` is zero-padded for the same reason `session_segment`'s
/// `seg` is: plain lexicographic listing must already be numeric order.
///
/// `generation` (the compaction run's `sessions_hash` — the content hash of the
/// exact sealed-session set it folded in, stripped of its `sha256:` prefix so it's
/// a clean path segment) puts every run's output under its own directory, rather
/// than the fixed `part-000000.parquet`, `part-000001.parquet`, ... every prior
/// version of this function produced regardless of which run wrote them. A review
/// caught what that fixed naming actually meant: a second compaction run PUTs over
/// the *same* keys the first run's output — and any reader — is still using, with
/// no cross-key atomicity protecting a concurrent `LIST` across the two PUTs, so a
/// reader could see some parts from the old generation and some from the new one,
/// duplicated or missing rows, undetectable from the data. Two different sealed-
/// session sets now always land in two different `gen=` directories, so a
/// recompaction can never touch a byte a previous, still-being-read generation
/// wrote — it can only ever add a new one, the same "add, never rewrite" discipline
/// bronze itself already has. [`sessions_compaction_marker`] is the pointer that
/// says which `gen=` is current; a reader must follow it rather than listing this
/// prefix directly, the same way [`snapshot_latest`] is the pointer for
/// `snapshot/`. Old generations are never deleted (deleting one a lagging reader
/// might still be mid-read of would reintroduce the exact race this exists to
/// avoid), so a partition recompacted often accumulates old generations' storage —
/// a known, documented cost (`docs/storage.md`), not a silent one.
pub fn sessions_compacted_part(date: &str, fleet: &str, generation: &str, part: u32) -> Path {
    Path::from("sessions")
        .join("compacted")
        .join(format!("dt={date}"))
        .join(format!("fleet={fleet}"))
        .join(format!("gen={generation}"))
        .join(format!("part-{part:06}.parquet"))
}

/// Records exactly which sealed sessions a `(date, fleet)` compaction run folded in,
/// so a second run over an unchanged partition can recognize that and skip rewriting
/// — see `ctxlake_maint::compact`'s idempotency contract. Single-writer, CAS-free:
/// like [`roster`], any prior content this overwrites is itself fully disposable
/// (the marker is fully recomputed from a fresh listing every run, never
/// accumulated), so there is no "don't clobber a concurrent writer's progress" state
/// to protect with a CAS read-modify-write here.
pub fn sessions_compaction_marker(date: &str, fleet: &str) -> Path {
    Path::from("sessions")
        .join("compacted")
        .join(format!("dt={date}"))
        .join(format!("fleet={fleet}"))
        .join("_COMPACTED")
}

/// One claim proposal (`memory_propose`, never `memory_write` — AGENTS.md invariant
/// 9). `ulid` is expected to already be a ULID string, which is why it is not itself
/// escaped further here beyond the standard segment encoding every dynamic value
/// gets.
pub fn claim_event(date: &str, agent: &str, ulid: &str) -> Path {
    claims_events_prefix()
        .join(format!("dt={date}"))
        .join(format!("agent={agent}"))
        .join(format!("{ulid}.json"))
}

/// Claims already extracted out of a given session, so re-running extraction is
/// idempotent instead of re-proposing the same claim twice.
pub fn claims_extracted(fleet_id: &str, session_id: &str) -> Path {
    Path::from("claims")
        .join("extracted")
        .join(fleet_id)
        .join(session_id)
}

/// The pre-fleet-scoping marker prefix, for `ctxlake maint --prune` to clean up.
///
/// Scoped for the same reason the roster and the snapshot pointer were: two fleets
/// sharing a bucket shared these, so one fleet extracting a session id marked it done
/// for the other. Session ids are runtime-generated UUIDs, so a collision is unlikely
/// rather than impossible — but "unlikely" is not the guarantee `--fleet` advertises.
pub fn legacy_claims_extracted_prefix() -> Path {
    Path::from("claims").join("extracted")
}

/// The `claims/fleet/` prefix — promoted (and later contested/retired) claims,
/// one object per `claim_id`. Listed by the gate on every run to find the current
/// set of already-decided claims to check new candidates against.
pub fn claims_fleet_prefix() -> Path {
    Path::from("claims").join("fleet")
}

/// One claim's current folded state, once it has reached fleet scope. Written
/// **only** by the promotion gate inside `ctxlake maint` (AGENTS.md invariant 9) —
/// nothing else in this codebase constructs this path for a `put`.
///
/// Unlike `live/` this is a plain overwrite, not a CAS write: the content written
/// here is always a claim's current *folded* state (`ctxlake_maint::claims::fold`),
/// which is itself deterministic in the values that matter (status,
/// `independent_count`, `confidence`) given the same view of `claims/events/` —
/// two gate runs computing the same promotion write the same bytes, and
/// `claims::fold`'s handling of a repeated `Promoted` event (first promotion wins)
/// is what keeps this key well-defined even when two runs' views briefly disagree.
/// This key is fully re-derivable from the event log at any time, the same
/// "disposable, recomputed" property [`roster`] and [`sessions_compaction_marker`]
/// rely on for their own plain overwrites — see `docs/architecture.md`'s
/// maintenance chain.
pub fn claim_fleet(claim_id: &str) -> Path {
    claims_fleet_prefix().join(format!("{claim_id}.json"))
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
pub fn snapshot_latest(fleet_id: &str) -> Path {
    Path::from("snapshot")
        .join("fleets")
        .join(fleet_id)
        .join("latest.json")
}

/// The pre-fleet-scoping pointer, for `ctxlake maint --prune` to clean up.
///
/// Scoped once the snapshot started carrying session history: the blob is
/// content-addressed and shared harmlessly, but a single global pointer meant two
/// fleets publishing in turn each served the other's artifact half the time. That was
/// tolerable while the snapshot held only claims — which are still fleet-global, a
/// separate and older leak — and is not once it holds one fleet's sessions.
pub fn legacy_snapshot_latest() -> Path {
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
        assert_eq!(
            agent_intent("myteam", "cc-01").as_ref(),
            "live/fleets/myteam/agents/cc-01.json"
        );
        assert_eq!(roster("myteam").as_ref(), "live/fleets/myteam/roster.json");
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
        assert_eq!(claims_events_prefix().as_ref(), "claims/events");
        assert_eq!(
            claim_event("2026-09-11", "cc-01", "01J000000000000000000000").as_ref(),
            "claims/events/dt=2026-09-11/agent=cc-01/01J000000000000000000000.json"
        );
        assert_eq!(
            claims_extracted("myteam", "sess-1").as_ref(),
            "claims/extracted/myteam/sess-1"
        );
        assert_eq!(claims_fleet_prefix().as_ref(), "claims/fleet");
        assert_eq!(
            claim_fleet("01J000000000000000000000").as_ref(),
            "claims/fleet/01J000000000000000000000.json"
        );
        assert_eq!(
            snapshot("deadbeef1234").as_ref(),
            "snapshot/deadbeef1234.sqlite"
        );
        assert_eq!(
            snapshot_latest("myteam").as_ref(),
            "snapshot/fleets/myteam/latest.json"
        );
        assert_eq!(legacy_snapshot_latest().as_ref(), "snapshot/latest.json");
        assert_eq!(quarantine_prefix().as_ref(), "quarantine");
        assert_eq!(
            session_digest("2026-09-11", "oxidant", Runtime::ClaudeCode, "cc-01", "sess-1")
                .as_ref(),
            "sessions/dt=2026-09-11/fleet=oxidant/runtime=claude_code/agent=cc-01/session=sess-1/digest.json"
        );
        assert_eq!(
            sessions_compacted_part("2026-09-11", "oxidant", "deadbeef", 2).as_ref(),
            "sessions/compacted/dt=2026-09-11/fleet=oxidant/gen=deadbeef/part-000002.parquet"
        );
        assert_eq!(
            sessions_compaction_marker("2026-09-11", "oxidant").as_ref(),
            "sessions/compacted/dt=2026-09-11/fleet=oxidant/_COMPACTED"
        );
    }

    #[test]
    fn compacted_output_lives_outside_the_bronze_session_directory() {
        // Compaction must never be able to address a key inside a session's own
        // seg-*.parquet directory — that would make "compaction" and "the single
        // writer still appending this session" two writers on one key (AGENTS.md
        // invariant 3), instead of the sibling-prefix, add-only relationship the
        // module doc for `sessions_compacted_part` describes.
        let session_key = session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        let compacted_key = sessions_compacted_part("2026-09-11", "oxidant", "deadbeef", 0);
        assert!(
            !compacted_key.as_ref().starts_with("sessions/dt="),
            "compacted output must not land under the bronze dt= partition: {compacted_key}"
        );
        assert_ne!(session_key, compacted_key);
    }

    #[test]
    fn layout_segments_cannot_escape_their_directory() {
        // An agent id (or any other dynamic segment) is attacker- or bug-controlled
        // input from another process's config, not a literal we wrote. If it could
        // introduce extra path separators, "../../_meta/fleet.json" as an agent id
        // would let one agent's intent write clobber fleet config instead of landing
        // harmlessly under live/agents/.
        let evil = "../../_meta/fleet";
        let p = agent_intent("myteam", evil);
        assert!(
            p.as_ref().starts_with("live/fleets/myteam/agents/"),
            "escaped its directory: {p}"
        );

        // `fleet_id` became a dynamic path segment when `live/` was partitioned by
        // fleet, so it needs the same guarantee the agent id already had — and it
        // comes from the same place, another process's config file.
        let pf = agent_intent(evil, "cc-01");
        assert!(
            pf.as_ref().starts_with("live/fleets/"),
            "a fleet id escaped its directory: {pf}"
        );
        assert_eq!(
            pf.as_ref().matches('/').count(),
            4,
            "an extra path separator survived encoding: {pf}"
        );
        let pr = roster(evil);
        assert!(
            pr.as_ref().starts_with("live/fleets/"),
            "a fleet id escaped its directory: {pr}"
        );
        assert_eq!(
            pr.as_ref().matches('/').count(),
            3,
            "an extra path separator survived encoding: {pr}"
        );
        // The literal characters ".." can still appear in the encoded segment
        // (percent-encoding a "/" doesn't touch the letters on either side of it)
        // — what matters is that they stayed inert *inside one segment* instead of
        // being interpreted as traversal. Exactly two "/" (between "live",
        // "agents", and the one encoded segment) proves no extra directory level
        // was introduced.
        assert_eq!(
            p.as_ref().matches('/').count(),
            4,
            "an extra path separator survived encoding: {p}"
        );

        let evil_session = "a/b/../c";
        let cp = claims_extracted("myteam", evil_session);
        assert!(
            cp.as_ref().starts_with("claims/extracted/myteam/"),
            "escaped its directory: {cp}"
        );
        assert_eq!(
            cp.as_ref().matches('/').count(),
            3,
            "an extra path separator survived encoding: {cp}"
        );
    }

    #[test]
    fn enrichment_sits_beside_the_segments_it_enriches() {
        let d = session_dir("2026-09-12", "myteam", Runtime::ClaudeCode, "cc-01", "s1");
        assert_eq!(
            session_enrichment("2026-09-12", "myteam", Runtime::ClaudeCode, "cc-01", "s1"),
            d.join("transcript.json")
        );
        // Must not collide with the digest computed from it, nor with a segment.
        assert_ne!(
            session_enrichment("2026-09-12", "myteam", Runtime::ClaudeCode, "cc-01", "s1"),
            session_digest("2026-09-12", "myteam", Runtime::ClaudeCode, "cc-01", "s1")
        );
    }

    #[test]
    fn distinct_agents_never_collide() {
        // A naive concatenation without a separating join could make agent "ab" +
        // suffix "c" collide with agent "a" + suffix "bc". join() makes each
        // dynamic value its own percent-encoded segment, so this cannot happen even
        // before the ".json" suffix is considered.
        assert_ne!(agent_intent("f", "ab"), agent_intent("f", "a/b"));
        // And the same across the fleet segment, which is new: fleet "a" + agent
        // "b/c" must not land where fleet "a/b" + agent "c" does.
        assert_ne!(agent_intent("a", "b/c"), agent_intent("a/b", "c"));
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
