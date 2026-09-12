//! Golden fixture tests: one file per event per runtime, asserting the resulting
//! [`ctxlake_core::Envelope`] field by field. These fixtures are the regression net
//! for each runtime's contract — see `src/adapters/claude_code.rs` and
//! `src/adapters/cursor.rs` for where those field names come from.
//!
//! Each fixture is `{"event": <native event name>, "payload": <raw stdin JSON>,
//! "expect": {...}}`. `expect` is checked against the envelope re-serialized to JSON
//! as a **partial match**: every key `expect` names must be present and equal on the
//! actual envelope (recursively into nested objects, e.g. `"tool": {"name": "Bash"}`
//! checks only `tool.name` and ignores `tool.input_hash`), but keys the envelope has
//! that `expect` doesn't mention are never a mismatch — a fixture only asserts what it
//! cares about.

use ctxlake_core::Runtime;
use ctxlake_hook::adapters;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn fixtures_dir(runtime_dir: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(runtime_dir)
}

/// Run every `*.json` fixture in `dir` against the adapter for `runtime`, returning
/// how many were checked so the caller can assert none were silently skipped.
fn run_fixtures(dir: &str, runtime: Runtime) -> usize {
    let dir_path = fixtures_dir(dir);
    let entries =
        fs::read_dir(&dir_path).unwrap_or_else(|e| panic!("read {}: {e}", dir_path.display()));

    let mut checked = 0;
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("read_dir entry in {}: {e}", dir_path.display()))
            .path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let fixture: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("{}: invalid fixture JSON: {e}", path.display()));

        let event = fixture["event"]
            .as_str()
            .unwrap_or_else(|| panic!("{}: fixture missing string 'event'", path.display()));
        let payload = fixture
            .get("payload")
            .unwrap_or_else(|| panic!("{}: fixture missing 'payload'", path.display()))
            .to_string();

        let envelope = adapters::normalize(runtime, event, &payload)
            .unwrap_or_else(|e| panic!("{}: normalize({event:?}) failed: {e}", path.display()));
        let actual = serde_json::to_value(&envelope).unwrap();

        if let Some(expect) = fixture.get("expect") {
            assert_subset(&actual, expect, "", &path, &actual);
        }
        checked += 1;
    }
    checked
}

/// Assert every key `expect` names is present and equal on `actual`, recursing into
/// nested objects so a fixture can assert `"tool": {"name": "Bash"}` and leave
/// `tool.input_hash` (and everything else on `tool`) unchecked. `full` is threaded
/// through only so a failing assertion can print the whole envelope for context.
fn assert_subset(
    actual: &Value,
    expect: &Value,
    path_so_far: &str,
    fixture_path: &Path,
    full: &Value,
) {
    let Value::Object(expect_map) = expect else {
        assert_eq!(
            actual,
            expect,
            "{}: field '{path_so_far}' mismatch\nfull envelope: {full:#}",
            fixture_path.display()
        );
        return;
    };
    for (key, expected_value) in expect_map {
        let dotted = if path_so_far.is_empty() {
            key.clone()
        } else {
            format!("{path_so_far}.{key}")
        };
        let actual_value = actual.get(key).unwrap_or_else(|| {
            panic!(
                "{}: expected field '{dotted}' is absent from the envelope: {full:#}",
                fixture_path.display()
            )
        });
        assert_subset(actual_value, expected_value, &dotted, fixture_path, full);
    }
}

#[test]
fn claude_code_fixtures_cover_every_documented_event_and_match_expected_fields() {
    // SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFailure,
    // PreCompact, SessionEnd, Stop — see adapters/claude_code.rs's `normalize` match.
    const EXPECTED_EVENT_COUNT: usize = 8;
    let checked = run_fixtures("claude_code", Runtime::ClaudeCode);
    assert_eq!(
        checked, EXPECTED_EVENT_COUNT,
        "a claude_code event is missing its golden fixture (or an extra file crept in)"
    );
}

#[test]
fn cursor_fixtures_cover_every_documented_event_and_match_expected_fields() {
    // See adapters/cursor.rs's TOOL_EVENTS (10) plus sessionStart, sessionEnd,
    // beforeSubmitPrompt, preCompact, stop, afterAgentResponse (6) = 16.
    const EXPECTED_EVENT_COUNT: usize = 16;
    let checked = run_fixtures("cursor", Runtime::Cursor);
    assert_eq!(
        checked, EXPECTED_EVENT_COUNT,
        "a cursor event is missing its golden fixture (or an extra file crept in)"
    );
}

#[test]
fn hermes_fixtures_cover_every_documented_event_and_match_expected_fields() {
    // on_session_start, pre_llm_call, post_llm_call, pre_tool_call, post_tool_call,
    // on_session_end, on_session_finalize, on_session_reset — see adapters/hermes.rs's
    // `normalize` match and docs/runtimes/hermes.md's event table.
    const EXPECTED_EVENT_COUNT: usize = 8;
    let checked = run_fixtures("hermes", Runtime::Hermes);
    assert_eq!(
        checked, EXPECTED_EVENT_COUNT,
        "a hermes event is missing its golden fixture (or an extra file crept in)"
    );
}

/// The fixture harness itself must fail loudly on a genuine mismatch, and must not
/// demand fields a fixture never mentioned — a harness that always "passes", or one
/// that requires every fixture to spell out the whole envelope, would both defeat the
/// point of a golden-fixture regression net.
#[test]
fn assert_subset_ignores_unlisted_fields_but_catches_a_real_mismatch() {
    let dummy = PathBuf::from("dummy.json");
    let actual = serde_json::json!({
        "tool": {"name": "Bash", "input_hash": "sha256:unrelated"},
        "session_id": "s1"
    });

    // Passes: only the listed fields are checked.
    assert_subset(
        &actual,
        &serde_json::json!({"tool": {"name": "Bash"}}),
        "",
        &dummy,
        &actual,
    );

    // A genuine mismatch must still panic.
    let result = std::panic::catch_unwind(|| {
        assert_subset(
            &actual,
            &serde_json::json!({"tool": {"name": "Read"}}),
            "",
            &dummy,
            &actual,
        );
    });
    assert!(
        result.is_err(),
        "assert_subset must fail on a real mismatch"
    );
}
