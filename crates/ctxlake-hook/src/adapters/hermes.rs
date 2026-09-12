//! Hermes adapter — shell hooks, a deliberately Claude Code-compatible wire
//! contract (`docs/runtimes.md § Hermes`, verified against Hermes's own source).
//!
//! This replaces the wave-1 in-process Python plugin (`adapters/hermes/`, now
//! deleted): a hand-ported second redactor with no shared source was exactly the
//! divergence risk AGENTS.md invariant 7 exists to rule out — the Python half could
//! stop catching a pattern and every Hermes session would leak it while Claude Code
//! sessions stayed clean, with nothing failing anywhere. Routing Hermes through this
//! one binary means Hermes gets the same redactor, the same spool writer, and the
//! same golden-fixture regression net as everyone else.
//!
//! ## The payload
//!
//! `{"hook_event_name": <event>, tool_name, tool_input, session_id, cwd, extra:
//! {...}}`. The first five are top-level and named identically to Claude Code's
//! (`docs/runtimes.md § Hermes`). **`result` and `duration_ms` are nested under
//! `extra`, never top-level** — this is the field the task brief calls out
//! explicitly, and for good reason: it is the exact silent-failure shape the
//! Cursor adapter's own double-encoded `tool_output` bug had (see `cursor.rs`'s
//! module doc). An adapter that reads `v.get("result")` here compiles, passes any
//! fixture that (wrongly) puts the value at the top level, and then records every
//! real Hermes tool call with no result and no duration. `tests/fixtures/hermes/`
//! and this file's own unit tests both include a fixture with a *decoy* top-level
//! value and the real one only under `extra`, specifically so a regression to
//! reading the top level fails loudly instead of merely under-testing.
//!
//! `session_id` falls back to `parent_session_id`, then is refused if still empty
//! (`docs/runtimes.md § Hermes`) — an event that can't be attributed to a session
//! pollutes every aggregate it lands in, so this adapter drops it rather than
//! spooling it under a placeholder.
//!
//! Hermes has no compaction event (`docs/runtimes.md § Hermes`'s "compaction gap").
//! `EventType::Compact` is simply never produced here — a documented gap, never a
//! synthesized event.
//!
//! `on_session_end`, `on_session_finalize`, and `on_session_reset` all collapse
//! onto `EventType::SessionEnd` (`docs/runtimes.md § Hermes`'s event-mapping table):
//! the schema has one session-boundary-at-the-end variant, not three, and Hermes's
//! own docs list all three as synonyms for it rather than distinct lifecycle
//! events.
//!
//! `pre_tool_call` is Hermes's one blocking event (`docs/runtimes.md § Hermes`'s
//! capabilities table: `{"action":"block", ...}` or `exit 2`). Like every other
//! adapter in this crate, [`response_for`] never blocks yet — a real block decision
//! needs the daemon's view of the lake (a briefing), which does not reach the hook
//! in this wave (see `claude_code::response_for`'s doc for the same reasoning
//! applied to Claude Code and Cursor).
//!
//! Content for `pre_llm_call`/`post_llm_call` (the prompt/assistant text) has no
//! field name given in the shell-hook contract this wave verified — unlike
//! `claude_code.rs`'s fields, nothing here was checked against a live payload
//! capture. Rather than guess a key that might not exist on the wire, `content`
//! stays absent for both events, the same choice `claude_code.rs` makes for `Stop`.
//! Fix this by diffing against a live Hermes shell-hook capture, the way
//! `cursor.rs` was corrected once real fixtures existed.

use ctxlake_core::envelope::{Envelope, EventType, ToolCall};
use ctxlake_core::redact::Redactor;
use ctxlake_core::{hash, Runtime};
use serde_json::Value;

use super::common::{
    get_str, get_stringified, scrub_field, truncate, withhold_if_denied_path, RedactionAcc,
};

const TOOL_EVENTS: &[&str] = &["pre_tool_call", "post_tool_call"];

/// See the module doc: `pre_tool_call` is Hermes's only blocking event, but this
/// wave never exercises that — every Hermes hook gets `{}`, the documented no-op
/// shape `docs/runtimes.md § Hermes`'s config section describes for a hook that
/// declares no opinion.
pub fn response_for(_event: &str) -> String {
    "{}".to_string()
}

