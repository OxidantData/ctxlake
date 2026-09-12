//! Runtime adapters: normalize a hook's native event into a [`ctxlake_core::Envelope`].
//!
//! Hermes reaches this binary through shell hooks, a wire contract deliberately
//! Claude Code-compatible (`docs/runtimes.md § Hermes`) — it no longer runs an
//! in-process Python plugin (`adapters/hermes/`, deleted: a hand-ported second
//! redactor with no shared source was the exact silent-divergence risk AGENTS.md
//! invariant 7 exists to rule out). See [`hermes`] for the payload shape.

pub mod claude_code;
pub mod common;
pub mod cursor;
pub mod hermes;

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
/// injects a briefing, because both need data — a synthesized digest, a rendered
/// briefing — that only the daemon (a later wave) can supply without putting the
/// store on the hook path (AGENTS.md invariant 1). `runtime` is read directly from
/// argv rather than parsed through [`parse_runtime`], because an unrecognized
/// runtime should still get *some* runtime's safe default rather than a made-up
/// third shape.
pub fn response_for(runtime: &str, event: &str) -> String {
    match runtime {
        "cursor" => cursor::response_for(event),
        "hermes" => hermes::response_for(event),
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
        Runtime::Hermes => hermes::normalize(event, &value),
        Runtime::Other => Err(
            "ctxlake-hook does not normalize an unrecognized runtime live (no adapter to \
             normalize against)"
                .to_string(),
        ),
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
    fn normalize_refuses_an_unrecognized_runtime_rather_than_guessing() {
        assert!(normalize(Runtime::Other, "whatever", "{}").is_err());
    }

    #[test]
    fn normalize_routes_hermes_to_its_own_adapter() {
        // Regression: Hermes used to have no adapter at all (an in-process Python
        // plugin instead); this pins that it now normalizes live, through this
        // module's own dispatch, rather than being silently refused again.
        let env = normalize(
            Runtime::Hermes,
            "on_session_start",
            r#"{"session_id":"s1"}"#,
        )
        .unwrap();
        assert_eq!(env.runtime, Runtime::Hermes);
    }

    #[test]
    fn response_for_routes_hermes_to_its_own_adapter() {
        assert_eq!(response_for("hermes", "pre_tool_call"), "{}");
    }
}
