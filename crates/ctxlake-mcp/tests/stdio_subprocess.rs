//! Drives the *real* `ctxlake-mcp` binary as a child process and inspects its
//! actual stdout — the property `lib.rs`'s in-memory `serve()` test cannot check.
//!
//! `serve()`'s own conformance test (`stdout_carries_only_valid_json_rpc_frames_
//! across_a_mixed_session`) writes into a `Vec<u8>` the test controls; a stray
//! `println!` anywhere on the dispatch path writes to the process's *real* stdout,
//! which that test never looks at, so it stays green through exactly the
//! regression it claims to guard against (verified: injecting such a `println!`
//! into `protocol::dispatch` left every one of that crate's 52-plus unit tests
//! passing). `run_stdio` — the function every real client actually invokes — has
//! its own real stdin/stdout and was untested altogether.
//!
//! This test closes that gap by spawning the genuine binary, feeding it real
//! stdin, and asserting every line on its real stdout is exactly one valid
//! JSON-RPC frame — the same assertions `serve()`'s test makes, but against a
//! channel a stray `println!`/`eprintln!`-to-stdout mistake cannot hide from.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn real_process_stdout_carries_only_json_rpc_frames() {
    let exe = env!("CARGO_BIN_EXE_ctxlake-mcp");
    let dir = tempfile::tempdir().unwrap();

    let mut child = Command::new(exe)
        .env("CTXLAKE_SPOOL_DIR", dir.path().join("spool"))
        .env("CTXLAKE_CACHE_DIR", dir.path().join("cache"))
        .env("CTXLAKE_FLEET_ID", "test-fleet")
        .env("CTXLAKE_AGENT_ID", "test-agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the ctxlake-mcp binary under test");

    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fleet_status","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_propose","arguments":{"claim":"x","type":"convention","evidence":[]}}}"#,
        r#"{"jsonrpc":"2.0","id":5,"method":"nope"}"#,
    ];
    {
        let stdin = child.stdin.as_mut().expect("child stdin was not piped");
        for line in requests {
            writeln!(stdin, "{line}").expect("write to child stdin");
        }
    }
    // Close our end so the child's `input.lines()` loop sees EOF and returns.
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .expect("failed to wait for the child process");
    assert!(
        output.status.success(),
        "ctxlake-mcp exited non-zero: {:?}",
        output.status
    );

    let text = String::from_utf8(output.stdout).expect("stdout must be valid UTF-8");
    let lines: Vec<&str> = text.lines().collect();
    // 5 answerable requests went in (the notification does not get a response);
    // any other count means something wrote to real stdout outside a response.
    assert_eq!(
        lines.len(),
        5,
        "unexpected frame count on real stdout: {text:?}"
    );
    for line in &lines {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line on real stdout was not valid JSON: {e}: {line:?}"));
        assert_eq!(
            parsed["jsonrpc"], "2.0",
            "frame on real stdout missing jsonrpc marker: {line}"
        );
    }
}
