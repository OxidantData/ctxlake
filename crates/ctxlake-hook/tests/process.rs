//! Process-level behavior of the compiled `ctxlake-hook` binary: things that can only
//! be observed by actually spawning it (exit code, what lands on stdout before stdin
//! is even readable, behavior on genuinely malformed input). Everything that can be
//! tested in-process instead lives in `src/` unit tests or `tests/golden.rs`.
//!
//! Each test sets `CTXLAKE_SPOOL_DIR`/`CTXLAKE_HOOK_ERROR_LOG` on the *child's*
//! environment to a fresh tempdir. That is deliberate: a child process gets its own
//! environment block, so tests running in parallel never race over these vars the way
//! they would if a test mutated `std::env` in-process (see `src/hostinfo.rs`'s docs
//! for the same concern from the other side).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn hook_cmd(tmp: &tempfile::TempDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ctxlake-hook"));
    cmd.env("CTXLAKE_SPOOL_DIR", tmp.path().join("spool"));
    cmd.env("CTXLAKE_HOOK_ERROR_LOG", tmp.path().join("hook-errors.log"));
    cmd.env("CTXLAKE_FLEET_ID", "test-fleet");
    cmd.env("CTXLAKE_AGENT_ID", "test-agent");
    cmd
}

fn run(mut cmd: Command, stdin_bytes: &[u8]) -> (i32, String) {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ctxlake-hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_bytes)
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for ctxlake-hook");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
    )
}

#[test]
fn exits_zero_and_emits_a_response_on_well_formed_input() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = hook_cmd(&tmp);
    cmd.arg("SessionStart").arg("claude_code");
    let (code, stdout) = run(cmd, br#"{"session_id":"s1","cwd":"/repo"}"#);
    assert_eq!(code, 0);
    assert_eq!(stdout, "{}\n");

    let spooled = std::fs::read_to_string(tmp.path().join("spool/claude_code/s1.ndjson"))
        .expect("spool file written");
    assert!(spooled.contains("\"session_start\""), "got: {spooled}");
}

#[test]
fn exits_zero_on_empty_stdin_and_never_panics() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = hook_cmd(&tmp);
    cmd.arg("PreToolUse").arg("claude_code");
    let (code, stdout) = run(cmd, b"");
    assert_eq!(code, 0);
    assert_eq!(stdout, "{}\n");
    // Nothing valid to capture — no spool directory should even get created for this
    // (nonexistent) session, but the process must still not have crashed.
}

#[test]
fn exits_zero_on_truncated_json_and_never_panics() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = hook_cmd(&tmp);
    cmd.arg("PostToolUse").arg("claude_code");
    let (code, stdout) = run(cmd, br#"{"session_id": "s1", "tool_name": "Bash""#); // missing closing braces
    assert_eq!(code, 0);
    assert_eq!(stdout, "{}\n");
}

#[test]
fn exits_zero_on_malformed_utf8_and_never_panics() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = hook_cmd(&tmp);
    cmd.arg("PreToolUse").arg("cursor");
    // 0xFF is never valid UTF-8 in any position.
    let (code, _stdout) = run(cmd, &[0x7B, 0xFF, 0xFE, 0x7D]);
    assert_eq!(code, 0);
}

#[test]
fn exits_zero_for_an_unrecognized_runtime_and_event() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = hook_cmd(&tmp);
    cmd.arg("SomeFutureEvent").arg("some-future-runtime");
    let (code, stdout) = run(cmd, br#"{"session_id":"s1"}"#);
    assert_eq!(code, 0);
    assert_eq!(
        stdout, "{}\n",
        "an unrecognized (event, runtime) must still get a safe default response"
    );
}

#[test]
fn exits_zero_with_no_arguments_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let cmd = hook_cmd(&tmp);
    let (code, stdout) = run(cmd, b"");
    assert_eq!(code, 0);
    assert_eq!(stdout, "{}\n");
}

