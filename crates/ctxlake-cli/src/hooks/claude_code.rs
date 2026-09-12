//! Claude Code — `~/.claude/settings.json`, `hooks` object.
//!
//! Shape (docs/runtimes/claude-code.md, AGENTS.md): each event name maps to an array
//! of *matcher groups* — `{"matcher"?: "...", "hooks": [{"type": "command",
//! "command": "..."}]}` — so more than one tool can register on the same event
//! without one clobbering another's group. ctxlake always appends its own
//! standalone group with no `matcher`, meaning "run for every tool," rather than
//! trying to fold its command into someone else's group.

use anyhow::Result;
use json::JsonValue;

use super::json_util::{ensure_array, ensure_object, parse_or_empty, remove_if_empty};
use super::{hook_command, is_ours, Runtime};

/// Every Claude Code hook event ctxlake wires up — the full mapping table in
/// docs/runtimes/claude-code.md, `Notification` excluded (documented there as
/// carrying nothing the envelope schema holds).
pub const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PreCompact",
    "Stop",
    "SubagentStop",
    "SessionEnd",
];

fn command_of(entry: &JsonValue) -> Option<&str> {
    entry["command"].as_str()
}

/// Strip any ctxlake-authored command from one matcher group's inner `hooks` array.
/// Returns `false` if the group should be dropped entirely (its inner array is now
/// empty) and `true` if it should be kept, whether or not anything changed.
fn strip_group(group: &mut JsonValue) -> bool {
    let JsonValue::Object(obj) = group else {
        return true; // not shaped like a matcher group; not ours to touch
    };
    let Some(JsonValue::Array(inner)) = obj.get_mut("hooks") else {
        return true;
    };
    inner.retain(|entry| !command_of(entry).is_some_and(is_ours));
    !inner.is_empty()
}

fn strip_all(doc: &mut JsonValue, event: &str) -> Result<()> {
    if !doc["hooks"].has_key(event) {
        return Ok(());
    }
    let hooks = ensure_object(doc, "hooks")?;
    let arr = ensure_array(hooks, event)?;
    if let JsonValue::Array(groups) = arr {
        groups.retain_mut(strip_group);
    }
    remove_if_empty(hooks, event);
    Ok(())
}

fn add(doc: &mut JsonValue, event: &str, fleet_id: &str, agent_id: &str) -> Result<()> {
    strip_all(doc, event)?;
    let hooks = ensure_object(doc, "hooks")?;
    let arr = ensure_array(hooks, event)?;
    let cmd = hook_command(fleet_id, agent_id, Runtime::ClaudeCode, event);

    // Built imperatively, not via the `object!`/`array!` macros: those macros
    // pre-count elements by taking `{ let _ = &$i; 1 }` on each one before pushing
    // it, which for a *nested* macro literal means the whole inner expression gets
    // textually expanded twice. That is harmless for a `Copy` literal but moves
    // `cmd` (a `String`) on the first, throwaway expansion and fails to compile on
    // the second — `.push()` as a plain method call evaluates its argument once.
    let mut command_entry = JsonValue::new_object();
    command_entry["type"] = "command".into();
    command_entry["command"] = cmd.into();

    let mut inner = JsonValue::new_array();
    inner.push(command_entry)?;

    let mut group = JsonValue::new_object();
    group["hooks"] = inner;

    arr.push(group)?;
    Ok(())
}

/// Compute the full new file content after merging ctxlake's hooks into `existing`
/// (or starting fresh if `None`/blank). Never touches any key outside `hooks`.
pub fn install(existing: Option<&str>, fleet_id: &str, agent_id: &str) -> Result<String> {
    let mut doc = parse_or_empty(existing)?;
    for event in EVENTS {
        add(&mut doc, event, fleet_id, agent_id)?;
    }
    remove_if_empty(&mut doc, "hooks");
    Ok(doc.pretty(2) + "\n")
}

/// Remove exactly what [`install`] would have added, leaving everything else —
/// including other tools' matcher groups on the same events — untouched.
pub fn uninstall(existing: Option<&str>) -> Result<String> {
    let mut doc = parse_or_empty(existing)?;
    for event in EVENTS {
        strip_all(&mut doc, event)?;
    }
    remove_if_empty(&mut doc, "hooks");
    Ok(doc.pretty(2) + "\n")
}

