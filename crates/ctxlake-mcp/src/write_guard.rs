//! Secret-redaction gate for the write path — the mirror of `sanitize.rs`, which
//! guards the read path.
//!
//! AGENTS.md invariant 7: redaction runs before the spool, on every path, no
//! exceptions. Every write-shaped tool in this crate (`fleet_handoff`,
//! `memory_propose`) accepts free-text fields from whatever runtime is hosting
//! this MCP server — a claim, a handoff note, an evidence citation — and that
//! text can contain whatever the calling agent decided to paste, including a
//! real credential copied out of a
//! shell error into a handoff note. Once a line lands in `spool.rs`'s ndjson file,
//! `ctxlake sync` (later work) drains it into bronze, and bronze is immutable: no
//! pass after this one can un-leak it. So the scrub has to happen here, before
//! `spool::append_at` is ever called, using the same `ctxlake_core::redact::Redactor`
//! the hook's own adapters use for the identical reason (see
//! `crates/ctxlake-hook/src/adapters/common.rs::scrub_field`).
//!
//! `is_tool_output` mirrors the hook's own distinction (`ctxlake_core::redact`'s
//! module docs): prose typed by a human or an agent (a claim, a handoff summary)
//! gets literal-marker matching only, since the entropy heuristic trips on
//! ordinary prose far more readily than it does on genuine tool output. Evidence
//! citations are treated as tool-output-shaped here, since a caller may paste a
//! raw excerpt (e.g. the output that convinced them of a claim) directly into one.

use ctxlake_core::redact::Redactor;
use serde_json::Value;

/// Bound a field's length (per `sanitize::clean`'s reasoning) and then scrub it for
/// known secret shapes, in that order: stripping invisible/bidi codepoints first
/// means a secret can't be smuggled past the literal-marker scan by interleaving
/// zero-width characters through it.
pub fn bound_and_scrub_str(
    redactor: &Redactor,
    input: &str,
    max_chars: usize,
    is_tool_output: bool,
) -> String {
    let bounded = crate::sanitize::clean(input, max_chars);
    redactor.scrub(&bounded, is_tool_output).1
}

/// Recursively apply [`bound_and_scrub_str`] to every string in a JSON value —
/// object keys and values, array elements, at any depth. Used for `evidence`,
/// which is caller-shaped JSON this crate does not control the schema of: a fixed
/// per-field allowlist here would have exactly the gap AGENTS.md's house rule
/// warns about (see `sanitize::clean_value`'s docs for the same argument applied
/// to the read path), and a citation is meant to be an identifier, not a place to
/// paste a paragraph — the length bound holds regardless of how deep the caller
/// nests it.
pub fn bound_and_scrub_value(
    redactor: &Redactor,
    v: &Value,
    max_chars: usize,
    is_tool_output: bool,
) -> Value {
    match v {
        Value::String(s) => {
            Value::String(bound_and_scrub_str(redactor, s, max_chars, is_tool_output))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| bound_and_scrub_value(redactor, item, max_chars, is_tool_output))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| {
                    (
                        k.clone(),
                        bound_and_scrub_value(redactor, val, max_chars, is_tool_output),
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bound_and_scrub_str_withholds_a_literal_secret_marker() {
        let r = Redactor::new();
        let out = bound_and_scrub_str(
            &r,
            "blocked: export ANTHROPIC_API_KEY=sk-ant-api03-REALLOOKINGSECRET1234567890 did not help",
            crate::sanitize::MAX_LONG_FIELD,
            false,
        );
        assert!(!out.contains("sk-ant-api03-REALLOOKINGSECRET1234567890"));
        assert!(out.contains("withheld"));
    }

    #[test]
    fn bound_and_scrub_str_leaves_clean_prose_alone() {
        let r = Redactor::new();
        let out = bound_and_scrub_str(&r, "implemented the mcp server", 200, false);
        assert_eq!(out, "implemented the mcp server");
    }

    #[test]
    fn bound_and_scrub_value_recurses_into_nested_evidence() {
        let r = Redactor::new();
        let v = json!({
            "session_id": "s1",
            "quote": "found it in AKIAABCDEFGHIJKLMNOP, don't reuse that",
        });
        let out = bound_and_scrub_value(&r, &v, crate::sanitize::MAX_SHORT_FIELD, true);
        assert_eq!(out["session_id"], "s1");
        assert!(!out["quote"]
            .as_str()
            .unwrap()
            .contains("AKIAABCDEFGHIJKLMNOP"));
    }
}