pub fn normalize(event: &str, v: &Value) -> Result<Envelope, String> {
    let session_id = get_str(v, "session_id")
        .filter(|s| !s.is_empty())
        .or_else(|| get_str(v, "parent_session_id").filter(|s| !s.is_empty()))
        .ok_or("hermes payload missing session_id (and parent_session_id)")?
        .to_string();

    let event_type = match event {
        "on_session_start" => EventType::SessionStart,
        "pre_llm_call" => EventType::Prompt,
        "post_llm_call" => EventType::Assistant,
        "pre_tool_call" | "post_tool_call" => EventType::ToolCall,
        "on_session_end" | "on_session_finalize" | "on_session_reset" => EventType::SessionEnd,
        other => return Err(format!("unknown hermes event: {other}")),
    };

    let redactor = Redactor::new();
    let mut acc = RedactionAcc::default();

    let mut env = Envelope::new(
        crate::hostinfo::fleet_id(),
        crate::hostinfo::agent_id(),
        Runtime::Hermes,
        session_id,
        event_type,
        crate::clock::now_rfc3339(),
    );
    env.host_id = crate::hostinfo::host_id();
    env.cwd = get_str(v, "cwd").map(truncate);

    let extra = v.get("extra");

    // `platform` (e.g. "vscode", "cli") names the surface Hermes is embedded in;
    // there is no dedicated envelope slot for it, so `runtime_version` is the
    // closest fit — the same field the retired Python plugin mapped it onto.
    env.runtime_version = extra
        .and_then(|e| get_str(e, "platform"))
        .map(str::to_string);

    if event == "pre_llm_call" {
        env.role = Some("user".to_string());
        // See the module doc: no documented field for prompt text yet, so content
        // stays absent rather than a guess.
    }
    if event == "post_llm_call" {
        env.role = Some("assistant".to_string());
    }

    if TOOL_EVENTS.contains(&event) {
        env.tool = Some(build_tool_call(event, v, extra, &redactor, &mut acc));
        env.message_id = extra
            .and_then(|e| get_str(e, "tool_call_id"))
            .map(str::to_string);
        env.turn_id = extra
            .and_then(|e| get_str(e, "task_id"))
            .map(str::to_string);
    }

    env.redaction = acc.into_redaction();
    // Re-hash after redaction: `content_hash` must reflect what actually landed in
    // bronze, never the pre-redaction value — envelope.rs's dedup story depends on
    // this, the same reasoning `claude_code.rs` and `cursor.rs` both follow.
    env.content_hash = hash::content_hash(env.content.as_deref().unwrap_or(""));
    Ok(env)
}

