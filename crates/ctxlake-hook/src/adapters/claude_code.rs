//! Claude Code adapter.
//!
//! Event names and stdin field names below (`session_id`, `transcript_path`, `cwd`,
//! `permission_mode`, `hook_event_name`, `tool_name`, `tool_input`, `tool_use_id`,
//! `tool_result`, `user_input`) come from a live `~/.claude/settings.json` install —
//! trust them over any docs, per the wave-1 task brief. The golden fixtures in
//! `tests/fixtures/claude_code/` are the regression net for this contract.

use ctxlake_core::envelope::{Envelope, EventType, ToolCall};
use ctxlake_core::redact::Redactor;
use ctxlake_core::{hash, Runtime};
use serde_json::Value;

use super::common::{
    get_str, get_stringified, scrub_field, truncate, withhold_if_denied_path, RedactionAcc,
};

/// Wave 1 never blocks (`PreToolUse`) or injects (`SessionStart`) — both need the
/// daemon's view of the lake (a briefing), which does not exist yet. `{}` means "no
/// opinion" in every one of Claude Code's hook response shapes, so it is a safe
/// default across all of them rather than one hand-picked shape per event.
///
/// `SessionEnd` gets the same `{}`: its budget is shared across *every* hook Claude
/// Code runs for that event (1.5s total, not per-hook), so nothing beyond the normal
/// capture-and-return path belongs there. See `normalize`'s `SessionEnd` handling —
/// it is exactly the same work as every other event, not less.
pub fn response_for(_event: &str) -> String {
    "{}".to_string()
}

pub fn normalize(event: &str, v: &Value) -> Result<Envelope, String> {
    let session_id = get_str(v, "session_id")
        .ok_or("claude_code payload missing session_id")?
        .to_string();

    let event_type = match event {
        "SessionStart" => EventType::SessionStart,
        "UserPromptSubmit" => EventType::Prompt,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => EventType::ToolCall,
        "PreCompact" => EventType::Compact,
        "SessionEnd" => EventType::SessionEnd,
        // `Stop` is "the assistant finished its turn" — the closest normalized
        // counterpart is an assistant message, since `EventType` has no dedicated
        // turn-boundary variant (see envelope.rs).
        "Stop" => EventType::Assistant,
        other => return Err(format!("unknown claude_code event: {other}")),
    };

    let redactor = Redactor::new();
    let mut acc = RedactionAcc::default();

    let mut env = Envelope::new(
        crate::hostinfo::fleet_id(),
        crate::hostinfo::agent_id(),
        Runtime::ClaudeCode,
        session_id,
        event_type,
        crate::clock::now_rfc3339(),
    );
    env.host_id = crate::hostinfo::host_id();
    env.cwd = get_str(v, "cwd").map(truncate);

    if event == "UserPromptSubmit" {
        env.role = Some("user".to_string());
        env.content = scrub_field(
            &redactor,
            &mut acc,
            // `prompt` is what Claude Code actually sends — captured from a live
            // session (tests/fixtures/claude-code-verified/user_prompt_submit.json).
            // The published hooks reference documents `user_input`, so both are read:
            // the docs and the binary disagree, and either could be right on a version
            // we have not seen. Reading only the documented name captured NO prompt
            // text at all, silently — every prompt event hashed the empty string.
            get_str(v, "prompt")
                .or_else(|| get_str(v, "user_input"))
                .map(str::to_string),
            false,
        );
    }

    if event == "Stop" {
        env.role = Some("assistant".to_string());
        // Claude Code's `Stop` payload carries `transcript_path`, not the message
        // text itself; opening and scanning that file here would risk the 5ms budget
        // on a long session, so `content` is intentionally left empty — see
        // docs/runtimes.md § Claude Code for the gap.
    }

    if matches!(event, "PreToolUse" | "PostToolUse" | "PostToolUseFailure") {
        env.tool = Some(build_tool_call(event, v, &redactor, &mut acc));
        // `tool_use_id` is the join key across Pre/Post for the same call; `message_id`
        // is the closest existing slot (see adapters/mod.rs's Envelope field notes).
        env.message_id = get_str(v, "tool_use_id").map(str::to_string);
    }

    env.redaction = acc.into_redaction();
    // Re-hash after redaction: `content_hash` must reflect what actually landed in
    // bronze, never the pre-redaction value (envelope.rs's dedup/idempotency story
    // depends on this).
    env.content_hash = hash::content_hash(env.content.as_deref().unwrap_or(""));

    Ok(env)
}

