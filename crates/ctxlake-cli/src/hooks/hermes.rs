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
    splice_hooks_block(existing.unwrap_or(""), hooks_value(&doc))
}

pub fn uninstall(existing: Option<&str>) -> Result<String> {
    let mut doc = parse_or_empty(existing)?;
    for event in EVENTS {
        strip_all(&mut doc, event)?;
    }
    remove_if_empty(&mut doc, "hooks");
    splice_hooks_block(existing.unwrap_or(""), hooks_value(&doc))
}

fn hooks_value(doc: &Value) -> Option<&Value> {
    match doc {
        Value::Mapping(m) => m.get("hooks"),
        _ => None,
    }
}

/// Merge `hooks_value` into `original`'s raw text, touching only the top-level
/// `hooks:` block — every other byte (comments, anchors, key order, everything
/// else in the file) is left exactly as it was. `hooks_value` is `None` when
/// nothing is left to install (an uninstall that emptied every event), in which
/// case the block is deleted rather than replaced with an empty `hooks: {}`.
///
/// This exists because round-tripping the *whole* document through
/// `serde_yaml::Value` — as this installer used to, via a single
/// `serde_yaml::to_string(&doc)` — silently drops every comment and expands every
/// anchor/alias in the file: `serde_yaml::Value` has no concept of either once
/// parsed, and that loss covers content ctxlake never touches. Splicing confines
/// the loss to the one block ctxlake actually owns, matching the same "entries
/// survive, formatting inside our own block does not" guarantee already disclosed
/// for the two JSON runtimes (docs/cli.md) — now honestly true for YAML too,
/// rather than the whole-file loss the docs previously excluded.
fn splice_hooks_block(original: &str, hooks_value: Option<&Value>) -> Result<String> {
    let block = match hooks_value {
        Some(Value::Mapping(m)) if !m.is_empty() => {
            let mut wrapper = Mapping::new();
            wrapper.insert(Value::from("hooks"), Value::Mapping(m.clone()));
            let rendered = serde_yaml::to_string(&Value::Mapping(wrapper))
                .map_err(|e| anyhow!("serializing hooks block: {e}"))?;
            // serde_yaml sometimes emits a leading `---` document marker; this is
            // spliced into the middle of an existing file, not a document of its
            // own, so that marker (and any trailing blank lines) would be noise.
            Some(rendered.trim_start_matches("---\n").trim_end().to_string() + "\n")
        }
        _ => None,
    };

    match find_hooks_span(original) {
        Some((start, end)) => {
            let mut out = String::with_capacity(original.len());
            out.push_str(&original[..start]);
            if let Some(block) = &block {
                out.push_str(block);
            }
            out.push_str(&original[end..]);
            Ok(out)
        }
        None => match block {
            None => Ok(original.to_string()),
            Some(block) if original.trim().is_empty() => Ok(block),
            Some(block) => {
                let mut out = original.to_string();
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
                out.push_str(&block);
                Ok(out)
            }
        },
    }
}

/// True for a line with no leading whitespace and some non-blank content — in YAML,
/// the only place a *new* top-level key (or, for our purposes, a boundary this
/// splice must not cross) can start. Column-0 comments count as boundaries too: a
/// `#`-comment sitting outside the indented content that follows a key is, by
/// construction, not part of that key's value.
fn is_column_zero_content(line: &str) -> bool {
    !line.starts_with(' ') && !line.starts_with('\t') && !line.trim().is_empty()
}

