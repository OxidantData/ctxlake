//! The Claude Code adapter, asserted against payloads captured from a live session.
//!
//! `tests/fixtures/claude_code/` was written from the published hooks reference. This
//! directory holds what the binary actually sent, and the two disagree.
//!
//! That disagreement was invisible from inside the repo: every test passed, the hook
//! fired, events reached the spool with correct ids, timestamps, cwd and attribution —
//! and every prompt event carried no prompt text, hashing the empty string instead.
//! Nothing anywhere reported a problem. It surfaced only from running a real session and
//! looking at what landed.
//!
//! Third runtime, same lesson: a fixture written from documentation agrees with an
//! implementation written from the same documentation, so neither can catch the other
//! being wrong about a third party's wire format.

use ctxlake_core::Runtime;
use ctxlake_hook::adapters;
use std::fs;
use std::path::PathBuf;

fn verified(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-verified")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn envelope(event: &str, fixture: &str) -> ctxlake_core::Envelope {
    adapters::normalize(Runtime::ClaudeCode, event, &verified(fixture))
        .unwrap_or_else(|e| panic!("{fixture} failed to normalize: {e}"))
}

/// sha256 of the empty string. Seeing this as an event's `content_hash` is the exact
/// signature of the bug: content that should have been captured was silently absent.
const EMPTY_SHA: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[test]
fn prompt_text_is_captured() {
    let env = envelope("UserPromptSubmit", "user_prompt_submit.json");
    let content = env
        .content
        .expect("a prompt event must carry the prompt text");
    assert_eq!(content, "say hi", "the captured prompt should be verbatim");
    assert_ne!(
        env.content_hash, EMPTY_SHA,
        "content_hash is the empty-string hash — the prompt was not captured"
    );
}

#[test]
fn the_documented_field_name_still_works() {
    // The published reference says `user_input`. The binary sends `prompt`. Both are
    // read, because either could be correct on a version we have not observed, and
    // reading only one of them is how this broke the first time.
    let raw = r#"{"session_id":"s1","cwd":"/tmp","hook_event_name":"UserPromptSubmit","user_input":"documented shape"}"#;
    let env = adapters::normalize(Runtime::ClaudeCode, "UserPromptSubmit", raw).unwrap();
    assert_eq!(env.content.as_deref(), Some("documented shape"));
}

#[test]
fn the_observed_field_wins_when_both_are_present() {
    // Not a shape anyone has seen, but the precedence should be deliberate rather than
    // whatever the fallback chain happens to do.
    let raw = r#"{"session_id":"s1","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"observed","user_input":"documented"}"#;
    let env = adapters::normalize(Runtime::ClaudeCode, "UserPromptSubmit", raw).unwrap();
    assert_eq!(env.content.as_deref(), Some("observed"));
}

#[test]
fn session_start_normalizes_from_the_real_payload() {
    // The live payload carries `source: "startup"`, where the reference documents
    // `startup_reason`. The adapter reads neither, so nothing breaks — but the fixture
    // pins the real shape so a future change reads the right key rather than the
    // documented one.
    let env = envelope("SessionStart", "session_start.json");
    assert_eq!(env.runtime, Runtime::ClaudeCode);
    assert!(env.cwd.is_some(), "cwd must survive normalization");
    assert!(!env.session_id.is_empty());
    assert!(
        verified("session_start.json").contains("\"source\""),
        "fixture should preserve the observed `source` key"
    );
}

#[test]
fn redaction_still_runs_on_a_real_prompt() {
    // The capture path must not become a way around the scrubber just because the
    // payload came from a real session rather than a fixture.
    let raw = r#"{"session_id":"s1","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"use AKIAIOSFODNN7EXAMPLE for the bucket"}"#;
    let env = adapters::normalize(Runtime::ClaudeCode, "UserPromptSubmit", raw).unwrap();
    assert_ne!(env.redaction.status, "clean", "a key prefix must be caught");
    assert!(
        !env.content.clone().unwrap_or_default().contains("AKIA"),
        "the credential must not survive into the envelope"
    );
}
