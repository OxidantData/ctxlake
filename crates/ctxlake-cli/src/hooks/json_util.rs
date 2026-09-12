//! Small, order-preserving JSON helpers shared by the Claude Code and Cursor
//! installers.
//!
//! Built on the `json` crate rather than `serde_json::Value`: `serde_json::Map` is a
//! `BTreeMap` unless the crate's `preserve_order` feature is enabled, and turning
//! that on here would (via Cargo's feature unification across a workspace build)
//! risk leaking an extra dependency into `ctxlake-hook`'s tree — the one thing
//! AGENTS.md invariant 2 exists to prevent. `json::object::Object` preserves
//! insertion order by construction, with no shared feature flag to reason about, so
//! a config's untouched keys never get silently alphabetized on write.

use anyhow::{anyhow, Result};
use json::JsonValue;

/// Parse `text`, or start a fresh empty object if there is nothing to parse yet.
/// Blank/whitespace-only content (an empty file `ctxlake install` finds) is treated
/// the same as "file absent" — there is nothing there to lose either way.
pub fn parse_or_empty(text: Option<&str>) -> Result<JsonValue> {
    match text.map(str::trim) {
        None | Some("") => Ok(JsonValue::new_object()),
        Some(s) => json::parse(s).map_err(|e| {
            anyhow!(
                "existing config is not valid JSON ({e}) — refusing to modify it; \
                 fix or remove it by hand, then re-run install"
            )
        }),
    }
}

/// Get-or-insert an object-typed child at `key`. Errors rather than clobbering if
/// the key exists with some other shape (a hand-edited `"hooks": "disabled"`,
/// say) — silently overwriting it would be exactly the kind of data loss AGENTS.md
/// invariant 8 exists to rule out.
pub fn ensure_object<'a>(parent: &'a mut JsonValue, key: &str) -> Result<&'a mut JsonValue> {
    let JsonValue::Object(obj) = parent else {
        return Err(anyhow!("expected an object, found {}", describe(parent)));
    };
    if obj.get(key).is_none() {
        obj.insert(key, JsonValue::new_object());
    }
    let child = obj.get_mut(key).expect("just inserted or already present");
    if !child.is_object() {
        return Err(anyhow!(
            "expected \"{key}\" to be an object, found {}",
            describe(child)
        ));
    }
    Ok(child)
}

/// Get-or-insert an array-typed child at `key`, same refusal-over-clobber rule as
/// [`ensure_object`].
pub fn ensure_array<'a>(parent: &'a mut JsonValue, key: &str) -> Result<&'a mut JsonValue> {
    let JsonValue::Object(obj) = parent else {
        return Err(anyhow!("expected an object, found {}", describe(parent)));
    };
    if obj.get(key).is_none() {
        obj.insert(key, JsonValue::new_array());
    }
    let child = obj.get_mut(key).expect("just inserted or already present");
    if !child.is_array() {
        return Err(anyhow!(
            "expected \"{key}\" to be an array, found {}",
            describe(child)
        ));
    }
    Ok(child)
}

/// Remove `key` from `parent` if its value is an empty array or empty object.
///
/// This is the other half of "uninstall is exact": if `install` had to create the
/// `"hooks"` object, or a per-event array, because neither existed, removing only
/// ctxlake's own entries would leave an empty shell behind that the original file
/// never had. An empty hook-event array and a missing one mean the same thing to
/// every runtime here (no hooks fire), so collapsing the two on the way out
/// restores the pre-install shape rather than a technically-different-but-equivalent
/// one.
pub fn remove_if_empty(parent: &mut JsonValue, key: &str) {
    let JsonValue::Object(obj) = parent else {
        return;
    };
    let is_empty = match obj.get(key) {
        Some(JsonValue::Array(a)) => a.is_empty(),
        Some(JsonValue::Object(o)) => o.is_empty(),
        _ => false,
    };
    if is_empty {
        obj.remove(key);
    }
}

fn describe(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Short(_) | JsonValue::String(_) => "a string",
        JsonValue::Number(_) => "a number",
        JsonValue::Boolean(_) => "a boolean",
        JsonValue::Object(_) => "an object",
        JsonValue::Array(_) => "an array",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_or_empty_treats_absent_and_blank_the_same() {
        assert_eq!(parse_or_empty(None).unwrap(), JsonValue::new_object());
        assert_eq!(
            parse_or_empty(Some("   \n")).unwrap(),
            JsonValue::new_object()
        );
    }

    #[test]
    fn parse_or_empty_refuses_malformed_json() {
        let err = parse_or_empty(Some("{not json")).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn ensure_object_creates_then_reuses() {
        let mut doc = JsonValue::new_object();
        ensure_object(&mut doc, "hooks").unwrap();
        doc["hooks"]["already-here"] = "value".into();
        ensure_object(&mut doc, "hooks").unwrap();
        assert_eq!(doc["hooks"]["already-here"], "value");
    }

    #[test]
    fn ensure_object_refuses_to_clobber_a_wrong_shaped_key() {
        let mut doc = JsonValue::new_object();
        doc["hooks"] = "disabled".into();
        let err = ensure_object(&mut doc, "hooks").unwrap_err();
        assert!(err.to_string().contains("expected"));
    }

    #[test]
    fn remove_if_empty_drops_only_truly_empty_containers() {
        let mut doc = JsonValue::new_object();
        doc["a"] = json::array![];
        doc["b"] = json::array!["kept"];
        remove_if_empty(&mut doc, "a");
        remove_if_empty(&mut doc, "b");
        assert!(!doc.has_key("a"));
        assert!(doc.has_key("b"));
    }
}
