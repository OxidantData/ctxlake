//! JSON-RPC 2.0 framing for MCP protocol revision 2024-11-05.
//!
//! Framing is newline-delimited JSON on stdin/stdout: one message per line, one
//! response line per request, notifications (no `id`) never answered. The wire
//! surface is `initialize`, `tools/list`, and `tools/call` — three methods — so
//! this is hand-rolled with `serde_json` rather than pulling in an MCP framework,
//! matching the house pattern in `oxidant-cli::mcp`.
//!
//! **stdout carries protocol frames only.** Every diagnostic in this crate goes to
//! `stderr` — see `lib.rs`'s `run_stdio`. A stray `println!` anywhere on this path
//! would interleave arbitrary text into a stream the client parses one line at a
//! time as JSON, corrupting every frame after it in a way that is hard to diagnose
//! from the client side, which is exactly the failure this comment exists to
//! prevent someone from reintroducing.
//!
//! Every request is dispatched against a [`Ctx`](crate::paths::Ctx) — the local
//! spool/cache roots and this fleet/agent's identity — read from the environment
//! once at process start and passed in by the caller, rather than re-read from
//! `std::env` per line. That is what lets tests exercise the full JSON-RPC path
//! for a write-shaped tool against an isolated tempdir instead of racing every
//! other test in this binary over a shared process-global env var.

use serde_json::{json, Value};

use crate::paths::Ctx;

/// MCP protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

// JSON-RPC 2.0 error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// Handle one input line. Returns the response frame, or `None` for notifications
/// and stray responses, which must never be answered. Malformed JSON still
/// produces a `-32700` response (id `null`) and the caller keeps serving — one bad
/// line must never kill the server, and this function itself never panics: every
/// dispatch is additionally wrapped in `catch_unwind` so a bug inside a tool
/// handler surfaces as a JSON-RPC internal error, not a dropped connection.
pub fn handle_line(line: &str, ctx: &Ctx) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                PARSE_ERROR,
                &format!("parse error: {e}"),
            ));
        }
    };
    if !msg.is_object() {
        return Some(error_response(
            Value::Null,
            INVALID_REQUEST,
            "request must be a JSON object",
        ));
    }
    // A stray response (result/error, no method) is not ours to answer.
    if msg.get("method").is_none() && (msg.get("result").is_some() || msg.get("error").is_some()) {
        return None;
    }
    let id = msg.get("id").cloned();
    let method = match msg.get("method").and_then(Value::as_str) {
        Some(m) => m,
        None => {
            return id.map(|id| error_response(id, INVALID_REQUEST, "missing `method` field"));
        }
    };
    // No id => notification (`notifications/initialized`, `notifications/cancelled`,
    // ...): never answered, even when the method itself is unknown.
    let id = id?;

    let empty = Value::Null;
    let params = msg.get("params").unwrap_or(&empty);
    let method_owned = method.to_string();
    let params_owned = params.clone();
    let ctx_owned = ctx.clone();

    // Defense in depth against the "never panic" requirement: nothing in this
    // crate's tool handlers is expected to panic (they route untrusted input
    // through `.get`/`.as_str` rather than indexing or unwrapping), but a caller
    // waiting on stdio should never find out the hard way that an edge case was
    // missed. A caught panic here becomes an ordinary JSON-RPC error frame.
    let outcome =
        std::panic::catch_unwind(move || dispatch(&method_owned, &params_owned, &ctx_owned));
    Some(match outcome {
        Ok(Ok(result)) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Ok(Err((code, message))) => error_response(id, code, &message),
        Err(_) => error_response(id, INTERNAL_ERROR, "internal error handling request"),
    })
}

