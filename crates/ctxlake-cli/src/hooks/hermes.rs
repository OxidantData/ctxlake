//! Hermes — `~/.hermes/config.yaml`, top-level `hooks:` block.
//!
//! Per AGENTS.md and docs/runtimes/hermes.md: shell hooks, never the Python plugin
//! in `adapters/hermes/` (that mechanism is being retired precisely because a second
//! redaction implementation is a liability — see that doc's module note). Shape is
//! flat, like Cursor's: `hooks.<event>` is a list of `{command, matcher?, timeout?,
//! fail_closed?}`.
//!
//! `fail_closed` is meaningful on exactly one event — `pre_tool_call`, the sole
//! blocking hook — and Hermes logs a warning if it is set anywhere else (per
//! docs/runtimes/hermes.md), so this installer only ever writes it there, left at
//! `false`: a collision check is advisory, and a hook crash must never wedge a
//! session (AGENTS.md invariant 5's reasoning, applied to the hook itself).

use anyhow::{anyhow, Result};
use serde_yaml::{Mapping, Value};

use super::{hook_command, is_ours, Runtime};

/// Hermes' own hook names (`docs/runtimes/hermes.md`'s event-mapping table) — both
/// the YAML key under `hooks:` and, per `ctxlake-hook`'s argv[1] contract, the event
/// name passed to the binary. `compact` has no Hermes equivalent and is omitted, the
/// same gap that page documents.
const EVENTS: &[&str] = &[
    "on_session_start",
    "pre_llm_call",
    "pre_tool_call",
    "post_tool_call",
    "post_llm_call",
    "on_session_end",
];

/// Only `pre_tool_call` gets `timeout`/`fail_closed` — see the module doc.
const PRE_TOOL_CALL: &str = "pre_tool_call";

fn parse_or_empty(text: Option<&str>) -> Result<Value> {
    match text.map(str::trim) {
        None | Some("") => Ok(Value::Mapping(Mapping::new())),
        Some(s) => serde_yaml::from_str(s).map_err(|e| {
            anyhow!(
                "existing config is not valid YAML ({e}) — refusing to modify it; \
                 fix or remove it by hand, then re-run install"
            )
        }),
    }
}

fn describe(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a sequence",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}

fn ensure_mapping<'a>(parent: &'a mut Value, key: &str) -> Result<&'a mut Value> {
    let Value::Mapping(map) = parent else {
        return Err(anyhow!("expected a mapping, found {}", describe(parent)));
    };
    if !map.contains_key(key) {
        map.insert(Value::from(key), Value::Mapping(Mapping::new()));
    }
    let child = map.get_mut(key).expect("just inserted or already present");
    if !matches!(child, Value::Mapping(_)) {
        return Err(anyhow!(
            "expected \"{key}\" to be a mapping, found {}",
            describe(child)
        ));
    }
    Ok(child)
}

fn ensure_sequence<'a>(parent: &'a mut Value, key: &str) -> Result<&'a mut Value> {
    let Value::Mapping(map) = parent else {
        return Err(anyhow!("expected a mapping, found {}", describe(parent)));
    };
    if !map.contains_key(key) {
        map.insert(Value::from(key), Value::Sequence(Vec::new()));
    }
    let child = map.get_mut(key).expect("just inserted or already present");
    if !matches!(child, Value::Sequence(_)) {
        return Err(anyhow!(
            "expected \"{key}\" to be a sequence, found {}",
            describe(child)
        ));
    }
    Ok(child)
}

fn remove_if_empty(parent: &mut Value, key: &str) {
    let Value::Mapping(map) = parent else {
        return;
    };
    let is_empty = match map.get(key) {
        Some(Value::Sequence(s)) => s.is_empty(),
        Some(Value::Mapping(m)) => m.is_empty(),
        _ => false,
    };
    if is_empty {
        // `shift_remove`, not `remove` (== `swap_remove`): swap-removing would move
        // whatever key happens to sit last in the map into this slot, silently
        // reordering an event the operator never touched.
        map.shift_remove(key);
    }
}

fn command_of(entry: &Value) -> Option<&str> {
    entry.get("command").and_then(Value::as_str)
}

fn strip_all(doc: &mut Value, event: &str) -> Result<()> {
    let has_event = doc.get("hooks").and_then(|h| h.get(event)).is_some();
    if !has_event {
        return Ok(());
    }
    let hooks = ensure_mapping(doc, "hooks")?;
    let arr = ensure_sequence(hooks, event)?;
    if let Value::Sequence(entries) = arr {
        entries.retain(|e| !command_of(e).is_some_and(is_ours));
    }
    remove_if_empty(hooks, event);
    Ok(())
}

