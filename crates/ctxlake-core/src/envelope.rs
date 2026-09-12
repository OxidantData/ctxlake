//! The envelope — one schema for every runtime.
//!
//! This is the load-bearing artifact of the whole system. Claude Code, Cursor, and
//! Hermes each have different hook surfaces with different event names, payload shapes,
//! and transports; the adapters normalize all of them into this one type. Everything
//! downstream — compaction, digests, the belief layer — reads only this.
//!
//! Versioning rule: bump [`SCHEMA_VERSION`] and add fields. Never reinterpret an
//! existing field. Bronze is immutable, so a field that changed meaning is unreadable
//! history.

use serde::{Deserialize, Serialize};

/// Bumped when a field is added. Never bumped to reinterpret an existing field.
pub const SCHEMA_VERSION: u32 = 1;

use std::cell::RefCell;

thread_local! {
    /// A monotonic ULID generator per thread.
    ///
    /// Plain `Ulid::new()` is NOT monotonic: two ULIDs minted in the same
    /// millisecond differ only in their random bits, so they sort arbitrarily. That
    /// would make a session's own events interleave out of order — the one ordering
    /// property this design actually relies on.
    static EVENT_ID_GEN: RefCell<ulid::Generator> =
        const { RefCell::new(ulid::Generator::new()) };
}

/// A ULID that is strictly greater than the previous one from this thread.
///
/// Falls back to a random ULID if the generator's random bits would overflow, which
/// takes on the order of 2^80 ids inside a single millisecond. At that point the
/// next millisecond restores ordering on its own, so a fallback is better than a
/// panic on a path that runs inside the user's agent.
pub fn next_event_id() -> String {
    EVENT_ID_GEN
        .with(|g| {
            g.borrow_mut()
                .generate()
                .unwrap_or_else(|_| ulid::Ulid::new())
        })
        .to_string()
}

/// Which agent runtime produced this event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    ClaudeCode,
    Cursor,
    Hermes,
    /// An unrecognized runtime, carried through rather than dropped so a new
    /// adapter's events are still captured before this enum learns about it.
    Other,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::ClaudeCode => "claude_code",
            Runtime::Cursor => "cursor",
            Runtime::Hermes => "hermes",
            Runtime::Other => "other",
        }
    }
}

/// The normalized event kind. Runtime-specific event names map onto these; see
/// `docs/runtimes.md` for the per-runtime mapping tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    SessionStart,
    Prompt,
    Assistant,
    ToolCall,
    /// Context compaction is about to discard history. Claude Code and Cursor emit
    /// this; Hermes has no equivalent event, so Hermes sessions have a gap here.
    Compact,
    SessionEnd,
}

/// A tool invocation and its outcome.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub input_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Paths this call touched, when derivable. Feeds collision detection and
    /// `silver.artifacts` without a git walk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// Token and cost accounting.
///
/// Claude Code transcripts carry a `cost-state` record with real numbers, so this is
/// read rather than re-derived. Runtimes that do not report cost leave it `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// One claim that was injected into this session's context.
///
/// This is read-lineage, and it is why the independence gate works: if agent B's
/// session had agent A's claim injected into it, and B later asserts something
/// derived from it, that is one observation with two reporters — not corroboration.
///
/// It cannot be retrofitted. By the time you want it, the sessions are gone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectedContext {
    pub claim_id: String,
    pub source: String,
    pub observed_by: String,
}

/// What the redactor did to this event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Redaction {
    /// `clean` | `redacted` | `quarantined`
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules_fired: Vec<String>,
}

/// One captured event, normalized across runtimes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub schema_version: u32,
    /// ULID. Monotonic *within a writer process* (see [`next_event_id`]), so the
    /// events of one session are strictly ordered — which is the guarantee that
    /// matters here, because every key in the lake has exactly one writer.
    ///
    /// Across hosts, ordering is by millisecond and same-millisecond ties break
    /// arbitrarily. Bronze therefore sorts chronologically at millisecond
    /// resolution with no secondary index, and no finer than that.
    pub event_id: String,
    pub emitted_at: String,

    pub fleet_id: String,
    /// Logical identity, stable across restarts. Not a hostname.
    pub agent_id: String,
    pub runtime: Runtime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    /// Hashed, never the raw hostname.
    pub host_id: String,

    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_message_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    pub event_type: EventType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Hash of the post-redaction content. Gives idempotent replay and cross-runtime
    /// dedup — without it, the `AGENTS.md` read into every session dominates storage
    /// and skews every frequency count.
    pub content_hash: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub injected_context: Vec<InjectedContext>,

    pub redaction: Redaction,

    /// Set when this event came from `ctxlake import` rather than a live hook, so a
    /// backfilled session is never mistaken for a live one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub imported: bool,
}

