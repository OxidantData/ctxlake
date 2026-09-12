//! Shared helpers for the runtime adapters: pulling fields out of loosely-typed JSON
//! without ever panicking, bounding what we store, and folding the several redaction
//! outcomes one envelope can carry (content, tool input, tool output can each trip
//! independently) into the single `Redaction` the schema has room for.

use ctxlake_core::envelope::Redaction;
use ctxlake_core::redact::{RedactionOutcome, Redactor};
use serde_json::Value;

/// Cap on any single string field before it is hashed, redacted, or stored. Two
/// reasons, both from AGENTS.md invariant 2's 5ms budget: Aho-Corasick scanning and
/// SHA-256 hashing are cheap per byte but not free, and `spool.rs`'s single-syscall
/// write is safer the smaller the line — see its module docs.
pub const MAX_FIELD_BYTES: usize = 32 * 1024;

pub fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// A field that may arrive as a JSON string (Claude Code's `user_input`) or as a
/// structured value (every runtime's `tool_input`/`args`/`edits`) — normalize both to
/// the compact JSON text the envelope stores. A bare string is kept verbatim rather
/// than re-wrapped in quotes, so prompt/content text isn't JSON-escaped in storage.
pub fn get_stringified(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => serde_json::to_string(other).ok(),
    }
}

/// Truncate to [`MAX_FIELD_BYTES`], never splitting a multi-byte character — slicing
/// through one panics, and a panic here takes the agent's turn down with it (the same
/// reasoning as `redact.rs`'s `floor_char_boundary`, reproduced rather than shared
/// because it is three lines and not worth a `pub` export across the crate boundary).
pub fn truncate(s: &str) -> String {
    if s.len() <= MAX_FIELD_BYTES {
        return s.to_string();
    }
    let mut cut = MAX_FIELD_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...[ctxlake:truncated]", &s[..cut])
}

/// Accumulates redaction outcomes across the several fields one envelope can carry.
/// Quarantine always wins over redacted, which always wins over clean: a reader
/// checking one status field should never have to also check per-field status to know
/// whether to be careful with this event.
#[derive(Default)]
pub struct RedactionAcc {
    quarantined: bool,
    redacted: bool,
    rules: Vec<String>,
}

impl RedactionAcc {
    pub fn record(&mut self, outcome: &RedactionOutcome) {
        match outcome {
            RedactionOutcome::Clean => {}
            RedactionOutcome::Redacted { rules } => {
                self.redacted = true;
                self.merge_rules(rules);
            }
            RedactionOutcome::Quarantined { rules } => {
                self.quarantined = true;
                self.merge_rules(rules);
            }
        }
    }

    fn merge_rules(&mut self, rules: &[String]) {
        for r in rules {
            if !self.rules.iter().any(|x| x == r) {
                self.rules.push(r.clone());
            }
        }
    }

    pub fn into_redaction(self) -> Redaction {
        let status = if self.quarantined {
            "quarantined"
        } else if self.redacted {
            "redacted"
        } else {
            "clean"
        };
        Redaction {
            status: status.to_string(),
            rules_fired: self.rules,
        }
    }
}

/// Truncate, scrub, and fold the outcome into `acc` in one step — every field an
/// adapter stores goes through this so none can skip redaction by accident.
pub fn scrub_field(
    redactor: &Redactor,
    acc: &mut RedactionAcc,
    value: Option<String>,
    is_tool_output: bool,
) -> Option<String> {
    let value = value?;
    let bounded = truncate(&value);
    let (outcome, out) = redactor.scrub(&bounded, is_tool_output);
    acc.record(&outcome);
    Some(out)
}