/// The byte range of `text` occupied by the top-level `hooks:` key: from the line
/// that starts with the literal `hooks:` through the last line before the next
/// column-0 content (another key, or a column-0 comment), or EOF. `None` if there
/// is no top-level `hooks:` key at all.
///
/// This is a plain text scan, not a YAML-position-aware one — deliberately, since a
/// position-aware parse is exactly the round-trip this function exists to avoid.
/// The one thing it assumes is the universal YAML convention that a *nested* value
/// is indented relative to its key; a hand-authored file that puts a bare, unindented
/// comment line **inside** what a human intends as the `hooks:` section (rather than
/// indenting it to match) will have that comment — and anything below it — treated
/// as outside the block. That is an unusual enough style choice to accept as a
/// disclosed edge case rather than a reason to parse indentation properly here.
fn find_hooks_span(text: &str) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    let mut start = None;
    let mut end = None;
    for line in text.split_inclusive('\n') {
        let boundary = is_column_zero_content(line);
        match start {
            None if boundary && line.starts_with("hooks:") => start = Some(offset),
            Some(_) if boundary => {
                end = Some(offset);
                break;
            }
            _ => {}
        }
        offset += line.len();
    }
    start.map(|s| (s, end.unwrap_or(text.len())))
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

    #[test]
    fn install_preserves_comments_and_anchors_with_no_hooks_block_yet() {
        // Regression test: round-tripping the whole file through `serde_yaml::Value`
        // used to destroy every comment and expand every anchor/alias in the file,
        // not just inside the `hooks:` block ctxlake owns — reproduced against a
        // config shaped like a real, hand-maintained one (a header comment marked
        // "DO NOT hand-edit", an inline comment, and a merge-key anchor/alias pair).
        let existing = "\
# ~/.hermes/config.yaml — managed by ops/bootstrap.sh, DO NOT hand-edit
# escalation: #platform-oncall
limits: &limits
  max_tokens: 4000 # raised after the 2026-03 incident

other-plugin:
  <<: *limits
  retries: 3
";
        let out = install(Some(existing), "myteam", "cc-01").unwrap();
        assert!(
            out.contains("# ~/.hermes/config.yaml — managed by ops/bootstrap.sh, DO NOT hand-edit"),
            "{out}"
        );
        assert!(out.contains("# escalation: #platform-oncall"), "{out}");
        assert!(
            out.contains("max_tokens: 4000 # raised after the 2026-03 incident"),
            "{out}"
        );
        assert!(out.contains("limits: &limits"), "{out}");
        assert!(out.contains("<<: *limits"), "{out}");

        // And the actual install still happened.
        let doc = parsed(&out);
        assert_eq!(
            doc["hooks"]["pre_tool_call"].as_sequence().unwrap().len(),
            1
        );
    }

    #[test]
    fn install_preserves_content_around_an_existing_hooks_block() {
        // Same guarantee, but where `hooks:` already exists and must be *replaced*
        // in place rather than appended — content both before and after it (with
        // its own anchor/alias pair) must survive untouched, and only the hooks
        // subtree itself is regenerated.
        let existing = "\
# managed by ops — see runbook
limits: &limits
  max_tokens: 4000
hooks:
  post_tool_call:
    - command: other-plugin --run
other-plugin:
  <<: *limits
";
        let out = install(Some(existing), "myteam", "cc-01").unwrap();
        assert!(out.contains("# managed by ops — see runbook"), "{out}");
        assert!(out.contains("limits: &limits"), "{out}");
        assert!(out.contains("<<: *limits"), "{out}");

        let doc = parsed(&out);
        let entries = doc["hooks"]["post_tool_call"].as_sequence().unwrap();
        assert_eq!(entries.len(), 2, "{out}");
        assert_eq!(
            doc["hooks"]["pre_tool_call"].as_sequence().unwrap().len(),
            1
        );
    }

    #[test]
    fn find_hooks_span_ignores_hooks_mentioned_only_inside_another_key() {
        // A value that merely contains the substring "hooks:" indented under some
        // other top-level key must not be mistaken for ctxlake's own block.
        let text = "other:\n  hooks: not-a-real-hooks-key\nhooks:\n  x: []\ntail: 1\n";
        let (start, end) = find_hooks_span(text).unwrap();
        assert_eq!(&text[start..end], "hooks:\n  x: []\n");
    }
}
