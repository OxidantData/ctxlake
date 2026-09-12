//! Cursor adapter.
//!
//! Event names and the response-shape buckets (`{"permission": ...}` for the gate
//! events, `{"continue": true}` for `beforeSubmitPrompt`, `{}` otherwise) are given in
//! the wave-1 task brief, checked against a live `~/.cursor/hooks.json` install.
//!
//! The per-event *stdin* field names below were **not** captured from a live payload
//! for this wave — only Claude Code's and Hermes' were. They come from Cursor's public
//! hooks reference (<https://cursor.com/docs/hooks>, fetched 2026-09-11) instead. Every
//! field is read defensively through `Option` (never a required key besides the
//! session identifier), so a wrong guess drops a field rather than panicking or
//! failing the hook — but this adapter's fixtures are best-effort, not a verified
//! contract the way `claude_code.rs`'s are. Diff it against a real payload capture
//! before relying on field-level fidelity, and update `tests/fixtures/cursor/` to match.

use ctxlake_core::envelope::{Envelope, EventType, ToolCall};
use ctxlake_core::redact::Redactor;
use ctxlake_core::{hash, Runtime};
use serde_json::Value;

use super::common::{
    get_str, get_stringified, scrub_field, truncate, withhold_if_denied_path, RedactionAcc,
};

const TOOL_EVENTS: &[&str] = &[
    "preToolUse",
    "postToolUse",
    "postToolUseFailure",
    "beforeShellExecution",
    "afterShellExecution",
    "beforeMCPExecution",
    "afterFileEdit",
    "beforeReadFile",
    "subagentStart",
    "subagentStop",
];

/// The "gate" events: Cursor accepts a decision from these before it proceeds. Wave 1
/// always allows — see `response_for`'s doc for why blocking is a later wave — but
/// still answers in the shape each event expects, since `{}` alone is not documented
/// as "allow" for a permission gate the way it is for Claude Code's hooks.
const PERMISSION_GATE_EVENTS: &[&str] = &[
    "preToolUse",
    "beforeShellExecution",
    "beforeMCPExecution",
    "beforeReadFile",
    "subagentStart",
];

/// See `claude_code::response_for`'s doc for why wave 1 always answers "proceed,
/// no opinion" rather than gating or injecting anything.
pub fn response_for(event: &str) -> String {
    if event == "beforeSubmitPrompt" {
        return r#"{"continue":true}"#.to_string();
    }
    if PERMISSION_GATE_EVENTS.contains(&event) {
        return r#"{"permission":"allow"}"#.to_string();
    }
    "{}".to_string()
}

pub fn normalize(event: &str, v: &Value) -> Result<Envelope, String> {
    let session_id = get_str(v, "session_id")
        .or_else(|| get_str(v, "conversation_id"))
        .ok_or("cursor payload missing session_id/conversation_id")?
        .to_string();

    let event_type = match event {
        "sessionStart" => EventType::SessionStart,
        "sessionEnd" => EventType::SessionEnd,
        "beforeSubmitPrompt" => EventType::Prompt,
        "preCompact" => EventType::Compact,
        // `stop` (the agent loop ended) and `afterAgentResponse` (the assistant
        // produced a message) both land on `Assistant` — the same choice as Claude
        // Code's `Stop`, for the same reason: no dedicated turn-boundary variant.
        "stop" | "afterAgentResponse" => EventType::Assistant,
        e if TOOL_EVENTS.contains(&e) => EventType::ToolCall,
        other => return Err(format!("unknown cursor event: {other}")),
    };

    let redactor = Redactor::new();
    let mut acc = RedactionAcc::default();

    let mut env = Envelope::new(
        crate::hostinfo::fleet_id(),
        crate::hostinfo::agent_id(),
        Runtime::Cursor,
        session_id,
        event_type,
        crate::clock::now_rfc3339(),
    );
    env.host_id = crate::hostinfo::host_id();
    env.cwd = get_str(v, "cwd").map(truncate);

    match event {
        "beforeSubmitPrompt" => {
            env.role = Some("user".to_string());
            env.content = scrub_field(
                &redactor,
                &mut acc,
                get_str(v, "prompt").map(str::to_string),
                false,
            );
        }
        "afterAgentResponse" => {
            env.role = Some("assistant".to_string());
            env.content = scrub_field(
                &redactor,
                &mut acc,
                get_str(v, "text").map(str::to_string),
                false,
            );
        }
        "stop" => {
            env.role = Some("assistant".to_string());
            // `status`/`loop_count` have no slot on `Assistant`-typed envelopes; left
            // out rather than misfiled into `content`.
        }
        _ => {}
    }

    if TOOL_EVENTS.contains(&event) {
        env.tool = Some(build_tool_call(event, v, &redactor, &mut acc));
        env.message_id = get_str(v, "tool_use_id")
            .or_else(|| get_str(v, "tool_call_id"))
            .map(str::to_string);
    }

    env.redaction = acc.into_redaction();
    env.content_hash = hash::content_hash(env.content.as_deref().unwrap_or(""));
    Ok(env)
}

