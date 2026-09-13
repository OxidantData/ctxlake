//! ctxlake-hook — the per-event capture binary for Claude Code, Cursor, and Hermes.
//!
//! Hermes reaches this binary through shell hooks it declares in
//! `~/.hermes/config.yaml`, a wire contract deliberately Claude Code-compatible
//! (`docs/runtimes.md § Hermes`) — it spawns this same binary per event exactly like
//! the other two runtimes, and no longer runs an in-process Python plugin
//! (`adapters/hermes/`, deleted: see `adapters::hermes`'s module doc for why a
//! second, hand-ported redactor was a liability rather than an optimization).
//!
//! Budget: 5ms p99, fired on every tool call (AGENTS.md invariant 2 — this crate
//! links only `ctxlake-core`, `serde`, `serde_json`; CI's `hook-deps` job fails the
//! build if that tree grows an async runtime, an HTTP stack, or an object store).
//! AGENTS.md invariant 1: this binary never opens a socket or touches the object
//! store — that is `ctxlake-sync`'s (the daemon's) job.
//!
//! Contract with the caller: whatever goes wrong below, exit 0 and have already
//! printed a valid response. A hook that fails the agent's turn is a worse bug than
//! one that silently drops an event.
//!
//! argv[1] is the event name in the calling runtime's own vocabulary (e.g.
//! `PreToolUse` for Claude Code, `preToolUse` for Cursor, `post_tool_call` for
//! Hermes — each adapter owns its own event names, see `adapters/`). argv[2] is the
//! runtime id (`claude_code` | `cursor` | `hermes`). Both are expected to come from
//! the installer's hook command line (a later wave), not from stdin, which is
//! exactly what makes the latency trick below possible.

use ctxlake_hook::{adapters, briefing, hostinfo, spool};
use std::io::{self, Read, Write};

/// Which event each runtime calls "the session is starting, here is your chance to
/// inject". Hermes's is `pre_llm_call` rather than `on_session_start`: only the former
/// has an injection channel, so injecting on the latter would be silently dropped
/// (docs/runtimes.md § Hermes).
fn is_session_start(runtime: &str, event: &str) -> bool {
    matches!(
        (runtime, event),
        ("claude_code", "SessionStart") | ("hermes", "pre_llm_call")
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let event = args.get(1).map(String::as_str).unwrap_or("");
    let runtime_arg = args.get(2).map(String::as_str).unwrap_or("");

    // Latency trick (see ~/.orca/agent-hooks/cursor-hook.sh): the response depends
    // only on (runtime, event), both already in argv, so it is written and flushed
    // before stdin is touched at all. The agent's turn is never blocked on anything
    // below this line.
    // SessionStart is the one event with something to say before stdin is read: the
    // briefing is already rendered in the local cache, so emitting it costs a file read
    // and keeps the latency trick intact. Every other event keeps the payload-
    // independent default. A missing or unreadable cache falls back to that default —
    // the hook fails open, always (see briefing.rs).
    let response = if is_session_start(runtime_arg, event) {
        briefing::session_start_response(runtime_arg, &hostinfo::fleet_id())
            .unwrap_or_else(|| adapters::response_for(runtime_arg, event))
    } else {
        adapters::response_for(runtime_arg, event)
    };
    print_response(&response);

    let mut raw = String::new();
    // A truncated, empty, or non-UTF-8 pipe is not worth surfacing to the agent — the
    // response is already on its way. Best-effort read, best-effort everything after.
    let _ = io::stdin().read_to_string(&mut raw);

    if let Err(e) = capture(runtime_arg, event, &raw) {
        report_error(&format!(
            "capture failed (runtime={runtime_arg:?} event={event:?}): {e}"
        ));
    }

    std::process::exit(0);
}

fn print_response(response: &str) {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    // Ignore write errors: a closed pipe on the caller's end must not turn into a
    // panic or a nonzero exit, either of which would fail the agent's turn.
    let _ = writeln!(lock, "{response}");
    let _ = lock.flush();
}

fn capture(runtime_arg: &str, event: &str, raw: &str) -> Result<(), String> {
    let runtime = adapters::parse_runtime(runtime_arg);
    let envelope = adapters::normalize(runtime, event, raw)?;
    let line = envelope.to_ndjson().map_err(|e| e.to_string())?;
    spool::append_event(runtime.as_str(), &envelope.session_id, &line)?;

    if envelope.event_type == ctxlake_core::EventType::SessionEnd {
        // Claude Code shares a 1.5s budget across *every* hook it runs for
        // `SessionEnd` — this sentinel write is the only extra work done here, and
        // it is a single local file create, not a scan of anything.
        //
        // `transcript_path` rides along because this is the last moment anything knows
        // it: the daemon seals the session later, from the spool alone, and needs it to
        // attach results and usage. Parsed straight out of the raw payload rather than
        // carried on the envelope — it is a local filesystem path and has no business in
        // the lake.
        let transcript_path = serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|v| {
                v.get("transcript_path")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            });
        spool::mark_session_done(
            runtime.as_str(),
            &envelope.session_id,
            transcript_path.as_deref(),
        )?;
    }
    Ok(())
}

fn report_error(msg: &str) {
    eprintln!("ctxlake-hook: {msg}");
    spool::log_error(msg);
}
