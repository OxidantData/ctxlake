//! Wiring `ctxlake mcp` into a runtime's MCP server registry.
//!
//! **Why this exists.** Everything the memory layer produces was reachable two ways:
//! pushed into a session as a briefing, or pulled by an agent calling `memory_search`
//! / `fleet_history`. Only the push half was ever wired. `ctxlake mcp` has been a real
//! subcommand serving six working tools, `main.rs`'s own doc comment says "install
//! writes this command into each runtime's MCP config" — and no code anywhere did.
//! On a live machine `mcpServers` was empty, so an agent had no way to ask the lake
//! anything. 38 promoted claims sat there, unreachable by the thing they were for.
//!
//! **The file this touches is the most dangerous one ctxlake writes.** Claude Code's
//! `~/.claude.json` is 112 KB of live state on a real machine — 116 top-level keys, 45
//! project records, onboarding flags, conversation history. The merge-never-clobber
//! rule that protects a 31 KB `settings.json` applies here at three and a half times
//! the stakes, so this module touches exactly one key: `mcpServers.ctxlake`.
//!
//! Deliberately **not** wired for Hermes. Its MCP configuration lives in
//! `config.yaml`, a different format under a different schema, and no live Hermes
//! install has been available to verify the shape against — which is precisely how
//! the `tool_result` bug was born. `docs/runtimes.md` records the gap instead.

use anyhow::Result;
use json::JsonValue;

use super::json_util::{ensure_object, parse_or_empty, remove_if_empty};

/// The registry key ctxlake owns. Nothing else in the file is ever read or written.
pub const SERVER_NAME: &str = "ctxlake";

/// The `mcpServers` entry for this host.
///
/// `fleet_id` and `agent_id` ride in `env` rather than in `args` because the MCP
/// server resolves them exactly the way the hook does — from the environment — and a
/// server started by a runtime inherits none of the shell's.
fn server_entry(fleet_id: &str, agent_id: &str) -> JsonValue {
    let mut env = JsonValue::new_object();
    env["CTXLAKE_FLEET_ID"] = fleet_id.into();
    env["CTXLAKE_AGENT_ID"] = agent_id.into();

    let mut args = JsonValue::new_array();
    let _ = args.push("mcp");

    let mut entry = JsonValue::new_object();
    entry["command"] = "ctxlake".into();
    entry["args"] = args;
    entry["env"] = env;
    entry
}

/// Merge ctxlake's server entry into `existing`, leaving every other key untouched.
///
/// Idempotent by construction: the entry is rebuilt and assigned, so re-running after
/// a fleet or agent rename converges rather than accumulating.
pub fn install(existing: Option<&str>, fleet_id: &str, agent_id: &str) -> Result<String> {
    let mut doc = parse_or_empty(existing)?;
    let servers = ensure_object(&mut doc, "mcpServers")?;
    servers[SERVER_NAME] = server_entry(fleet_id, agent_id);
    Ok(doc.pretty(2) + "\n")
}

/// Remove exactly what [`install`] added.
pub fn uninstall(existing: Option<&str>) -> Result<String> {
    let mut doc = parse_or_empty(existing)?;
    if doc["mcpServers"].is_object() {
        let servers = ensure_object(&mut doc, "mcpServers")?;
        servers.remove(SERVER_NAME);
    }
    // Only when ctxlake created it: an empty `mcpServers` the user had before is
    // theirs, and removing it would be a change they did not ask for.
    remove_if_empty(&mut doc, "mcpServers");
    Ok(doc.pretty(2) + "\n")
}

