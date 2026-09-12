//! Cursor — `~/.cursor/hooks.json`, schema version 1.
//!
//! Flatter than Claude Code's: `hooks.<event>` is directly an array of
//! `{"command": "...", "timeout"?: N}` entries, no matcher-group nesting
//! (docs/runtimes/cursor.md).

use anyhow::Result;
use json::JsonValue;

use super::json_util::{ensure_array, ensure_object, parse_or_empty, remove_if_empty};
use super::{hook_command, is_ours, Runtime};

/// The six events docs/runtimes/cursor.md documents ctxlake against. Cursor's hook
/// surface has several more (the pre-check-only and subagent events cursor.rs's
/// adapter also recognizes), left uninstalled here because the doc page — the
/// contract this installer has to match — only commits to these six.
pub const EVENTS: &[&str] = &[
    "beforeSubmitPrompt",
    "beforeShellExecution",
    "beforeReadFile",
    "beforeMCPExecution",
    "afterFileEdit",
    "stop",
];

fn strip_all(doc: &mut JsonValue, event: &str) -> Result<()> {
    if !doc["hooks"].has_key(event) {
        return Ok(());
    }
    let hooks = ensure_object(doc, "hooks")?;
    let arr = ensure_array(hooks, event)?;
    if let JsonValue::Array(entries) = arr {
        entries.retain(|e| !e["command"].as_str().is_some_and(is_ours));
    }
    remove_if_empty(hooks, event);
    Ok(())
}

fn add(doc: &mut JsonValue, event: &str, fleet_id: &str, agent_id: &str) -> Result<()> {
    strip_all(doc, event)?;
    let hooks = ensure_object(doc, "hooks")?;
    let arr = ensure_array(hooks, event)?;
    let cmd = hook_command(fleet_id, agent_id, Runtime::Cursor, event);
    arr.push(json::object! { "command" => cmd })?;
    Ok(())
}

pub fn install(existing: Option<&str>, fleet_id: &str, agent_id: &str) -> Result<String> {
    let mut doc = parse_or_empty(existing)?;
    if !doc.has_key("version") {
        doc["version"] = 1.into();
    }
    for event in EVENTS {
        add(&mut doc, event, fleet_id, agent_id)?;
    }
    remove_if_empty(&mut doc, "hooks");
    Ok(doc.pretty(2) + "\n")
}

pub fn uninstall(existing: Option<&str>) -> Result<String> {
    let mut doc = parse_or_empty(existing)?;
    for event in EVENTS {
        strip_all(&mut doc, event)?;
    }
    remove_if_empty(&mut doc, "hooks");
    Ok(doc.pretty(2) + "\n")
}

/// See `claude_code::count_entries` — same purpose, flat shape.
pub fn count_entries(text: &str) -> usize {
    let Ok(doc) = parse_or_empty(Some(text)) else {
        return 0;
    };
    EVENTS
        .iter()
        .map(|event| match &doc["hooks"][*event] {
            JsonValue::Array(entries) => entries.len(),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_on_an_absent_file_sets_version_and_every_event() {
        let out = install(None, "myteam", "cc-01").unwrap();
        let doc = json::parse(&out).unwrap();
        assert_eq!(doc["version"], 1);
        for event in EVENTS {
            assert_eq!(doc["hooks"][*event].len(), 1, "event {event}: {out}");
        }
    }

    #[test]
    fn install_preserves_an_existing_version_number() {
        let existing = r#"{"version": 1, "hooks": {}}"#;
        let out = install(Some(existing), "myteam", "cc-01").unwrap();
        let doc = json::parse(&out).unwrap();
        assert_eq!(doc["version"], 1);
    }

    #[test]
    fn install_preserves_another_tools_entry_on_the_same_event() {
        let existing = json::object! {
            "version" => 1,
            "hooks" => json::object! {
                "afterFileEdit" => json::array![
                    json::object! { "command" => "OTHER_TOOL_ENV=1 /usr/local/bin/other-tool --watch" }
                ]
            }
        }
        .dump();
        let out = install(Some(&existing), "myteam", "cc-01").unwrap();
        let doc = json::parse(&out).unwrap();
        let entries = &doc["hooks"]["afterFileEdit"];
        assert_eq!(entries.len(), 2, "{out}");
        assert_eq!(
            entries[0]["command"],
            "OTHER_TOOL_ENV=1 /usr/local/bin/other-tool --watch"
        );
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
        let existing = json::object! {
            "version" => 1,
            "hooks" => json::object! {
                "afterFileEdit" => json::array![
                    json::object! { "command" => "other-tool" }
                ]
            }
        }
        .dump();
        let installed = install(Some(&existing), "myteam", "cc-01").unwrap();
        let uninstalled = uninstall(Some(&installed)).unwrap();
        let doc = json::parse(&uninstalled).unwrap();
        assert_eq!(doc["hooks"]["afterFileEdit"].len(), 1);
        assert_eq!(doc["hooks"]["afterFileEdit"][0]["command"], "other-tool");
        // Every other event ctxlake touched had nothing else in it, so it must be
        // gone entirely, not left behind as an empty array.
        for event in EVENTS.iter().filter(|e| **e != "afterFileEdit") {
            assert!(
                !doc["hooks"].has_key(event),
                "{event} should be gone: {uninstalled}"
            );
        }
    }

    #[test]
    fn malformed_existing_json_is_refused() {
        let err = install(Some("not json at all"), "myteam", "cc-01").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }
}
