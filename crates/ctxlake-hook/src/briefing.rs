//! Reading the pre-rendered briefing and handing it to the agent.
//!
//! This is the last link in the read path, and it is deliberately the dumbest one.
//! Everything expensive — listing the roster, folding claims, applying attribution and
//! sanitization — happens in `ctxlake sync`'s cache leg, which writes a finished string
//! to `<cache_root>/<fleet_id>/briefing.json`. All this module does is read that file
//! and wrap it in whatever shape the runtime expects.
//!
//! It has to be that way. The hook runs on every session start with a 5ms budget and
//! may not touch the object store (AGENTS.md invariants 1 and 2), so it cannot render
//! anything itself: rendering needs the lake. Splitting it here means the hook stays a
//! file read, and the part that needs data stays in the process that already has it.
//!
//! **The hook fails open.** No cache, unreadable file, malformed JSON, daemon never
//! started — every one of those yields no briefing and a normal session, never an
//! error and never a blocked start. A missing briefing costs the agent some context; a
//! hook that fails a session start costs the user their turn. Those are not close.

/// How much briefing text is allowed into a session.
///
/// This rides in the context window of every session in the fleet, so an unbounded
/// briefing is an unbounded tax on every agent forever. The cap is enforced here, at
/// the last possible moment, rather than trusting whatever wrote the cache — a renderer
/// bug should cost a truncated briefing, not a blown context window.
const MAX_BRIEFING_BYTES: usize = 8 * 1024;

/// The briefing text, or `None` when there is nothing to inject.
///
/// `None` is the normal case on a fresh install, in shadow mode, and any time the
/// daemon has not completed a first refresh. None of those is an error.
pub fn read(fleet_id: &str) -> Option<String> {
    read_at(&ctxlake_core::paths::cache_root(), fleet_id)
}

/// [`read`], with the cache root injected.
///
/// The split exists so tests never have to set `CTXLAKE_CACHE_DIR`. Rust runs tests in
/// parallel threads and that variable is process-global, so env-mutating tests race
/// each other — which is exactly how the first version of this module's tests failed,
/// intermittently and for a reason that had nothing to do with the code under test.
pub fn read_at(cache_root: &std::path::Path, fleet_id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(cache_root.join(fleet_id).join("briefing.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let text = v.get("text")?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(truncate_on_char_boundary(text, MAX_BRIEFING_BYTES))
}

/// Truncate to a byte budget without splitting a character.
///
/// A briefing carries agent-authored text — task descriptions, handoff notes — so it is
/// routinely multi-byte. Slicing at a raw byte index would panic, inside a hook, on a
/// session start.
fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[ctxlake: briefing truncated]", &s[..end])
}

/// The `SessionStart` response for a runtime, carrying the briefing when there is one.
///
/// Returns `None` when the runtime has no injection channel or there is nothing to
/// inject, so the caller keeps its existing default rather than inventing a shape.
pub fn session_start_response(runtime: &str, fleet_id: &str) -> Option<String> {
    session_start_response_at(&ctxlake_core::paths::cache_root(), runtime, fleet_id)
}

/// [`session_start_response`], with the cache root injected — see [`read_at`].
pub fn session_start_response_at(
    cache_root: &std::path::Path,
    runtime: &str,
    fleet_id: &str,
) -> Option<String> {
    let text = read_at(cache_root, fleet_id)?;
    match runtime {
        // Verified against a live install: `hookSpecificOutput.additionalContext` is
        // what Claude Code reads on SessionStart.
        "claude_code" => Some(
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": text,
                }
            })
            .to_string(),
        ),
        // Hermes injects from `pre_llm_call` — a `{"context": ...}` return is appended
        // to the turn's user message (docs/runtimes.md § Hermes, read from its source).
        // Deliberately NOT on_session_start: that hook has no injection channel, and
        // returning a context shape there would be silently discarded.
        "hermes" => Some(serde_json::json!({ "context": text }).to_string()),
        // Cursor's sessionStart has no documented injection field, and no live capture
        // has shown one. Rather than guess a shape, Cursor agents reach the same data
        // through the `fleet_status` MCP tool — see docs/runtimes.md § Cursor.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn cache_with(body: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(b) = body {
            let d = dir.path().join("f1");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("briefing.json"), b).unwrap();
        }
        dir
    }

    fn resp(dir: &Path, runtime: &str) -> Option<String> {
        session_start_response_at(dir, runtime, "f1")
    }

    #[test]
    fn a_rendered_briefing_reaches_claude_code_as_additional_context() {
        let body = serde_json::json!({ "text": "## Fleet\n- cc-01 holds crates/**" }).to_string();
        let d = cache_with(Some(&body));
        let r = resp(d.path(), "claude_code").expect("a response");
        assert!(r.contains("additionalContext"), "got: {r}");
        assert!(r.contains("cc-01 holds"), "briefing text must survive: {r}");
    }

    #[test]
    fn hermes_gets_the_context_shape_its_source_documents() {
        let body = serde_json::json!({ "text": "hello" }).to_string();
        let d = cache_with(Some(&body));
        let r = resp(d.path(), "hermes").expect("a response");
        assert!(r.contains("\"context\""), "got: {r}");
    }

    #[test]
    fn no_cache_means_no_briefing_and_no_error() {
        // The most common case by far: a fresh install, or the daemon has not finished
        // a first refresh. It must be silent, never a failure.
        let d = cache_with(None);
        assert!(resp(d.path(), "claude_code").is_none());
        assert!(read_at(d.path(), "f1").is_none());
    }

    #[test]
    fn malformed_or_empty_cache_is_treated_as_no_briefing() {
        for body in ["not json", "{}", "{\"text\":\"   \"}", "{\"text\":5}"] {
            let d = cache_with(Some(body));
            assert!(
                read_at(d.path(), "f1").is_none(),
                "should be silent: {body}"
            );
        }
    }

    #[test]
    fn an_oversized_briefing_is_truncated_rather_than_injected_whole() {
        let huge = "x".repeat(MAX_BRIEFING_BYTES * 3);
        let body = serde_json::json!({ "text": huge }).to_string();
        let d = cache_with(Some(&body));
        let got = read_at(d.path(), "f1").expect("truncated, not dropped");
        assert!(got.len() <= MAX_BRIEFING_BYTES + 64, "len {}", got.len());
        assert!(got.contains("truncated"), "truncation must be visible");
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // Briefings carry agent-authored prose, so multi-byte content is routine.
        // Slicing at a raw byte index would panic inside a hook, on session start.
        let s = "é".repeat(MAX_BRIEFING_BYTES);
        let out = truncate_on_char_boundary(&s, MAX_BRIEFING_BYTES);
        assert!(out.len() <= MAX_BRIEFING_BYTES + 64);
        assert!(out.starts_with('é'));
    }

    #[test]
    fn cursor_gets_no_injected_shape_because_none_is_known() {
        let body = serde_json::json!({ "text": "hello" }).to_string();
        let d = cache_with(Some(&body));
        assert!(
            resp(d.path(), "cursor").is_none(),
            "inventing an injection shape would be silently discarded at best"
        );
    }
}