/// The path denylist is a stronger guarantee than the marker/entropy scan: a read of
/// `.aws/credentials` is withheld even if what's inside doesn't happen to match a
/// known prefix. Applied only to *results* — the fact that a call touched this path
/// is still worth recording, per `redact.rs`'s module docs.
pub fn withhold_if_denied_path(
    redactor: &Redactor,
    acc: &mut RedactionAcc,
    path: Option<&str>,
    result: Option<String>,
) -> Option<String> {
    if let Some(p) = path {
        if redactor.is_denied_path(p) {
            acc.record(&RedactionOutcome::Quarantined {
                rules: vec!["denied_path".to_string()],
            });
            return Some(format!("[ctxlake: withheld, denied path {p}]"));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_stringified_keeps_a_plain_string_unquoted() {
        let v = serde_json::json!({"k": "hello world"});
        assert_eq!(get_stringified(&v, "k"), Some("hello world".to_string()));
    }

    #[test]
    fn get_stringified_serializes_an_object() {
        let v = serde_json::json!({"k": {"a": 1, "b": "x"}});
        let s = get_stringified(&v, "k").unwrap();
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back, serde_json::json!({"a": 1, "b": "x"}));
    }

    #[test]
    fn get_stringified_is_none_for_missing_or_null() {
        let v = serde_json::json!({"k": null});
        assert_eq!(get_stringified(&v, "k"), None);
        assert_eq!(get_stringified(&v, "missing"), None);
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("hello"), "hello");
    }

    #[test]
    fn truncate_caps_long_strings_and_marks_them() {
        let long = "x".repeat(MAX_FIELD_BYTES + 100);
        let out = truncate(&long);
        assert!(out.len() < long.len());
        assert!(out.ends_with("[ctxlake:truncated]"));
    }

    #[test]
    fn truncate_does_not_split_a_multibyte_character() {
        // A multi-byte char sitting right at the cut boundary must not panic and
        // must not appear mangled in the output.
        let long = format!("{}é", "x".repeat(MAX_FIELD_BYTES - 1));
        let out = truncate(&long); // must not panic
        assert!(out.is_char_boundary(out.len() - "...[ctxlake:truncated]".len()));
    }

    #[test]
    fn acc_quarantine_wins_over_redacted() {
        let mut acc = RedactionAcc::default();
        acc.record(&RedactionOutcome::Redacted {
            rules: vec!["high_entropy_run".to_string()],
        });
        acc.record(&RedactionOutcome::Quarantined {
            rules: vec!["aws_access_key_id".to_string()],
        });
        let r = acc.into_redaction();
        assert_eq!(r.status, "quarantined");
        assert_eq!(r.rules_fired.len(), 2);
    }

    #[test]
    fn acc_dedups_repeated_rules() {
        let mut acc = RedactionAcc::default();
        acc.record(&RedactionOutcome::Redacted {
            rules: vec!["high_entropy_run".to_string()],
        });
        acc.record(&RedactionOutcome::Redacted {
            rules: vec!["high_entropy_run".to_string()],
        });
        assert_eq!(
            acc.into_redaction().rules_fired,
            vec!["high_entropy_run".to_string()]
        );
    }

    #[test]
    fn acc_defaults_to_clean() {
        let acc = RedactionAcc::default();
        let r = acc.into_redaction();
        assert_eq!(r.status, "clean");
        assert!(r.rules_fired.is_empty());
    }

    #[test]
    fn withhold_if_denied_path_replaces_result_and_quarantines() {
        let redactor = Redactor::new();
        let mut acc = RedactionAcc::default();
        let out = withhold_if_denied_path(
            &redactor,
            &mut acc,
            Some("/Users/x/.aws/credentials"),
            Some("[default]\naws_access_key_id=AKIA...".to_string()),
        );
        let out = out.unwrap();
        assert!(out.contains("withheld"), "got: {out}");
        assert!(!out.contains("AKIA"));
        assert_eq!(acc.into_redaction().status, "quarantined");
    }

    #[test]
    fn withhold_if_denied_path_passes_through_a_safe_path() {
        let redactor = Redactor::new();
        let mut acc = RedactionAcc::default();
        let out = withhold_if_denied_path(
            &redactor,
            &mut acc,
            Some("README.md"),
            Some("contents".to_string()),
        );
        assert_eq!(out, Some("contents".to_string()));
        assert_eq!(acc.into_redaction().status, "clean");
    }
}
