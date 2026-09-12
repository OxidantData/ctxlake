//! The `tools/list` catalog and `tools/call` dispatch.
//!
//! Every tool name and schema here is the entire tool surface this server exposes
//! — see the module docs on `fleet.rs` and `memory.rs` for what each one actually
//! does and, more importantly, what it honestly cannot do yet. There is
//! deliberately no `memory_write` in this list (AGENTS.md invariant 9): grep this
//! file and you will not find the string anywhere, and `protocol.rs`'s
//! `tools_list_returns_every_tool_with_a_schema` test asserts it stays that way.

use serde_json::{json, Value};

use crate::paths::Ctx;
use crate::{fleet, memory};

/// Tool execution outcome. `Params` maps to a JSON-RPC `-32602` protocol error (a
/// bad tool name or malformed/missing arguments); everything else — a claim with
/// no evidence, a claim path that doesn't exist — is a normal, expected outcome of
/// calling the tool correctly and comes back as an `isError: true` tool result
/// instead, so the MCP session keeps going rather than looking like a transport
/// failure. This mirrors `oxidant-cli::mcp`'s identical split.
enum ToolError {
    Params(String),
    Execution(String),
}

/// Execute a `tools/call` request: `{name, arguments?}`.
pub fn tools_call(params: &Value, ctx: &Ctx) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
        (
            crate::protocol::INVALID_PARAMS,
            "tools/call requires a `name` string".to_string(),
        )
    })?;
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);
    match call_tool(name, args, ctx) {
        Ok(value) => Ok(tool_result(&pretty(&value), false)),
        Err(ToolError::Execution(msg)) => Ok(tool_result(&msg, true)),
        Err(ToolError::Params(msg)) => Err((crate::protocol::INVALID_PARAMS, msg)),
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// `{"content":[{"type":"text","text":...}],"isError":...}` — the MCP tool-result
/// shape every client expects back from `tools/call`.
fn tool_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn call_tool(name: &str, args: &Value, ctx: &Ctx) -> Result<Value, ToolError> {
    match name {
        "fleet_status" => Ok(fleet::status(&ctx.cache_root, &ctx.fleet_id)),
        "fleet_claim" => {
            let paths = required_string_array(args, "paths")?;
            let reason = required_str(args, "reason")?;
            let ttl = optional_u64(args, "ttl_secs")?;
            fleet::claim(
                &ctx.spool_root,
                &ctx.fleet_id,
                &ctx.agent_id,
                &paths,
                reason,
                ttl,
            )
            .map_err(ToolError::Execution)
        }
        "fleet_release" => {
            let paths = optional_string_array(args, "paths")?;
            fleet::release(
                &ctx.spool_root,
                &ctx.fleet_id,
                &ctx.agent_id,
                paths.as_deref(),
            )
            .map_err(ToolError::Execution)
        }
        "fleet_history" => {
            let repo = optional_str(args, "repo")?;
            let since = optional_str(args, "since")?;
            Ok(fleet::history(&ctx.cache_root, &ctx.fleet_id, repo, since))
        }
        "fleet_handoff" => {
            let summary = required_str(args, "summary")?;
            let status = required_str(args, "status")?;
            let next = optional_str(args, "next")?;
            fleet::handoff(
                &ctx.spool_root,
                &ctx.fleet_id,
                &ctx.agent_id,
                summary,
                status,
                next,
            )
            .map_err(ToolError::Execution)
        }
        "memory_search" => {
            let query = required_str(args, "query")?;
            let k = optional_u64(args, "k")?.unwrap_or(10) as usize;
            Ok(memory::search(&ctx.cache_root, &ctx.fleet_id, query, k))
        }
        "memory_propose" => {
            let claim = required_str(args, "claim")?;
            let claim_type = required_str(args, "type")?;
            let evidence = required_array(args, "evidence")?;
            memory::propose(
                &ctx.spool_root,
                &ctx.fleet_id,
                &ctx.agent_id,
                claim,
                claim_type,
                &evidence,
            )
            .map_err(ToolError::Execution)
        }
        "memory_timeline" => {
            let subject = required_str(args, "subject")?;
            let since = optional_str(args, "since")?;
            Ok(memory::timeline(
                &ctx.cache_root,
                &ctx.fleet_id,
                subject,
                since,
            ))
        }
        other => Err(ToolError::Params(format!("unknown tool `{other}`"))),
    }
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ToolError::Params(format!("tool argument `{key}` must be a non-empty string"))
        })
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(ToolError::Params(format!(
            "tool argument `{key}` must be a string"
        ))),
    }
}