fn add(doc: &mut Value, event: &str, fleet_id: &str, agent_id: &str) -> Result<()> {
    strip_all(doc, event)?;
    let hooks = ensure_mapping(doc, "hooks")?;
    let arr = ensure_sequence(hooks, event)?;
    let cmd = hook_command(fleet_id, agent_id, Runtime::Hermes, event);

    let mut entry = Mapping::new();
    entry.insert(Value::from("command"), Value::from(cmd));
    if event == PRE_TOOL_CALL {
        entry.insert(Value::from("timeout"), Value::from(5));
        entry.insert(Value::from("fail_closed"), Value::from(false));
    }

    if let Value::Sequence(entries) = arr {
        entries.push(Value::Mapping(entry));
    }
    Ok(())
}

pub fn install(existing: Option<&str>, fleet_id: &str, agent_id: &str) -> Result<String> {
    let mut doc = parse_or_empty(existing)?;
    for event in EVENTS {
        add(&mut doc, event, fleet_id, agent_id)?;
    }
    remove_if_empty(&mut doc, "hooks");
    serde_yaml::to_string(&doc).map_err(|e| anyhow!("serializing config.yaml: {e}"))
}

pub fn uninstall(existing: Option<&str>) -> Result<String> {
    let mut doc = parse_or_empty(existing)?;
    for event in EVENTS {
        strip_all(&mut doc, event)?;
    }
    remove_if_empty(&mut doc, "hooks");
    serde_yaml::to_string(&doc).map_err(|e| anyhow!("serializing config.yaml: {e}"))
}

/// See `claude_code::count_entries` — same purpose, flat shape.
pub fn count_entries(text: &str) -> usize {
    let Ok(doc) = parse_or_empty(Some(text)) else {
        return 0;
    };
    EVENTS
        .iter()
        .map(|event| match doc.get("hooks").and_then(|h| h.get(event)) {
            Some(Value::Sequence(entries)) => entries.len(),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn install_on_an_absent_file_creates_every_event() {
        let out = install(None, "myteam", "cc-01").unwrap();
        let doc = parsed(&out);
        for event in EVENTS {
            assert_eq!(
                doc["hooks"][*event].as_sequence().unwrap().len(),
                1,
                "{out}"
            );
        }
    }

    #[test]
    fn only_pre_tool_call_gets_timeout_and_fail_closed() {
        let out = install(None, "myteam", "cc-01").unwrap();
        let doc = parsed(&out);
        assert_eq!(doc["hooks"]["pre_tool_call"][0]["timeout"], Value::from(5));
        assert_eq!(
            doc["hooks"]["pre_tool_call"][0]["fail_closed"],
            Value::from(false)
        );
        assert!(doc["hooks"]["post_tool_call"][0]
            .as_mapping()
            .unwrap()
            .get("timeout")
            .is_none());
    }

    #[test]
    fn install_preserves_another_plugins_entry_on_the_same_event() {
        let existing = "hooks:\n  post_tool_call:\n    - command: other-plugin --run\n";
        let out = install(Some(existing), "myteam", "cc-01").unwrap();
        let doc = parsed(&out);
        let entries = doc["hooks"]["post_tool_call"].as_sequence().unwrap();
        assert_eq!(entries.len(), 2, "{out}");
        assert_eq!(entries[0]["command"], Value::from("other-plugin --run"));
        assert!(entries[1]["command"]
            .as_str()
            .unwrap()
            .contains("ctxlake-hook"));
    }

    #[test]
    fn install_twice_is_idempotent() {
        let once = install(None, "myteam", "cc-01").unwrap();
        let twice = install(Some(&once), "myteam", "cc-01").unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn uninstall_is_exact() {
        let existing = "hooks:\n  post_tool_call:\n    - command: other-plugin --run\n";
        let installed = install(Some(existing), "myteam", "cc-01").unwrap();
        let uninstalled = uninstall(Some(&installed)).unwrap();
        let doc = parsed(&uninstalled);
        let entries = doc["hooks"]["post_tool_call"].as_sequence().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], Value::from("other-plugin --run"));
        for event in EVENTS.iter().filter(|e| **e != "post_tool_call") {
            assert!(
                doc["hooks"].get(event).is_none(),
                "{event} should be gone: {uninstalled}"
            );
        }
    }

    #[test]
    fn uninstall_drops_hooks_entirely_when_ctxlake_was_the_only_content() {
        let installed = install(None, "myteam", "cc-01").unwrap();
        let uninstalled = uninstall(Some(&installed)).unwrap();
        let doc = parsed(&uninstalled);
        assert!(
            doc.as_mapping().map(Mapping::is_empty).unwrap_or(true) || doc.get("hooks").is_none(),
            "no trace of an install should remain: {uninstalled}"
        );
    }

    #[test]
    fn malformed_existing_yaml_is_refused() {
        let err = install(Some(": : not : valid : yaml : ["), "myteam", "cc-01").unwrap_err();
        assert!(err.to_string().contains("not valid YAML"));
    }
}