fn build_tool_call(
    event: &str,
    v: &Value,
    extra: Option<&Value>,
    redactor: &Redactor,
    acc: &mut RedactionAcc,
) -> ToolCall {
    let name = get_str(v, "tool_name").unwrap_or("unknown").to_string();
    let file_path = v
        .get("tool_input")
        .and_then(|ti| ti.get("file_path").or_else(|| ti.get("path")))
        .and_then(Value::as_str)
        .map(str::to_string);

    let input = get_stringified(v, "tool_input").map(|s| truncate(&s));
    let input_hash = hash::content_hash(input.as_deref().unwrap_or(""));
    // See claude_code.rs's `build_tool_call` for why the denylist and entropy pass
    // both have to cover *input*: a write to a denylisted path carries the secret
    // there, not in the result, and skipping that direction is a silent bypass of
    // AGENTS.md invariant 7.
    let input = withhold_if_denied_path(redactor, acc, file_path.as_deref(), input);
    let input = scrub_field(redactor, acc, input, true);

    let mut tool = ToolCall {
        name,
        input,
        input_hash,
        ..Default::default()
    };

    if event == "post_tool_call" {
        // THE regression this adapter exists to pin (see the module doc): `result`
        // and `duration_ms` live under `extra`, never at the top level.
        let raw_result = extra
            .and_then(|e| get_stringified(e, "result"))
            .map(|s| truncate(&s));
        let raw_result = withhold_if_denied_path(redactor, acc, file_path.as_deref(), raw_result);
        tool.result = scrub_field(redactor, acc, raw_result, true);
        tool.duration_ms = extra
            .and_then(|e| e.get("duration_ms"))
            .and_then(Value::as_u64);
    }

    if let Some(p) = file_path {
        tool.paths = vec![p];
    }
    tool
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(json: serde_json::Value) -> Value {
        json
    }

    #[test]
    fn on_session_start_maps_fields() {
        let env = normalize(
            "on_session_start",
            &v(serde_json::json!({"session_id": "s1", "cwd": "/repo"})),
        )
        .unwrap();
        assert_eq!(env.event_type, EventType::SessionStart);
        assert_eq!(env.session_id, "s1");
        assert_eq!(env.cwd.as_deref(), Some("/repo"));
        assert_eq!(env.runtime, Runtime::Hermes);
        assert_eq!(env.redaction.status, "clean");
    }

    #[test]
    fn missing_session_id_is_an_error_not_a_panic() {
        let err =
            normalize("on_session_start", &v(serde_json::json!({"cwd": "/repo"}))).unwrap_err();
        assert!(err.contains("session_id"));
    }

    #[test]
    fn empty_session_id_falls_back_to_parent_session_id() {
        let env = normalize(
            "on_session_start",
            &v(serde_json::json!({"session_id": "", "parent_session_id": "parent-1"})),
        )
        .unwrap();
        assert_eq!(env.session_id, "parent-1");
    }

    #[test]
    fn empty_session_id_with_no_parent_is_refused_rather_than_spooled_under_a_placeholder() {
        let err = normalize(
            "on_session_start",
            &v(serde_json::json!({"session_id": "", "parent_session_id": ""})),
        )
        .unwrap_err();
        assert!(err.contains("session_id"));
    }

    #[test]
    fn pre_tool_call_maps_tool_input_and_ids_from_extra() {
        let env = normalize(
            "pre_tool_call",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_name": "terminal",
                "tool_input": {"command": "cargo test"},
                "extra": {"tool_call_id": "tc-1", "task_id": "task-1"}
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert_eq!(tool.name, "terminal");
        assert_eq!(tool.input.as_deref(), Some(r#"{"command":"cargo test"}"#));
        assert!(tool.result.is_none());
        assert!(tool.duration_ms.is_none());
        assert_eq!(env.message_id.as_deref(), Some("tc-1"));
        assert_eq!(env.turn_id.as_deref(), Some("task-1"));
    }

    /// THE regression test the task brief asks for by name: a payload carrying a
    /// wrong, decoy value for `result`/`duration_ms` at the top level and the real
    /// values only under `extra`. An adapter that (incorrectly) reads the top level
    /// would pick up the decoys here and fail this exact assertion — this is not
    /// merely "extra values are read" (which a same-valued fixture could pass by
    /// accident either way), it is "the top-level values, if read, would be visibly
    /// wrong," which is what actually pins the regression.
    #[test]
    fn post_tool_call_extracts_result_and_duration_ms_from_extra_not_top_level() {
        let env = normalize(
            "post_tool_call",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_name": "terminal",
                "tool_input": {"command": "cargo test"},
                // Decoys: a top-level-reading implementation would pick these up.
                "result": "WRONG: read from top level",
                "duration_ms": 999999,
                "extra": {
                    "result": "cargo test passed",
                    "duration_ms": 1089,
                    "tool_call_id": "tc-1"
                }
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert_eq!(tool.result.as_deref(), Some("cargo test passed"));
        assert_eq!(tool.duration_ms, Some(1089));
        assert_ne!(tool.result.as_deref(), Some("WRONG: read from top level"));
        assert_ne!(tool.duration_ms, Some(999999));
    }

    #[test]
    fn post_tool_call_with_no_extra_at_all_records_the_call_with_no_result_or_duration() {
        // Not every event necessarily carries `extra` (a minimal/malformed payload,
        // or a future Hermes version); this must degrade to "no result known," not
        // panic on a missing key.
        let env = normalize(
            "post_tool_call",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_name": "terminal",
                "tool_input": {"command": "ls"}
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert!(tool.result.is_none());
        assert!(tool.duration_ms.is_none());
    }

    #[test]
    fn platform_in_extra_maps_to_runtime_version() {
        let env = normalize(
            "on_session_start",
            &v(serde_json::json!({"session_id": "s1", "extra": {"platform": "vscode"}})),
        )
        .unwrap();
        assert_eq!(env.runtime_version.as_deref(), Some("vscode"));
    }

    #[test]
    fn session_end_finalize_and_reset_all_collapse_onto_session_end() {
        for event in ["on_session_end", "on_session_finalize", "on_session_reset"] {
            let env = normalize(event, &v(serde_json::json!({"session_id": "s1"}))).unwrap();
            assert_eq!(
                env.event_type,
                EventType::SessionEnd,
                "event {event} should map to SessionEnd"
            );
        }
    }

    #[test]
    fn pre_llm_call_and_post_llm_call_map_roles_without_inventing_content() {
        let prompt =
            normalize("pre_llm_call", &v(serde_json::json!({"session_id": "s1"}))).unwrap();
        assert_eq!(prompt.event_type, EventType::Prompt);
        assert_eq!(prompt.role.as_deref(), Some("user"));
        assert!(prompt.content.is_none());

        let assistant =
            normalize("post_llm_call", &v(serde_json::json!({"session_id": "s1"}))).unwrap();
        assert_eq!(assistant.event_type, EventType::Assistant);
        assert_eq!(assistant.role.as_deref(), Some("assistant"));
        assert!(assistant.content.is_none());
    }

    #[test]
    fn unknown_event_name_is_an_error_not_a_guess() {
        let err = normalize(
            "some_future_event",
            &v(serde_json::json!({"session_id": "s1"})),
        )
        .unwrap_err();
        assert!(err.contains("some_future_event"));
    }

    #[test]
    fn no_compaction_event_is_ever_synthesized() {
        // There is no "compact" string in Hermes's event vocabulary at all — the
        // absence itself is the guard: if some future edit adds a mapping to
        // `EventType::Compact` for any Hermes event name, it does so by editing the
        // match in `normalize`, which every other test above already exercises.
        // This test instead pins the specific documented gap: none of the event
        // names Hermes actually sends (`docs/runtimes.md § Hermes`'s event table)
        // produce one.
        for event in [
            "on_session_start",
            "pre_llm_call",
            "post_llm_call",
            "pre_tool_call",
            "post_tool_call",
            "on_session_end",
            "on_session_finalize",
            "on_session_reset",
        ] {
            let env = normalize(event, &v(serde_json::json!({"session_id": "s1"}))).unwrap();
            assert_ne!(
                env.event_type,
                EventType::Compact,
                "event {event} must not map to Compact"
            );
        }
    }

    #[test]
    fn a_leaked_secret_in_tool_output_is_quarantined_before_it_reaches_the_envelope() {
        let env = normalize(
            "post_tool_call",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_name": "terminal",
                "tool_input": {"command": "cat .env"},
                "extra": {"result": "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"}
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert!(!tool.result.as_deref().unwrap().contains("AKIA"));
        assert_eq!(env.redaction.status, "quarantined");
    }

    #[test]
    fn writing_a_secret_to_a_denied_path_does_not_leak_via_input() {
        let secret = "qV3kRt8zLmNp0XyW7bHfJ2sD4gUe6AcZ1oIl5TnB";
        let env = normalize(
            "post_tool_call",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_name": "write_file",
                "tool_input": {
                    "file_path": "/Users/dev/.aws/credentials",
                    "content": format!("[default]\naws_access_key={secret}")
                },
                "extra": {"result": "ok"}
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert_eq!(env.redaction.status, "quarantined");
        assert!(
            !tool.input.as_deref().unwrap().contains(secret),
            "the secret written to a denylisted path leaked through tool.input: {:?}",
            tool.input
        );
    }

    #[test]
    fn response_for_never_blocks_in_this_wave() {
        for event in [
            "pre_tool_call",
            "post_tool_call",
            "on_session_start",
            "on_session_end",
        ] {
            assert_eq!(response_for(event), "{}");
        }
    }
}