fn required_string_array(args: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    let arr = args.get(key).and_then(Value::as_array).ok_or_else(|| {
        ToolError::Params(format!(
            "tool argument `{key}` must be a non-empty array of strings"
        ))
    })?;
    string_array(arr, key)
}

fn optional_string_array(args: &Value, key: &str) -> Result<Option<Vec<String>>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(arr)) => Ok(Some(string_array(arr, key)?)),
        Some(_) => Err(ToolError::Params(format!(
            "tool argument `{key}` must be an array of strings"
        ))),
    }
}

fn string_array(arr: &[Value], key: &str) -> Result<Vec<String>, ToolError> {
    arr.iter()
        .map(|v| {
            v.as_str().map(str::to_string).ok_or_else(|| {
                ToolError::Params(format!("every element of `{key}` must be a string"))
            })
        })
        .collect()
}

fn required_array(args: &Value, key: &str) -> Result<Vec<Value>, ToolError> {
    args.get(key)
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| ToolError::Params(format!("tool argument `{key}` must be an array")))
}

fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| {
            ToolError::Params(format!(
                "tool argument `{key}` must be a non-negative integer"
            ))
        }),
    }
}

/// The `tools/list` catalog: name, description, and JSON Schema `inputSchema` for
/// every tool this server exposes.
pub fn tool_definitions() -> Value {
    let object_schema = |props: Value, required: &[&str]| json!({ "type": "object", "properties": props, "required": required });
    let string_prop = |desc: &str| json!({ "type": "string", "description": desc });
    let string_array_prop =
        |desc: &str| json!({ "type": "array", "items": { "type": "string" }, "description": desc });

    json!([
        {
            "name": "fleet_status",
            "description": "Who else in this fleet is active right now, and what they currently hold, as of the last local cache refresh. Never blocks on the network; an empty roster can mean either \"nobody's active\" or \"not synced yet\" and the result says which.",
            "inputSchema": object_schema(json!({}), &[]),
        },
        {
            "name": "fleet_claim",
            "description": "Request an advisory lease on one or more paths. Never touches the object store directly — queues the request to the local spool for ctxlake sync to apply as a CAS-guarded acquire. Returns once the request is durably queued, not once a lease is confirmed held. Leases are advisory even when granted: this cannot stop another agent from writing anyway.",
            "inputSchema": object_schema(
                json!({
                    "paths": string_array_prop("Repo-relative paths or globs to claim"),
                    "reason": string_prop("Short human-readable reason, shown to peers"),
                    "ttl_secs": { "type": "integer", "description": "How long the claim should live before it's considered stale (default 300s)" },
                }),
                &["paths", "reason"],
            ),
        },
        {
            "name": "fleet_release",
            "description": "Release a previously claimed lease. Omit `paths` to request release of everything this agent currently holds.",
            "inputSchema": object_schema(
                json!({ "paths": string_array_prop("Paths to release; omit for all of this agent's claims") }),
                &[],
            ),
        },
        {
            "name": "fleet_history",
            "description": "Recent sessions and their outcomes from the local cache, optionally filtered by repo and a since timestamp. Returns an honest empty result if no history has synced locally yet.",
            "inputSchema": object_schema(
                json!({
                    "repo": string_prop("Filter to sessions against this repo"),
                    "since": string_prop("RFC3339 timestamp; only sessions ending at or after this"),
                }),
                &[],
            ),
        },
        {
            "name": "fleet_handoff",
            "description": "Write a handoff note for whoever picks up this repo next: a summary, a status, and an optional next-step. Queued to the local spool; never written directly to the lake.",
            "inputSchema": object_schema(
                json!({
                    "summary": string_prop("What happened this session"),
                    "status": string_prop("e.g. done, blocked, in-progress"),
                    "next": string_prop("Suggested next step for whoever reads this"),
                }),
                &["summary", "status"],
            ),
        },
        {
            "name": "memory_search",
            "description": "Search promoted claims (with attribution: observer, date, independent session count, confidence, and a CONTESTED marker) for the given query. The belief layer is not enabled until the promotion gate ships (Wave 3); until then this honestly returns enabled: false with no fabricated results.",
            "inputSchema": object_schema(
                json!({
                    "query": string_prop("Free-text query, matched against claim text and subject"),
                    "scope": string_prop("agent | repo | fleet (accepted, not yet enforced — no promoted claim exists yet)"),
                    "k": { "type": "integer", "description": "Maximum results to return (default 10)" },
                }),
                &["query"],
            ),
        },
        {
            "name": "memory_propose",
            "description": "Propose a candidate claim with evidence. This NEVER writes a promoted claim — only the promotion gate can do that, and it doesn't exist yet. A claim with no evidence is rejected outright, per the no-evidence-no-claim rule.",
            "inputSchema": object_schema(
                json!({
                    "claim": string_prop("One proposition — not a paragraph, not two facts joined by \"and\""),
                    "type": string_prop("environment | convention | outcome | preference | hypothesis"),
                    "evidence": { "type": "array", "description": "At least one citation, e.g. {session_id, message_id}; empty arrays are rejected" },
                }),
                &["claim", "type", "evidence"],
            ),
        },
        {
            "name": "memory_timeline",
            "description": "What this fleet has actually tried regarding a subject, from the local cache. Honestly empty until a timeline has been synced locally.",
            "inputSchema": object_schema(
                json!({
                    "subject": string_prop("Subject to look up, e.g. a repo, a service, a command"),
                    "since": string_prop("RFC3339 timestamp; only entries at or after this"),
                }),
                &["subject"],
            ),
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            spool_root: dir.path().join("spool"),
            cache_root: dir.path().join("cache"),
            fleet_id: "test-fleet".to_string(),
            agent_id: "test-agent".to_string(),
        };
        (dir, ctx)
    }

    #[test]
    fn every_tool_definition_has_a_matching_dispatch_arm() {
        let (_dir, ctx) = test_ctx();
        let defs = tool_definitions();
        for def in defs.as_array().unwrap() {
            let name = def["name"].as_str().unwrap();
            // Call with empty arguments: every tool either succeeds or returns an
            // execution error (isError: true) for missing required fields it
            // validates itself, but must never be rejected as `unknown tool`,
            // which would mean tools/list and tools/call disagree about the
            // tool's existence.
            let result = call_tool(name, &json!({}), &ctx);
            let is_unknown_tool = matches!(
                &result,
                Err(ToolError::Params(msg)) if msg.starts_with("unknown tool")
            );
            assert!(!is_unknown_tool, "{name} is listed but not dispatchable");
        }
    }

    #[test]
    fn fleet_claim_with_no_paths_is_a_params_error() {
        let (_dir, ctx) = test_ctx();
        let err = call_tool("fleet_claim", &json!({"reason": "x"}), &ctx);
        assert!(matches!(err, Err(ToolError::Params(_))));
    }

    #[test]
    fn memory_propose_with_empty_evidence_is_an_execution_error_not_a_params_error() {
        // Calling the tool correctly (right shape, right types) with evidence
        // that fails the no-evidence-no-claim rule is a normal outcome of using
        // the tool, not a malformed call — it must come back as isError: true via
        // tools_call, not a JSON-RPC protocol error.
        let (_dir, ctx) = test_ctx();
        let result = call_tool(
            "memory_propose",
            &json!({"claim": "x", "type": "convention", "evidence": []}),
            &ctx,
        );
        assert!(matches!(result, Err(ToolError::Execution(_))));
    }

    #[test]
    fn tools_call_wraps_execution_errors_as_error_content_not_protocol_errors() {
        let (_dir, ctx) = test_ctx();
        let result = tools_call(
            &json!({"name": "memory_propose", "arguments": {"claim": "x", "type": "convention", "evidence": []}}),
            &ctx,
        )
        .unwrap();
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no evidence"));
    }

    #[test]
    fn tools_call_succeeds_for_a_well_formed_fleet_claim() {
        let (_dir, ctx) = test_ctx();
        let result = tools_call(
            &json!({"name": "fleet_claim", "arguments": {"paths": ["crates/foo/**"], "reason": "refactor"}}),
            &ctx,
        )
        .unwrap();
        assert_eq!(result["isError"], false);
    }
}