/// Per-event (name, raw input field, raw result field, path-if-any) before
/// truncation/redaction/hashing, which is identical across events and lives here once.
fn build_tool_call(
    event: &str,
    v: &Value,
    redactor: &Redactor,
    acc: &mut RedactionAcc,
) -> ToolCall {
    let (name, input, result, path) = match event {
        "preToolUse" | "postToolUse" | "postToolUseFailure" => (
            get_str(v, "tool_name").unwrap_or("unknown").to_string(),
            get_stringified(v, "tool_input"),
            get_stringified(v, "tool_output")
                .or_else(|| get_str(v, "error_message").map(str::to_string)),
            v.get("tool_input")
                .and_then(|ti| ti.get("file_path"))
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        "beforeShellExecution" => (
            "shell".to_string(),
            get_str(v, "command").map(str::to_string),
            None,
            None,
        ),
        "afterShellExecution" => (
            "shell".to_string(),
            get_str(v, "command").map(str::to_string),
            get_str(v, "output").map(str::to_string),
            None,
        ),
        "beforeMCPExecution" => (
            get_str(v, "mcp_server_name")
                .map(|s| format!("mcp:{s}"))
                .unwrap_or_else(|| "mcp".to_string()),
            get_stringified(v, "tool_input"),
            None,
            None,
        ),
        "afterFileEdit" => (
            "edit".to_string(),
            get_stringified(v, "edits"),
            None,
            get_str(v, "file_path").map(str::to_string),
        ),
        "beforeReadFile" => (
            "read".to_string(),
            None,
            get_str(v, "content").map(str::to_string),
            get_str(v, "file_path").map(str::to_string),
        ),
        "subagentStart" => (
            get_str(v, "subagent_type")
                .map(|s| format!("subagent:{s}"))
                .unwrap_or_else(|| "subagent".to_string()),
            get_str(v, "task").map(str::to_string),
            None,
            None,
        ),
        "subagentStop" => (
            get_str(v, "subagent_type")
                .map(|s| format!("subagent:{s}"))
                .unwrap_or_else(|| "subagent".to_string()),
            get_str(v, "task").map(str::to_string),
            get_str(v, "summary").map(str::to_string),
            None,
        ),
        _ => ("unknown".to_string(), None, None, None),
    };

    let input = input.map(|s| truncate(&s));
    let input_hash = hash::content_hash(input.as_deref().unwrap_or(""));
    let input = scrub_field(redactor, acc, input, false);

    let result =
        withhold_if_denied_path(redactor, acc, path.as_deref(), result.map(|s| truncate(&s)));
    let result = scrub_field(redactor, acc, result, true);

    let mut tool = ToolCall {
        name,
        input,
        input_hash,
        result,
        ..Default::default()
    };
    tool.duration_ms = v
        .get("duration")
        .or_else(|| v.get("duration_ms"))
        .and_then(Value::as_u64);
    if let Some(p) = path {
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
    fn session_start_uses_session_id() {
        let env = normalize("sessionStart", &v(serde_json::json!({"session_id": "s1"}))).unwrap();
        assert_eq!(env.event_type, EventType::SessionStart);
        assert_eq!(env.session_id, "s1");
        assert_eq!(env.runtime, Runtime::Cursor);
    }

    #[test]
    fn falls_back_to_conversation_id_when_session_id_is_absent() {
        let env = normalize(
            "beforeSubmitPrompt",
            &v(serde_json::json!({"conversation_id": "c1", "prompt": "hi"})),
        )
        .unwrap();
        assert_eq!(env.session_id, "c1");
    }

    #[test]
    fn missing_both_ids_is_an_error_not_a_panic() {
        let err = normalize("sessionStart", &v(serde_json::json!({}))).unwrap_err();
        assert!(err.contains("session_id"));
    }

    #[test]
    fn before_submit_prompt_maps_content_and_role() {
        let env = normalize(
            "beforeSubmitPrompt",
            &v(serde_json::json!({"session_id": "s1", "prompt": "add a test"})),
        )
        .unwrap();
        assert_eq!(env.event_type, EventType::Prompt);
        assert_eq!(env.role.as_deref(), Some("user"));
        assert_eq!(env.content.as_deref(), Some("add a test"));
    }

    #[test]
    fn pre_tool_use_maps_tool_name_and_input() {
        let env = normalize(
            "preToolUse",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_use_id": "tu-1",
                "tool_name": "shell",
                "tool_input": {"command": "ls"}
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.input.as_deref(), Some(r#"{"command":"ls"}"#));
        assert_eq!(env.message_id.as_deref(), Some("tu-1"));
    }

    #[test]
    fn post_tool_use_maps_output_and_duration() {
        let env = normalize(
            "postToolUse",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_name": "shell",
                "tool_input": {"command": "ls"},
                "tool_output": "README.md",
                "duration": 42
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert_eq!(tool.result.as_deref(), Some("README.md"));
        assert_eq!(tool.duration_ms, Some(42));
    }

    #[test]
    fn before_read_file_of_a_denied_path_withholds_content() {
        let env = normalize(
            "beforeReadFile",
            &v(serde_json::json!({
                "session_id": "s1",
                "file_path": "/Users/x/.ssh/id_ed25519",
                "content": "-----BEGIN OPENSSH PRIVATE KEY-----"
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert!(tool.result.as_deref().unwrap().contains("withheld"));
        assert_eq!(tool.name, "read");
        assert_eq!(env.redaction.status, "quarantined");
    }

    #[test]
    fn before_read_file_of_a_safe_path_passes_content_through() {
        let env = normalize(
            "beforeReadFile",
            &v(serde_json::json!({"session_id": "s1", "file_path": "README.md", "content": "# ctxlake"})),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert_eq!(tool.result.as_deref(), Some("# ctxlake"));
        assert_eq!(tool.paths, vec!["README.md".to_string()]);
    }

    #[test]
    fn after_agent_response_maps_text() {
        let env = normalize(
            "afterAgentResponse",
            &v(serde_json::json!({"session_id": "s1", "text": "done"})),
        )
        .unwrap();
        assert_eq!(env.event_type, EventType::Assistant);
        assert_eq!(env.content.as_deref(), Some("done"));
    }

    #[test]
    fn a_leaked_secret_in_shell_output_is_quarantined() {
        let env = normalize(
            "afterShellExecution",
            &v(serde_json::json!({
                "session_id": "s1",
                "command": "cat .env",
                "output": "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert!(!tool.result.as_deref().unwrap().contains("AKIA"));
        assert_eq!(env.redaction.status, "quarantined");
    }

    #[test]
    fn response_for_covers_all_three_shapes() {
        assert_eq!(response_for("beforeSubmitPrompt"), r#"{"continue":true}"#);
        assert_eq!(response_for("preToolUse"), r#"{"permission":"allow"}"#);
        assert_eq!(
            response_for("beforeShellExecution"),
            r#"{"permission":"allow"}"#
        );
        assert_eq!(
            response_for("beforeMCPExecution"),
            r#"{"permission":"allow"}"#
        );
        assert_eq!(response_for("beforeReadFile"), r#"{"permission":"allow"}"#);
        assert_eq!(response_for("subagentStart"), r#"{"permission":"allow"}"#);
        assert_eq!(response_for("postToolUse"), "{}");
        assert_eq!(response_for("sessionEnd"), "{}");
    }

    #[test]
    fn unknown_event_name_is_an_error_not_a_guess() {
        let err = normalize(
            "someFutureEvent",
            &v(serde_json::json!({"session_id": "s1"})),
        )
        .unwrap_err();
        assert!(err.contains("someFutureEvent"));
    }
}
