//! Runtime adapters: normalize a hook's native event into a [`ctxlake_core::Envelope`].
//!
//! Hermes has no adapter here — it is an in-process Python plugin (`adapters/hermes/`
//! at the repo root) that writes to the same spool directly, because spawning this
//! binary once per LLM call would itself blow the latency budget it exists to protect.
//! `Runtime::Hermes` still exists on the [`ctxlake_core::Runtime`] enum for the
//! *import* path (`ctxlake import`, a later wave), which is why [`parse_runtime`]
//! recognizes the string without this module ever normalizing a live Hermes payload.

pub mod claude_code;
pub mod common;
pub mod cursor;

use ctxlake_core::{Envelope, Runtime};

/// argv[2] names which adapter owns this invocation. An unrecognized value maps to
/// [`Runtime::Other`] rather than erroring — the same reasoning as that variant's own
/// doc comment: a new adapter's rollout should not lose data before this match learns
/// its name.
pub fn parse_runtime(s: &str) -> Runtime {
    match s {
        "claude_code" => Runtime::ClaudeCode,
        "cursor" => Runtime::Cursor,
        "hermes" => Runtime::Hermes,
        _ => Runtime::Other,
    }
}

/// The instant, payload-independent response written before stdin is touched (see
/// `main.rs`'s latency trick). Wave 1 captures only: it never blocks a tool call or
/// injects a briefing, because both need data — a lease read, a synthesized digest —
/// that only the daemon (a later wave) can supply without putting the store on the
/// hook path (AGENTS.md invariant 1). `runtime` is read directly from argv rather than
/// parsed through [`parse_runtime`], because an unrecognized runtime should still get
/// *some* runtime's safe default rather than a made-up third shape.
pub fn response_for(runtime: &str, event: &str) -> String {
    match runtime {
        "cursor" => cursor::response_for(event),
        _ => claude_code::response_for(event),
    }
}

/// Parse `raw` and hand it to the adapter for `runtime`. Never panics: a JSON error,
/// a missing required field, or an unrecognized event name all come back as `Err` for
/// the caller to log — see `main.rs`'s "exit 0 either way" contract.
pub fn normalize(runtime: Runtime, event: &str, raw: &str) -> Result<Envelope, String> {
    if raw.trim().is_empty() {
        return Err("empty stdin".to_string());
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid JSON on stdin: {e}"))?;

    match runtime {
        Runtime::ClaudeCode => claude_code::normalize(event, &value),
        Runtime::Cursor => cursor::normalize(event, &value),
        Runtime::Hermes | Runtime::Other => Err(format!(
            "ctxlake-hook does not normalize {} live (Hermes writes its own spool lines; \
             an unrecognized runtime has no adapter to normalize against)",
            runtime.as_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_runtime_maps_known_names() {
        assert_eq!(parse_runtime("claude_code"), Runtime::ClaudeCode);
        assert_eq!(parse_runtime("cursor"), Runtime::Cursor);
        assert_eq!(parse_runtime("hermes"), Runtime::Hermes);
    }

    #[test]
    fn parse_runtime_carries_through_unknown_names_rather_than_failing() {
        assert_eq!(parse_runtime("some-future-runtime"), Runtime::Other);
        assert_eq!(parse_runtime(""), Runtime::Other);
    }

    #[test]
    fn normalize_rejects_empty_stdin_without_panicking() {
        let err = normalize(Runtime::ClaudeCode, "SessionStart", "").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn normalize_rejects_malformed_json_without_panicking() {
        let err = normalize(Runtime::ClaudeCode, "SessionStart", "{not json").unwrap_err();
        assert!(err.contains("invalid JSON"), "got: {err}");
    }

    #[test]
    fn normalize_refuses_hermes_and_other_rather_than_guessing() {
        assert!(normalize(Runtime::Hermes, "pre_tool_call", "{}").is_err());
        assert!(normalize(Runtime::Other, "whatever", "{}").is_err());
    }
}