fn build_tool_call(
    event: &str,
    v: &Value,
    redactor: &Redactor,
    acc: &mut RedactionAcc,
) -> ToolCall {
    let name = get_str(v, "tool_name").unwrap_or("unknown").to_string();
    let file_path = v
        .get("tool_input")
        .and_then(|ti| ti.get("file_path"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let input = get_stringified(v, "tool_input").map(|s| truncate(&s));
    let input_hash = hash::content_hash(input.as_deref().unwrap_or(""));
    // A `Write`/`Edit` to a denylisted path carries the secret in *input*, not
    // result — `.aws/credentials` and `.env` are two of the most common places an
    // agent puts a key. The denylist must cover both directions of a call, or
    // writing a secret is a silent bypass of AGENTS.md invariant 7 (the status field
    // would say "quarantined" from the result-side check below while the input still
    // held the plaintext). Entropy is enabled here too — a write's payload is
    // arbitrary file content, not prose, so the same heuristic that catches a leaked
    // key in tool *output* applies to it.
    let input = withhold_if_denied_path(redactor, acc, file_path.as_deref(), input);
    let input = scrub_field(redactor, acc, input, true);

    let mut tool = ToolCall {
        name,
        input,
        input_hash,
        ..Default::default()
    };

    if matches!(event, "PostToolUse" | "PostToolUseFailure") {
        let raw_result = get_stringified(v, "tool_result").map(|s| truncate(&s));
        let raw_result = withhold_if_denied_path(redactor, acc, file_path.as_deref(), raw_result);
        tool.result = scrub_field(redactor, acc, raw_result, true);
        if event == "PostToolUseFailure" {
            // Claude Code's `PostToolUseFailure` does not hand us a numeric exit
            // code on this hook; `1` records "this call failed" without inventing a
            // code the runtime never actually sent.
            tool.exit_code = Some(1);
        }
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
    fn session_start_maps_fields() {
        let env = normalize(
            "SessionStart",
            &v(serde_json::json!({"session_id": "s1", "cwd": "/repo"})),
        )
        .unwrap();
        assert_eq!(env.event_type, EventType::SessionStart);
        assert_eq!(env.session_id, "s1");
        assert_eq!(env.cwd.as_deref(), Some("/repo"));
        assert_eq!(env.runtime, Runtime::ClaudeCode);
        assert_eq!(env.redaction.status, "clean");
    }

    #[test]
    fn missing_session_id_is_an_error_not_a_panic() {
        let err = normalize("SessionStart", &v(serde_json::json!({"cwd": "/repo"}))).unwrap_err();
        assert!(err.contains("session_id"));
    }

    #[test]
    fn user_prompt_submit_maps_content_and_role() {
        let env = normalize(
            "UserPromptSubmit",
            &v(serde_json::json!({"session_id": "s1", "user_input": "fix the bug"})),
        )
        .unwrap();
        assert_eq!(env.event_type, EventType::Prompt);
        assert_eq!(env.role.as_deref(), Some("user"));
        assert_eq!(env.content.as_deref(), Some("fix the bug"));
        assert_eq!(env.content_hash, hash::content_hash("fix the bug"));
    }

    #[test]
    fn pre_tool_use_maps_tool_input_and_hash() {
        let env = normalize(
            "PreToolUse",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_use_id": "tu-1",
                "tool_name": "Bash",
                "tool_input": {"command": "ls -la"}
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert_eq!(tool.name, "Bash");
        assert_eq!(tool.input.as_deref(), Some(r#"{"command":"ls -la"}"#));
        assert_eq!(
            tool.input_hash,
            hash::content_hash(r#"{"command":"ls -la"}"#)
        );
        assert!(tool.result.is_none());
        assert_eq!(env.message_id.as_deref(), Some("tu-1"));
    }

    #[test]
    fn post_tool_use_maps_result_and_paths() {
        let env = normalize(
            "PostToolUse",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_use_id": "tu-1",
                "tool_name": "Read",
                "tool_input": {"file_path": "/repo/README.md"},
                "tool_result": "# ctxlake"
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert_eq!(tool.result.as_deref(), Some("# ctxlake"));
        assert_eq!(tool.paths, vec!["/repo/README.md".to_string()]);
        assert!(tool.exit_code.is_none());
    }

    #[test]
    fn post_tool_use_failure_records_a_synthetic_exit_code() {
        let env = normalize(
            "PostToolUseFailure",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_use_id": "tu-1",
                "tool_name": "Bash",
                "tool_input": {"command": "false"},
                "tool_result": "command failed"
            })),
        )
        .unwrap();
        assert_eq!(env.tool.unwrap().exit_code, Some(1));
    }

    #[test]
    fn reading_a_denied_path_withholds_the_result_but_keeps_the_call() {
        let env = normalize(
            "PostToolUse",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_use_id": "tu-1",
                "tool_name": "Read",
                "tool_input": {"file_path": "/Users/x/.aws/credentials"},
                "tool_result": "[default]\naws_access_key_id=AKIAIOSFODNN7EXAMPLE"
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert!(tool.result.as_deref().unwrap().contains("withheld"));
        assert!(!tool.result.as_deref().unwrap().contains("AKIA"));
        assert_eq!(tool.name, "Read", "the call itself is still recorded");
        assert_eq!(env.redaction.status, "quarantined");
    }

    #[test]
    fn a_dotenv_read_is_withheld_even_when_nothing_in_it_looks_like_a_key() {
        // The golden fixture for this event happens to contain `AKIA...`, so it
        // passed even while `.env` was absent from the deny list — the marker layer
        // caught it by luck. Most `.env` files hold things like
        // `DATABASE_PASSWORD=hunter2`, which no marker matches and no entropy check
        // flags, and those were spooled in full.
        let v = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "s1",
            "cwd": "/home/alice/app",
            "tool_name": "Read",
            "tool_use_id": "t1",
            "tool_input": {"file_path": "/home/alice/app/.env"},
            "tool_result": "DATABASE_PASSWORD=hunter2\nFEATURE_FLAG=true"
        });
        let env = normalize("PostToolUse", &v).expect("adapter must accept this");
        let tool = env.tool.expect("a tool call");
        let result = tool.result.unwrap_or_default();
        assert!(
            !result.contains("hunter2"),
            "a .env read must never carry its contents to the spool: {result}"
        );
        assert!(
            env.redaction.rules_fired.iter().any(|r| r == "denied_path"),
            "it must be withheld by path, not by luck: {:?}",
            env.redaction.rules_fired
        );
    }

    #[test]
    fn a_leaked_secret_in_tool_output_is_quarantined_before_it_reaches_the_envelope() {
        let env = normalize(
            "PostToolUse",
            &v(serde_json::json!({
                "session_id": "s1",
                "tool_use_id": "tu-1",
                "tool_name": "Bash",
                "tool_input": {"command": "cat .env"},
                "tool_result": "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert!(!tool.result.as_deref().unwrap().contains("AKIA"));
        assert_eq!(env.redaction.status, "quarantined");
        assert!(env
            .redaction
            .rules_fired
            .contains(&"aws_access_key_id".to_string()));
    }

    #[test]
    fn writing_a_secret_to_a_denied_path_does_not_leak_via_input() {
        // Regression for the finding: the path denylist was applied only to *results*
        // (a read of `.aws/credentials`), never to *input* — so a `Write` to that same
        // path stored the secret verbatim in `tool.input` while `redaction.status` said
        // "quarantined" (fired only by the unrelated, secret-free `tool_result`). A
        // reader trusting that status to mean "the value was withheld" would render the
        // key straight out of bronze.
        let secret = "qV3kRt8zLmNp0XyW7bHfJ2sD4gUe6AcZ1oIl5TnB";
        let env = normalize(
            "PostToolUse",
            &v(serde_json::json!({
                "session_id": "w1",
                "tool_use_id": "tu-1",
                "tool_name": "Write",
                "tool_input": {
                    "file_path": "/Users/dev/.aws/credentials",
                    "content": format!("[default]\naws_access_key={secret}")
                },
                "tool_result": "File written successfully."
            })),
        )
        .unwrap();
        let tool = env.tool.unwrap();
        assert_eq!(
            env.redaction.status, "quarantined",
            "the path denylist should still fire"
        );
        assert!(
            !tool.input.as_deref().unwrap().contains(secret),
            "the secret written to a denylisted path leaked through tool.input: {:?}",
            tool.input
        );
    }

    #[test]
    fn pre_compact_and_session_end_and_stop_map_event_types() {
        let base = |extra: serde_json::Value| {
            let mut m = serde_json::json!({"session_id": "s1"});
            m.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            m
        };
        assert_eq!(
            normalize("PreCompact", &v(base(serde_json::json!({}))))
                .unwrap()
                .event_type,
            EventType::Compact
        );
        assert_eq!(
            normalize("SessionEnd", &v(base(serde_json::json!({}))))
                .unwrap()
                .event_type,
            EventType::SessionEnd
        );
        let stop = normalize("Stop", &v(base(serde_json::json!({})))).unwrap();
        assert_eq!(stop.event_type, EventType::Assistant);
        assert_eq!(stop.role.as_deref(), Some("assistant"));
    }

    #[test]
    fn unknown_event_name_is_an_error_not_a_guess() {
        let err = normalize(
            "SomeFutureEvent",
            &v(serde_json::json!({"session_id": "s1"})),
        )
        .unwrap_err();
        assert!(err.contains("SomeFutureEvent"));
    }
}
