//! The Cursor adapter, asserted against payloads captured from a real `cursor-agent` run.
//!
//! `tests/fixtures/cursor/` holds hand-written fixtures. They are useful, but they were
//! authored from the same reading of the docs as the adapter itself, so they agree with
//! it by construction and cannot catch a wrong assumption. Four such assumptions were
//! wrong, and every one of them failed *silently* — no panic, no error, just a field
//! quietly absent from every Cursor envelope.
//!
//! `tests/fixtures/cursor-verified/` holds captured payloads instead. These assertions
//! exist to pin the specific things the capture corrected; see that directory's README.

use ctxlake_core::Runtime;
use ctxlake_hook::adapters;
use std::fs;
use std::path::PathBuf;

fn verified(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cursor-verified")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn envelope(event: &str, fixture: &str) -> ctxlake_core::Envelope {
    adapters::normalize(Runtime::Cursor, event, &verified(fixture))
        .unwrap_or_else(|e| panic!("{fixture} failed to normalize: {e}"))
}

#[test]
fn exit_code_is_read_from_the_double_encoded_tool_output() {
    // The bug that mattered most. `tool_output` is a JSON *string* containing an object,
    // so neither reading it as a string nor matching it as an object finds `exitCode`.
    // Friction detection — "abandoned after 4 failed `cargo test` runs" — is built
    // entirely on exit codes, so this failing meant the highest-value Tier 0 signal
    // worked on Claude Code and was blind on Cursor, with nothing to indicate it.
    let env = envelope("postToolUse", "post_tool_use.json");
    let tool = env.tool.expect("postToolUse must carry a tool");
    assert_eq!(
        tool.exit_code,
        Some(0),
        "exit code must be unwrapped from tool_output"
    );
}

#[test]
fn tool_result_is_the_output_text_not_the_json_wrapper() {
    let env = envelope("postToolUse", "post_tool_use.json");
    let tool = env.tool.expect("tool");
    let result = tool.result.expect("result");
    assert!(
        result.contains("2 data.txt"),
        "result should be the command's output, got: {result}"
    );
    assert!(
        !result.contains("exitCode"),
        "result must not still be the JSON envelope, got: {result}"
    );
}

#[test]
fn duration_survives_being_a_float() {
    // Cursor sends 1089.021. `Value::as_u64()` returns None for any non-integer, so
    // reading it that way dropped every duration. The hand-written fixture used the
    // integer 42, which is precisely how an invented fixture hides a real bug.
    let env = envelope("postToolUse", "post_tool_use.json");
    let tool = env.tool.expect("tool");
    assert_eq!(
        tool.duration_ms,
        Some(1089),
        "1089.021ms should round to 1089"
    );
}

#[test]
fn cwd_falls_back_to_workspace_roots_when_cursor_sends_it_empty() {
    // `cwd` is present in every payload and empty in all of them. Reading only `cwd`
    // left every Cursor event with no working directory, hence no repo attribution.
    let env = envelope("postToolUse", "post_tool_use.json");
    let cwd = env.cwd.expect("cwd must resolve from workspace_roots");
    assert!(!cwd.trim().is_empty(), "cwd must not be empty");
    assert!(
        cwd.contains("cursor-probe"),
        "cwd should come from workspace_roots[0], got: {cwd}"
    );
}

#[test]
fn operator_email_never_reaches_the_envelope() {
    // Every Cursor payload carries `user_email`. Stored verbatim it would publish each
    // operator's address to everyone else who can read the fleet's lake. There is no
    // envelope field for it and there should not be one.
    for (event, fixture) in [
        ("sessionStart", "session_start.json"),
        ("postToolUse", "post_tool_use.json"),
        ("sessionEnd", "session_end.json"),
    ] {
        let raw = verified(fixture);
        assert!(
            raw.contains("user_email"),
            "{fixture} should still contain user_email, or this test proves nothing"
        );
        let serialized = envelope(event, fixture).to_ndjson().unwrap();
        assert!(
            !serialized.contains("user_email") && !serialized.contains("@example.com"),
            "{fixture}: operator email leaked into the envelope"
        );
    }
}

#[test]
fn runtime_version_is_captured_from_cursor_version() {
    let env = envelope("sessionStart", "session_start.json");
    // Not load-bearing, but it is free information the payload already carries, and
    // knowing which client version produced an event matters when a contract moves.
    assert!(
        env.runtime_version.is_some(),
        "cursor_version should populate runtime_version"
    );
}

#[test]
fn shell_events_keep_their_own_output_and_duration_shape() {
    // afterShellExecution is NOT double-encoded — it carries `output` and `duration`
    // directly. A fix for postToolUse must not regress it.
    let env = envelope("afterShellExecution", "after_shell_execution.json");
    let tool = env.tool.expect("tool");
    let result = tool.result.expect("result");
    assert!(result.contains("2 data.txt"), "got: {result}");
    assert_eq!(tool.duration_ms, Some(1089));
}