/// Total hook-command entries across every event ctxlake manages, regardless of who
/// put them there. `hooks/mod.rs::detect` calls this on the *uninstalled* form of a
/// config to count what belongs to other tools.
pub fn count_entries(text: &str) -> usize {
    let Ok(doc) = parse_or_empty(Some(text)) else {
        return 0;
    };
    EVENTS
        .iter()
        .map(|event| {
            let JsonValue::Array(groups) = &doc["hooks"][*event] else {
                return 0;
            };
            groups
                .iter()
                .filter_map(|g| match &g["hooks"] {
                    JsonValue::Array(inner) => Some(inner.len()),
                    _ => None,
                })
                .sum::<usize>()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_on_an_absent_file_creates_every_event() {
        let out = install(None, "myteam", "cc-01").unwrap();
        let doc = json::parse(&out).unwrap();
        for event in EVENTS {
            assert!(
                doc["hooks"][*event].len() == 1,
                "expected exactly one group for {event}: {out}"
            );
        }
    }

    #[test]
    fn install_preserves_an_unrelated_top_level_key() {
        let existing =
            r#"{"model": "opus", "statusLine": {"type": "command", "command": "my-status"}}"#;
        let out = install(Some(existing), "myteam", "cc-01").unwrap();
        let doc = json::parse(&out).unwrap();
        assert_eq!(doc["model"], "opus");
        assert_eq!(doc["statusLine"]["command"], "my-status");
    }

    #[test]
    fn install_preserves_another_tools_matcher_group_on_the_same_event() {
        let existing = json::object! {
            "hooks" => json::object! {
                "PostToolUse" => json::array![
                    json::object! {
                        "matcher" => "Edit|Write",
                        "hooks" => json::array![
                            json::object! { "type" => "command", "command" => "some-other-tool --flag" }
                        ]
                    }
                ]
            }
        }
        .dump();
        let out = install(Some(&existing), "myteam", "cc-01").unwrap();
        let doc = json::parse(&out).unwrap();
        let groups = &doc["hooks"]["PostToolUse"];
        assert_eq!(groups.len(), 2, "the foreign group plus ours: {out}");
        assert_eq!(groups[0]["hooks"][0]["command"], "some-other-tool --flag");
        assert!(groups[1]["hooks"][0]["command"]
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
    fn install_converges_when_fleet_or_agent_changes() {
        let first = install(None, "myteam", "cc-01").unwrap();
        let second = install(Some(&first), "otherfleet", "cc-02").unwrap();
        let doc = json::parse(&second).unwrap();
        assert_eq!(
            doc["hooks"]["PostToolUse"].len(),
            1,
            "must replace, not accumulate, ctxlake's own stale entry: {second}"
        );
        assert!(doc["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("otherfleet"));
    }

    #[test]
    fn uninstall_removes_only_ctxlakes_entries() {
        let existing = json::object! {
            "hooks" => json::object! {
                "PostToolUse" => json::array![
                    json::object! {
                        "hooks" => json::array![
                            json::object! { "type" => "command", "command" => "some-other-tool --flag" }
                        ]
                    }
                ]
            }
        }
        .dump();
        let installed = install(Some(&existing), "myteam", "cc-01").unwrap();
        let uninstalled = uninstall(Some(&installed)).unwrap();
        let doc = json::parse(&uninstalled).unwrap();
        assert_eq!(
            doc["hooks"]["PostToolUse"].len(),
            1,
            "kept the foreign group: {uninstalled}"
        );
        assert_eq!(
            doc["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "some-other-tool --flag"
        );
    }

    #[test]
    fn uninstall_drops_an_event_key_it_created_from_nothing() {
        let installed = install(None, "myteam", "cc-01").unwrap();
        let uninstalled = uninstall(Some(&installed)).unwrap();
        let doc = json::parse(&uninstalled).unwrap();
        assert!(
            !doc.has_key("hooks"),
            "no trace of an install should remain: {uninstalled}"
        );
    }

    #[test]
    fn uninstall_on_a_never_installed_file_is_a_no_op() {
        let existing = r#"{"model": "opus"}"#;
        let out = uninstall(Some(existing)).unwrap();
        let doc = json::parse(&out).unwrap();
        assert_eq!(doc["model"], "opus");
        assert!(!doc.has_key("hooks"));
    }

    #[test]
    fn malformed_existing_json_is_refused_not_guessed_at() {
        let err = install(Some("{ this is not json"), "myteam", "cc-01").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }
}