#[test]
fn cursor_gate_events_get_a_permission_response_claude_code_gets_an_empty_object() {
    let tmp = tempfile::tempdir().unwrap();

    let mut cmd = hook_cmd(&tmp);
    cmd.arg("preToolUse").arg("cursor");
    let (_, stdout) = run(
        cmd,
        br#"{"session_id":"s1","tool_name":"shell","tool_input":{}}"#,
    );
    assert_eq!(stdout, "{\"permission\":\"allow\"}\n");

    let mut cmd = hook_cmd(&tmp);
    cmd.arg("beforeSubmitPrompt").arg("cursor");
    let (_, stdout) = run(cmd, br#"{"session_id":"s1","prompt":"hi"}"#);
    assert_eq!(stdout, "{\"continue\":true}\n");

    let mut cmd = hook_cmd(&tmp);
    cmd.arg("PreToolUse").arg("claude_code");
    let (_, stdout) = run(
        cmd,
        br#"{"session_id":"s1","tool_name":"Bash","tool_input":{}}"#,
    );
    assert_eq!(
        stdout, "{}\n",
        "claude_code never gates in wave 1 — see adapters/claude_code.rs"
    );
}

/// Regression for the removed lease/collision feature: there is no pre-edit
/// check anywhere in this binary that inspects `tool_input` to warn or block a
/// call because another agent "holds" the path it touches. `response_for` is
/// computed from `(runtime, event)` alone, before stdin is even read (see
/// `main.rs`'s latency trick), so every runtime's PreToolUse-equivalent must
/// come back exactly the same unconditional non-blocking answer regardless of
/// what path the tool call names — capture still happens (see the other tests
/// in this file), only the warning is gone.
#[test]
fn pre_tool_use_never_warns_or_blocks_on_the_touched_path() {
    let tmp = tempfile::tempdir().unwrap();
    let touching_a_path =
        br#"{"session_id":"s1","tool_name":"Edit","tool_input":{"file_path":"crates/foo/bar.rs"}}"#;

    let mut cmd = hook_cmd(&tmp);
    cmd.arg("PreToolUse").arg("claude_code");
    let (_, stdout) = run(cmd, touching_a_path);
    assert_eq!(stdout, "{}\n");

    let mut cmd = hook_cmd(&tmp);
    cmd.arg("preToolUse").arg("cursor");
    let (_, stdout) = run(cmd, touching_a_path);
    assert_eq!(stdout, "{\"permission\":\"allow\"}\n");

    let mut cmd = hook_cmd(&tmp);
    cmd.arg("pre_tool_call").arg("hermes");
    let (_, stdout) = run(cmd, touching_a_path);
    assert_eq!(stdout, "{}\n");
}

#[test]
fn a_leaked_secret_in_tool_output_never_reaches_the_spool_file() {
    // `cat .env` is the canonical leak. This used to assert the output was scrubbed and
    // the envelope marked `quarantined`. The guarantee is now stronger and simpler: the
    // hook reads no tool output at all for Claude Code, so there is nothing on this path
    // to scrub, mark, or miss.
    //
    // The redaction obligation did not disappear — it moved. Tool output now arrives via
    // the session transcript, read by the daemon, and the equivalent end-to-end
    // assertion lives with that reader. If you are here because you are moving output
    // capture back onto the hook path, restore the `quarantined` assertion too.
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = hook_cmd(&tmp);
    cmd.arg("PostToolUse").arg("claude_code");
    let stdin = br#"{"session_id":"s1","tool_use_id":"tu-1","tool_name":"Bash","tool_input":{"command":"cat .env"},"tool_response":"AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"}"#;
    let (code, _stdout) = run(cmd, stdin);
    assert_eq!(code, 0);

    let spooled = std::fs::read_to_string(tmp.path().join("spool/claude_code/s1.ndjson"))
        .expect("spool file written");
    assert!(
        !spooled.contains("AKIAIOSFODNN7EXAMPLE"),
        "a real secret reached the spool file: {spooled}"
    );
    assert!(
        !spooled.contains("\"result\""),
        "the hook must carry no tool output at all: {spooled}"
    );
    // The call itself is still recorded — losing the event would be its own bug.
    assert!(spooled.contains("\"tu-1\""), "got: {spooled}");
}

/// The latency trick this crate exists to copy: the response is written and flushed
/// before stdin is even read, so the caller is never blocked on the hook's own work.
/// This is demonstrated by reading a full response line back while our own stdin
/// write-half is still open (and the child, if it worked the naive way, would be
/// blocked forever waiting for EOF on it).
#[test]
fn response_is_written_before_stdin_is_read() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = hook_cmd(&tmp);
    cmd.arg("SessionStart").arg("claude_code");

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ctxlake-hook");

    // Deliberately do NOT write to or close stdin yet.
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("read response line without ever touching stdin");
    assert_eq!(
        line, "{}\n",
        "the response must be exactly what a payload-independent lookup produces"
    );

    // Now let the child finish: closing stdin unblocks its (best-effort) read.
    drop(child.stdin.take());
    let status = child.wait().expect("wait for ctxlake-hook");
    assert!(status.success());
}

#[test]
fn concurrent_hook_invocations_for_one_session_do_not_corrupt_the_spool_file() {
    // The realistic case: several parallel tool calls in one session each spawn their
    // own hook process and append to the same ndjson file at once.
    let tmp = tempfile::tempdir().unwrap();
    let mut children = Vec::new();
    for i in 0..12 {
        let mut cmd = hook_cmd(&tmp);
        cmd.arg("PreToolUse").arg("claude_code");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn ctxlake-hook");
        let payload = format!(
            r#"{{"session_id":"shared","tool_use_id":"tu-{i}","tool_name":"Bash","tool_input":{{"command":"echo {i}"}}}}"#
        );
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        children.push(child);
    }
    for child in &mut children {
        let status = child.wait().expect("wait for ctxlake-hook");
        assert!(status.success());
    }

    let path: PathBuf = tmp.path().join("spool/claude_code/shared.ndjson");
    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(
        lines.len(),
        12,
        "a line went missing or two lines merged into one"
    );
    for line in &lines {
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("line is not valid standalone JSON (interleaved write?): {e}\nline: {line}")
        });
        assert_eq!(parsed["session_id"], "shared");
    }
}

/// Reading valid UTF-8 that happens to *look* almost-empty (whitespace only) must not
/// be treated as well-formed JSON.
#[test]
fn whitespace_only_stdin_is_treated_as_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = hook_cmd(&tmp);
    cmd.arg("PreToolUse").arg("claude_code");
    let (code, stdout) = run(cmd, b"   \n\t  ");
    assert_eq!(code, 0);
    assert_eq!(stdout, "{}\n");
}
