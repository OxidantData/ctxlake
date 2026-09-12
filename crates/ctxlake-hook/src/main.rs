//! ctxlake-hook — the per-event capture binary for Claude Code and Cursor.
//!
//! Hermes does not spawn this binary: it is an in-process Python plugin
//! (`adapters/hermes/` at the repo root) that writes to the same spool directly,
//! because spawning a subprocess per LLM call would itself blow the latency budget
//! this binary exists to protect.
//!
//! Budget: 5ms p99, fired on every tool call (AGENTS.md invariant 2 — this crate
//! links only `ctxlake-core`, `serde`, `serde_json`; CI's `hook-deps` job fails the
//! build if that tree grows an async runtime, an HTTP stack, or an object store).
//! AGENTS.md invariant 1: this binary never opens a socket or touches the object
//! store — that is the daemon's job, several waves from now.
//!
//! Contract with the caller: whatever goes wrong below, exit 0 and have already
//! printed a valid response. A hook that fails the agent's turn is a worse bug than
//! one that silently drops an event.
//!
//! argv[1] is the event name in the calling runtime's own vocabulary (e.g.
//! `PreToolUse` for Claude Code, `preToolUse` for Cursor — each adapter owns its own
//! event names, see `adapters/`). argv[2] is the runtime id (`claude_code` | `cursor`).
//! Both are expected to come from the installer's hook command line (a later wave),
//! not from stdin, which is exactly what makes the latency trick below possible.

use ctxlake_hook::{adapters, spool};
use std::io::{self, Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let event = args.get(1).map(String::as_str).unwrap_or("");
    let runtime_arg = args.get(2).map(String::as_str).unwrap_or("");

    // Latency trick (see ~/.orca/agent-hooks/cursor-hook.sh): the response depends
    // only on (runtime, event), both already in argv, so it is written and flushed
    // before stdin is touched at all. The agent's turn is never blocked on anything
    // below this line.
    print_response(&adapters::response_for(runtime_arg, event));

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
        spool::mark_session_done(runtime.as_str(), &envelope.session_id)?;
    }
    Ok(())
}

fn report_error(msg: &str) {
    eprintln!("ctxlake-hook: {msg}");
    spool::log_error(msg);
}