/// Whether ctxlake's entry is present.
pub fn is_wired(text: &str) -> bool {
    parse_or_empty(Some(text))
        .map(|d| d["mcpServers"][SERVER_NAME].is_object())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed stand-in for the real 112 KB file: the point is that none of it is
    /// ctxlake's and all of it must survive.
    const REAL_SHAPED: &str = r#"{
      "numStartups": 412,
      "installMethod": "brew",
      "mcpServers": {"orca": {"command": "orca", "args": ["serve"]}},
      "projects": {"/Users/alice/work": {"allowedTools": ["Bash"], "history": [{"display": "x"}]}},
      "oauthAccount": {"emailAddress": "alice@example.com"}
    }"#;

    #[test]
    fn installing_touches_nothing_but_our_own_entry() {
        // `~/.claude.json` is the largest and most load-bearing file ctxlake writes.
        // Losing a project record or an auth block to a careless merge would be worse
        // than never wiring MCP at all.
        let out = install(Some(REAL_SHAPED), "myteam", "cc-01").unwrap();
        let d = json::parse(&out).unwrap();

        assert_eq!(d["numStartups"], 412);
        assert_eq!(d["installMethod"], "brew");
        assert_eq!(d["oauthAccount"]["emailAddress"], "alice@example.com");
        assert_eq!(
            d["projects"]["/Users/alice/work"]["allowedTools"][0],
            "Bash"
        );
        assert_eq!(
            d["mcpServers"]["orca"]["command"], "orca",
            "another tool's server must survive"
        );

        assert_eq!(d["mcpServers"]["ctxlake"]["command"], "ctxlake");
        assert_eq!(d["mcpServers"]["ctxlake"]["args"][0], "mcp");
        assert_eq!(
            d["mcpServers"]["ctxlake"]["env"]["CTXLAKE_FLEET_ID"],
            "myteam"
        );
        assert_eq!(
            d["mcpServers"]["ctxlake"]["env"]["CTXLAKE_AGENT_ID"],
            "cc-01"
        );
    }

    #[test]
    fn installing_twice_changes_nothing() {
        let once = install(Some(REAL_SHAPED), "myteam", "cc-01").unwrap();
        let twice = install(Some(&once), "myteam", "cc-01").unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn renaming_this_host_converges_rather_than_accumulating() {
        let a = install(Some(REAL_SHAPED), "myteam", "cc-01").unwrap();
        let b = install(Some(&a), "otherteam", "cc-02").unwrap();
        let d = json::parse(&b).unwrap();
        assert_eq!(
            d["mcpServers"]["ctxlake"]["env"]["CTXLAKE_FLEET_ID"],
            "otherteam"
        );
        assert_eq!(
            d["mcpServers"]["ctxlake"]["env"]["CTXLAKE_AGENT_ID"],
            "cc-02"
        );
        assert_eq!(
            d["mcpServers"]["ctxlake"]["args"].len(),
            1,
            "args must not grow"
        );
    }

    #[test]
    fn uninstall_removes_ours_and_only_ours() {
        let installed = install(Some(REAL_SHAPED), "myteam", "cc-01").unwrap();
        let out = uninstall(Some(&installed)).unwrap();
        let d = json::parse(&out).unwrap();
        assert!(!d["mcpServers"]["ctxlake"].is_object());
        assert_eq!(d["mcpServers"]["orca"]["command"], "orca");
        assert_eq!(d["numStartups"], 412);
    }

    #[test]
    fn uninstall_on_a_never_installed_file_is_a_no_op() {
        let out = uninstall(Some(REAL_SHAPED)).unwrap();
        assert_eq!(
            json::parse(&out).unwrap()["mcpServers"]["orca"]["command"],
            "orca"
        );
    }

    #[test]
    fn an_absent_file_gets_a_valid_minimal_config() {
        let out = install(None, "myteam", "cc-01").unwrap();
        let d = json::parse(&out).unwrap();
        assert_eq!(d["mcpServers"]["ctxlake"]["command"], "ctxlake");
        assert!(is_wired(&out));
    }

    #[test]
    fn malformed_existing_json_is_refused_not_guessed_at() {
        // Overwriting 112 KB of someone's live state because it failed to parse is
        // the worst possible response to that failure.
        assert!(install(Some("{not json"), "myteam", "cc-01").is_err());
    }
}