/// Route a request method to its handler. `params` is `Value::Null` when the
/// request carried no `params` field at all.
fn dispatch(method: &str, params: &Value, ctx: &Ctx) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "ctxlake-mcp", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": crate::tools::tool_definitions() })),
        "tools/call" => crate::tools::tools_call(params, ctx),
        _ => Err((METHOD_NOT_FOUND, format!("method not found: `{method}`"))),
    }
}

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An isolated `Ctx` pointed at a fresh tempdir, kept alive alongside it. Real
    /// tool calls in these tests read/write real files under this tempdir rather
    /// than the process's real `~/.ctxlake` — see the module doc for why this
    /// beats a shared env var.
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
    fn initialize_returns_protocol_version_and_capabilities() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            &ctx,
        )
        .expect("initialize must be answered");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "ctxlake-mcp");
    }

    #[test]
    fn tools_list_returns_every_tool_with_a_schema() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#, &ctx)
            .expect("tools/list must be answered");
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "fleet_status",
                "fleet_history",
                "fleet_handoff",
                "memory_search",
                "memory_propose",
                "memory_timeline",
            ]
        );
        assert!(
            !names.contains(&"memory_write"),
            "AGENTS.md invariant 9: there must be no memory_write tool"
        );
        assert!(
            !names.contains(&"fleet_claim") && !names.contains(&"fleet_release"),
            "the lease/claim tools were removed entirely, not renamed"
        );
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(!tool["description"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn malformed_json_yields_parse_error_and_server_keeps_serving() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line("this is not json", &ctx).expect("parse errors are answered");
        assert_eq!(resp["id"], Value::Null);
        assert_eq!(resp["error"]["code"], PARSE_ERROR);
        // The server is still alive: a well-formed request right after works.
        let resp = handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#, &ctx)
            .expect("ping after garbage");
        assert_eq!(resp["result"], json!({}));
    }

    #[test]
    fn a_bare_json_array_is_an_invalid_request_not_a_panic() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line("[1,2,3]", &ctx).expect("non-object frames are answered");
        assert_eq!(resp["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn notifications_are_never_answered() {
        let (_dir, ctx) = test_ctx();
        assert!(handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &ctx
        )
        .is_none());
        assert!(handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
            &ctx
        )
        .is_none());
        // Even unknown notifications are dropped silently (JSON-RPC rule).
        assert!(handle_line(r#"{"jsonrpc":"2.0","method":"bogus"}"#, &ctx).is_none());
    }

    #[test]
    fn a_stray_result_or_error_frame_is_never_answered() {
        let (_dir, ctx) = test_ctx();
        assert!(handle_line(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, &ctx).is_none());
        assert!(handle_line(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}"#,
            &ctx
        )
        .is_none());
    }

    #[test]
    fn a_request_with_an_id_but_no_method_is_invalid_request() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(r#"{"jsonrpc":"2.0","id":3}"#, &ctx).unwrap();
        assert_eq!(resp["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
            &ctx,
        )
        .unwrap();
        assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn unknown_tool_name_is_invalid_params() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
            &ctx,
        )
        .unwrap();
        assert_eq!(resp["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn missing_required_tool_argument_is_invalid_params() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"fleet_handoff","arguments":{}}}"#,
            &ctx,
        )
        .unwrap();
        assert_eq!(resp["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn wrong_type_tool_argument_is_invalid_params_not_a_panic() {
        // `limit` must be a non-negative integer; feeding a string must not
        // panic, it must come back as a protocol error.
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"fleet_history","arguments":{"limit":"not-a-number"}}}"#,
            &ctx,
        )
        .unwrap();
        assert_eq!(resp["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn missing_params_entirely_is_invalid_params_not_a_panic() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(r#"{"jsonrpc":"2.0","id":8,"method":"tools/call"}"#, &ctx).unwrap();
        assert_eq!(resp["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn a_well_formed_tool_call_round_trips_end_to_end() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fleet_status","arguments":{}}}"#,
            &ctx,
        )
        .unwrap();
        assert_eq!(resp["result"]["isError"], false);
    }

    #[test]
    fn frame_round_trips_through_compact_single_line_json() {
        let (_dir, ctx) = test_ctx();
        let resp = handle_line(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#, &ctx).unwrap();
        let encoded = serde_json::to_string(&resp).unwrap();
        assert!(!encoded.contains('\n'), "frames must be single-line");
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["jsonrpc"], "2.0");
        assert_eq!(decoded["id"], "abc");
    }

    #[test]
    fn a_multiline_field_inside_a_response_never_breaks_single_line_framing() {
        // Regression guard for the exact hazard the module doc calls out: content
        // pulled from a cache file (a handoff note, say) can legitimately contain
        // embedded newlines. JSON string encoding escapes them, so the frame
        // itself must still be exactly one line.
        let resp = error_response(json!(1), INTERNAL_ERROR, "line one\nline two");
        let encoded = serde_json::to_string(&resp).unwrap();
        assert_eq!(
            encoded.lines().count(),
            1,
            "frame must stay one line: {encoded}"
        );
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["error"]["message"], "line one\nline two");
    }
}
