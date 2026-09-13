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
        "fleet_history" => {
            let repo = optional_str(args, "repo")?;
            let since = optional_str(args, "since")?;
            let limit =
                optional_u64(args, "limit")?.unwrap_or(fleet::DEFAULT_ROW_LIMIT as u64) as usize;
            Ok(fleet::history(
                &ctx.cache_root,
                &ctx.fleet_id,
                repo,
                since,
                limit,
            ))
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
            let subject = optional_str(args, "subject")?;
            let claim_type = optional_str(args, "claim_type")?;
            let scope = optional_str(args, "scope")?;
            let k = optional_u64(args, "k")?.unwrap_or(10) as usize;
            Ok(memory::search(
                &ctx.cache_root,
                &ctx.fleet_id,
                query,
                subject,
                claim_type,
                scope,
                k,
            ))
        }
        "memory_propose" => {
            let claim = required_str(args, "claim")?;
            let claim_type = required_str(args, "type")?;
            let subject = required_str(args, "subject")?;
            let evidence = required_array(args, "evidence")?;
            memory::propose(
                &ctx.spool_root,
                &ctx.fleet_id,
                &ctx.agent_id,
                claim,
                claim_type,
                subject,
                &evidence,
            )
            .map_err(ToolError::Execution)
        }
        "memory_timeline" => {
            let subject = required_str(args, "subject")?;
            let since = optional_str(args, "since")?;
            let limit =
                optional_u64(args, "limit")?.unwrap_or(memory::DEFAULT_ROW_LIMIT as u64) as usize;
            Ok(memory::timeline(
                &ctx.cache_root,
                &ctx.fleet_id,
                subject,
                since,
                limit,
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

    json!([
        {
            "name": "fleet_status",
            "description": "Who else in this fleet is active right now, and what they're doing, as of the last local cache refresh. Never blocks on the network; an empty roster can mean either \"nobody's active\" or \"not synced yet\" and the result says which.",
            "inputSchema": object_schema(json!({}), &[]),
        },
        {
            "name": "fleet_history",
            "description": "Recent sessions and their outcomes from the local cache, optionally filtered by repo and a since timestamp. Returns an honest empty result if no history has synced locally yet. Rows are capped (default 50, hard ceiling 200) regardless of how much history matches; `truncated: true` in the result means more rows existed than were returned.",
            "inputSchema": object_schema(
                json!({
                    "repo": string_prop("Filter to sessions against this repo"),
                    "since": string_prop("RFC3339 timestamp; only sessions ending at or after this"),
                    "limit": { "type": "integer", "description": "Maximum sessions to return (default 50, hard ceiling 200)" },
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
            "description": "Search promoted claims from the local snapshot mirror (FTS5 lexical match plus brute-force cosine over each claim's 256-dim embedding), rendered with attribution: observer, date, INDEPENDENT session count (never the raw evidence count), confidence, and a CONTESTED marker when contested. Rendered as a peer's belief to verify, never as bare fact. Honestly returns enabled: false with no fabricated results whenever no snapshot has synced locally yet, or this fleet is in shadow mode (the default) — reading zero claims in that case is deliberate, not a bug.",
            "inputSchema": object_schema(
                json!({
                    "query": string_prop("Free-text query, matched against claim text and subject"),
                    "scope": string_prop("agent | repo | fleet — exact match filter"),
                    "subject": string_prop("Exact-match filter on the claim's subject"),
                    "claim_type": string_prop("environment | convention | outcome | preference | hypothesis — exact match filter"),
                    "k": { "type": "integer", "description": "Maximum results to return (default 10, hard ceiling 200)" },
                }),
                &["query"],
            ),
        },
        {
            "name": "memory_propose",
            "description": "Propose a candidate claim with a subject and evidence. This NEVER writes a promoted claim, and never writes anything beyond agent scope — only the promotion gate (ctxlake maint) can promote. A claim with no evidence, or an evidence citation missing session_id/message_id, is rejected outright, per the no-evidence-no-claim rule.",
            "inputSchema": object_schema(
                json!({
                    "claim": string_prop("One proposition — not a paragraph, not two facts joined by \"and\""),
                    "type": string_prop("environment | convention | outcome | preference | hypothesis"),
                    "subject": string_prop("The entity this is about — a repo, a service, a command"),
                    "evidence": { "type": "array", "description": "At least one citation, each an object with at least {session_id, message_id}; excerpt_hash/observed_at optional. Empty arrays, or citations missing session_id/message_id, are rejected." },
                }),
                &["claim", "type", "subject", "evidence"],
            ),
        },
        {
            "name": "memory_timeline",
            "description": "What this fleet has actually tried regarding a subject, from the local cache. Honestly empty until a timeline has been synced locally. Rows are capped (default 50, hard ceiling 200); `truncated: true` in the result means more entries matched than were returned.",
            "inputSchema": object_schema(
                json!({
                    "subject": string_prop("Subject to look up, e.g. a repo, a service, a command"),
                    "since": string_prop("RFC3339 timestamp; only entries at or after this"),
                    "limit": { "type": "integer", "description": "Maximum entries to return (default 50, hard ceiling 200)" },
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
    fn memory_propose_with_empty_evidence_is_an_execution_error_not_a_params_error() {
        // Calling the tool correctly (right shape, right types) with evidence
        // that fails the no-evidence-no-claim rule is a normal outcome of using
        // the tool, not a malformed call — it must come back as isError: true via
        // tools_call, not a JSON-RPC protocol error.
        let (_dir, ctx) = test_ctx();
        let result = call_tool(
            "memory_propose",
            &json!({"claim": "x", "type": "convention", "subject": "tooling", "evidence": []}),
            &ctx,
        );
        assert!(matches!(result, Err(ToolError::Execution(_))));
    }

    #[test]
    fn tools_call_wraps_execution_errors_as_error_content_not_protocol_errors() {
        let (_dir, ctx) = test_ctx();
        let result = tools_call(
            &json!({"name": "memory_propose", "arguments": {"claim": "x", "type": "convention", "subject": "tooling", "evidence": []}}),
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
    fn tools_call_succeeds_for_a_well_formed_fleet_status() {
        let (_dir, ctx) = test_ctx();
        let result = tools_call(&json!({"name": "fleet_status", "arguments": {}}), &ctx).unwrap();
        assert_eq!(result["isError"], false);
    }

    #[test]
    fn no_tool_definition_advertises_claiming_a_resource() {
        // Regression for the lease removal: nothing in the tool catalog should
        // read as an invitation to reserve a path or resource any more.
        let defs = tool_definitions();
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"fleet_claim"));
        assert!(!names.contains(&"fleet_release"));
        let catalog = serde_json::to_string(&defs).unwrap().to_lowercase();
        assert!(!catalog.contains("lease"), "{catalog}");
    }

    /// Regression: `fleet_history`'s `limit` argument must actually reach
    /// `fleet::history` through dispatch, not just exist in the schema.
    #[test]
    fn fleet_history_limit_argument_reaches_the_cache_read() {
        let (dir, ctx) = test_ctx();
        let fleet_dir = dir.path().join("cache").join("test-fleet");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        let sessions: Vec<_> = (0..10)
            .map(|i| {
                let id: &'static str = Box::leak(format!("sess-{i}").into_boxed_str());
                crate::snapshot::test_support::FixtureSession::new(id, "s")
            })
            .collect();
        crate::snapshot::test_support::write_snapshot_with(
            &fleet_dir.join("snapshot.bin"),
            &[],
            &sessions,
        );

        let result = tools_call(
            &json!({"name": "fleet_history", "arguments": {"limit": 2}}),
            &ctx,
        )
        .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["truncated"], true);
    }

    /// Same regression for `memory_timeline`'s `limit` argument, against a real
    /// snapshot fixture now that `memory_timeline` reads `snapshot.bin` instead of
    /// a placeholder `timeline.json`.
    #[test]
    fn memory_timeline_limit_argument_reaches_the_cache_read() {
        use crate::snapshot::test_support::{write_snapshot, FixtureClaim};

        let (dir, ctx) = test_ctx();
        let path = dir
            .path()
            .join("cache")
            .join("test-fleet")
            .join("snapshot.bin");
        let claims: Vec<FixtureClaim> = (0..10)
            .map(|i| {
                let id: &'static str = Box::leak(format!("c{i}").into_boxed_str());
                let at: &'static str =
                    Box::leak(format!("2026-09-{:02}T00:00:00Z", i + 1).into_boxed_str());
                let mut c = FixtureClaim::promoted(id, "outcome text", "outcome", "x");
                c.updated_at = at;
                c
            })
            .collect();
        write_snapshot(&path, &claims);

        let result = tools_call(
            &json!({"name": "memory_timeline", "arguments": {"subject": "x", "limit": 2}}),
            &ctx,
        )
        .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["entries"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["truncated"], true);
    }
}
