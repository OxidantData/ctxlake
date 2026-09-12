//! `ctxlake-mcp` — the MCP tool server, run as `ctxlake mcp`.
//!
//! See `docs/architecture.md`'s component table and `docs/reference.md` for what this
//! process is and is not allowed to do. In one sentence: it is a stdio JSON-RPC
//! 2.0 server speaking MCP protocol 2024-11-05, exposing fleet coordination and
//! (eventually) shared-memory tools to whichever runtime spawned it — Claude Code,
//! Cursor, or Hermes are all MCP clients, so this is the one integration surface
//! that works identically across every runtime this project supports.
//!
//! AGENTS.md invariant 1, restated for this process specifically: it never touches
//! the object store, in either direction. It reads the local cache and writes the
//! local spool — see `paths.rs` — the same discipline `ctxlake-hook` observes, for
//! the same reason: both processes run on a path the calling agent is
//! synchronously waiting on. There is no `object_store` or `tokio` dependency
//! anywhere in this crate's `Cargo.toml`, so that boundary cannot be violated by
//! accident; it would have to be reintroduced as a dependency first, which a
//! reviewer of that diff would have to explain. `rusqlite` (bundled) and `time`
//! are the two exceptions — both are local-file/clock utilities with no network or
//! async-runtime dependency of their own; see `snapshot.rs`'s and `Cargo.toml`'s
//! comments for why each was added.
//!
//! ## Module map
//! - [`protocol`] — JSON-RPC 2.0 framing: parsing, dispatch, error codes.
//! - [`tools`] — the `tools/list` catalog and `tools/call` routing.
//! - [`fleet`] — `fleet_status` / `fleet_history` / `fleet_handoff`.
//! - [`memory`] — `memory_search` / `memory_propose` / `memory_timeline`, wired to
//!   the real claim store (see [`snapshot`]). `memory_propose` is a real spool
//!   write today, as it always has been (AGENTS.md invariant 9: never a promoted
//!   claim); the two read tools now answer from the actual local snapshot mirror
//!   rather than a placeholder cache file, honestly empty whenever that mirror is
//!   absent or every claim in it is shadow-mode-invisible.
//! - [`snapshot`] — opens `<cache_root>/<fleet_id>/snapshot.bin` (the local mirror
//!   of `ctxlake-maint`'s published claim snapshot) and answers FTS5-plus-cosine
//!   search and the outcome timeline. This is where shadow mode's "reads nothing"
//!   guarantee actually lives structurally.
//! - [`wire`] — the exact JSON shape `memory_propose` spools, mirroring
//!   `ctxlake-maint::claims::ClaimEvent::Proposed` byte-for-byte without this
//!   crate depending on that crate.
//! - [`sanitize`] — the render-time cleaner every untrusted field (a peer's claim,
//!   a peer's handoff note) is routed through before it reaches a tool result.
//! - [`write_guard`] — the write-time secret scrubber every free-text argument to a
//!   write-shaped tool call is routed through before it reaches the spool
//!   (AGENTS.md invariant 7) — [`sanitize`]'s mirror image on the write path.
//! - [`paths`] — local spool/cache roots, and the [`paths::Ctx`] threaded through
//!   every dispatch call.
//! - [`spool`] — the append-only local queue every write-shaped tool call uses.

pub mod fleet;
pub mod memory;
pub mod paths;
pub mod protocol;
pub mod sanitize;
pub mod snapshot;
pub mod spool;
pub mod tools;
pub mod wire;
pub mod write_guard;

use std::io::{self, BufRead, Write};

use paths::Ctx;

/// Serve MCP frames on real stdin/stdout until EOF. This is the only function in
/// this crate that touches process I/O directly — everything else takes explicit
/// arguments, so it can be tested without a subprocess.
///
/// **stdout carries JSON-RPC response frames and nothing else.** Every diagnostic
/// goes to stderr via [`eprintln!`]; a stray write to stdout anywhere on this path
/// would corrupt the frame stream a client is parsing line-by-line as JSON, in a
/// way that looks like a client-side bug rather than a server one.
pub fn run_stdio() {
    let ctx = Ctx::from_env();
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(stdin.lock(), stdout.lock(), &ctx);
}

/// The real loop behind [`run_stdio`], generic over its input/output so it can be
/// driven by an in-memory buffer in tests instead of a real process pipe — that is
/// exactly what backs the "stdout carries only valid JSON-RPC frames" conformance
/// test below, which would otherwise need to spawn a subprocess.
pub fn serve(input: impl BufRead, mut output: impl Write, ctx: &Ctx) {
    for line in input.lines() {
        let line = match line {
            Ok(l) => l,
            // A read error on stdin (e.g. invalid UTF-8) ends the session — there
            // is no line to answer and no way to keep reading past it — but it
            // must not panic.
            Err(e) => {
                eprintln!("ctxlake-mcp: stdin read error: {e}");
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(response) = protocol::handle_line(line, ctx) {
            if write_frame(&mut output, &response).is_err() {
                // The client closed its end of the pipe. Nothing left to serve.
                break;
            }
        }
    }
}

fn write_frame(out: &mut impl Write, response: &serde_json::Value) -> io::Result<()> {
    let mut encoded =
        serde_json::to_string(response).expect("a serde_json::Value always serializes");
    encoded.push('\n');
    out.write_all(encoded.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn write_frame_emits_exactly_one_newline_terminated_line() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &json!({"jsonrpc": "2.0", "id": 1, "result": {}})).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.matches('\n').count(), 1);
        assert!(s.ends_with('\n'));
        let decoded: serde_json::Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(decoded["id"], 1);
    }

    /// The conformance requirement stated in AGENTS.md and this crate's own spec:
    /// nothing may reach stdout except JSON-RPC frames. This drives a whole
    /// session — well-formed requests, malformed JSON, notifications, an unknown
    /// method, a tool call with bad arguments — through the exact loop
    /// `run_stdio` uses, and inspects every line `serve` wrote as if it were a
    /// real client reading the pipe: each one must be valid JSON, must be exactly
    /// one line, and must carry `"jsonrpc":"2.0"`. A stray `eprintln!`/`println!`
    /// introduced anywhere on this path would either corrupt a line (this test's
    /// per-line JSON parse fails) or produce an extra frame with no `jsonrpc`
    /// field (the last assertion below fails).
    #[test]
    fn stdout_carries_only_valid_json_rpc_frames_across_a_mixed_session() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            spool_root: dir.path().join("spool"),
            cache_root: dir.path().join("cache"),
            fleet_id: "test-fleet".to_string(),
            agent_id: "test-agent".to_string(),
        };
        let input = Cursor::new(
            [
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                "", // blank line: must be skipped, not answered
                "not json at all",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fleet_status","arguments":{}}}"#,
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_propose","arguments":{"claim":"x","type":"convention","evidence":[]}}}"#,
                r#"{"jsonrpc":"2.0","id":5,"method":"nope"}"#,
            ]
            .join("\n")
            .into_bytes(),
        );
        let mut output = Vec::new();
        serve(input, &mut output, &ctx);

        let text = String::from_utf8(output).expect("stdout must be valid UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        // 6 answerable requests went in (the notification and the blank line do
        // not get a response); if any produced more or fewer than one frame,
        // something wrote to stdout outside handle_line's own response.
        assert_eq!(lines.len(), 6, "unexpected frame count in: {text:?}");
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line was not valid JSON: {e}: {line:?}"));
            assert_eq!(
                parsed["jsonrpc"], "2.0",
                "frame missing jsonrpc marker: {line}"
            );
            assert!(parsed.get("id").is_some(), "frame missing id: {line}");
        }
    }
}