impl Envelope {
    /// A minimal well-formed envelope. Adapters fill in the rest.
    pub fn new(
        fleet_id: impl Into<String>,
        agent_id: impl Into<String>,
        runtime: Runtime,
        session_id: impl Into<String>,
        event_type: EventType,
        emitted_at: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            event_id: next_event_id(),
            emitted_at: emitted_at.into(),
            fleet_id: fleet_id.into(),
            agent_id: agent_id.into(),
            runtime,
            runtime_version: None,
            host_id: String::new(),
            session_id: session_id.into(),
            turn_id: None,
            message_id: None,
            parent_message_id: None,
            repo: None,
            cwd: None,
            git_sha: None,
            branch: None,
            event_type,
            role: None,
            content: None,
            content_hash: crate::hash::content_hash(""),
            tool: None,
            usage: None,
            injected_context: Vec::new(),
            redaction: Redaction {
                status: "clean".into(),
                rules_fired: Vec::new(),
            },
            imported: false,
        }
    }

    /// Serialize as one NDJSON line (no trailing newline).
    pub fn to_ndjson(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Envelope {
        Envelope::new(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            "sess-1",
            EventType::ToolCall,
            "2026-09-11T18:22:03.114Z",
        )
    }

    #[test]
    fn roundtrips_through_json() {
        let e = sample();
        let s = e.to_ndjson().unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.event_id, e.event_id);
        assert_eq!(back.runtime, Runtime::ClaudeCode);
        assert_eq!(back.event_type, EventType::ToolCall);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn ndjson_line_has_no_embedded_newline() {
        // The spool is newline-delimited; an embedded newline would split one event
        // into two unparseable halves.
        let mut e = sample();
        e.content = Some("line one\nline two\r\nline three".into());
        let s = e.to_ndjson().unwrap();
        assert!(!s.contains('\n'), "serialized envelope must be one line");
    }

    #[test]
    fn event_ids_are_strictly_monotonic_within_a_process() {
        // A session's events are written by one process, and they must replay in the
        // order they happened. Plain Ulid::new() does not give this: ids minted in the
        // same millisecond differ only in random bits and sort arbitrarily, so this
        // test fails roughly half the time against it. 1000 iterations land many ids
        // inside one millisecond, which is exactly the case that has to hold.
        let ids: Vec<String> = (0..1000).map(|_| next_event_id()).collect();
        for w in ids.windows(2) {
            assert!(w[0] < w[1], "event ids must increase: {} !< {}", w[0], w[1]);
        }
    }

    #[test]
    fn event_ids_are_lexicographically_sortable() {
        // Bronze sorts by key with no secondary index, so byte order must equal
        // generation order — which needs the fixed-width Crockford base32 encoding,
        // not just increasing values.
        let mut ids: Vec<String> = (0..100).map(|_| next_event_id()).collect();
        let generated = ids.clone();
        ids.sort();
        assert_eq!(
            ids, generated,
            "lexicographic order must match generation order"
        );
        assert!(
            generated.iter().all(|i| i.len() == 26),
            "ULIDs are fixed-width"
        );
    }

    #[test]
    fn unknown_runtime_deserializes_rather_than_failing() {
        let json = serde_json::to_string(&Runtime::Other).unwrap();
        assert_eq!(json, "\"other\"");
    }

    #[test]
    fn empty_optionals_are_omitted_not_null() {
        // Bronze is append-only and large; null-padding every optional field across
        // millions of events is pure waste.
        let s = sample().to_ndjson().unwrap();
        assert!(!s.contains("\"tool\""), "None tool must be omitted: {s}");
        assert!(!s.contains("null"), "no nulls should be emitted: {s}");
    }
}
