//! The exit codes a supervisor keys off, exercised through the real binary.
//!
//! `service.rs` asserts that the rendered systemd unit names `EX_CONFIG`; this
//! asserts the binary actually produces it. Those are two halves of one contract and
//! each is useless alone — a unit naming 78 against a binary that exits 1 restarts a
//! hopeless daemon every five seconds forever, and the unit test would still pass.
//!
//! Deliberately an integration test rather than a unit test: the code under test is
//! `std::process::exit`, which cannot be observed from inside the process that calls
//! it.

use std::process::Command;

fn ctxlake() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ctxlake"))
}

#[test]
fn a_missing_config_exits_ex_config_not_one() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nowhere").join("ctxlake.toml");

    // `status` is the cheapest command that needs a config and touches nothing else.
    let out = ctxlake()
        .arg("--config")
        .arg(&missing)
        .arg("status")
        .output()
        .expect("running ctxlake");

    assert_eq!(
        out.status.code(),
        Some(78),
        "a missing config must exit EX_CONFIG so RestartPreventExitStatus=78 can \
         catch it; stderr was: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The code is for the supervisor; the human still needs to be told what to do.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ctxlake init"),
        "the error must name the fix, got: {stderr}"
    );
}

#[test]
fn a_bad_argument_does_not_masquerade_as_a_config_error() {
    // clap exits 2 for a usage error. If that ever collided with EX_CONFIG, a typo in
    // a unit file would be silently treated as unfixable and never retried.
    let out = ctxlake()
        .arg("sync")
        .arg("definitely-not-a-subcommand")
        .output()
        .expect("running ctxlake");
    assert_ne!(
        out.status.code(),
        Some(78),
        "usage errors are not EX_CONFIG"
    );
}

#[test]
fn the_service_unit_invokes_a_subcommand_that_actually_exists() {
    // The unit templates hard-code `sync run --foreground`. Before the sync
    // subcommands landed, that was an unrecognized argument — a unit that crash-looped
    // on a CLI usage error, which is exactly the sort of break no unit test sees
    // because the template and the parser live in different files.
    //
    // `--help` parses the full command path without starting a daemon.
    let out = ctxlake()
        .args(["sync", "run", "--help"])
        .output()
        .expect("running ctxlake");
    assert!(
        out.status.success(),
        "`ctxlake sync run` must parse — the service units invoke it verbatim; \
         stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("--foreground"),
        "the unit passes --foreground; it must be a flag of `sync run`"
    );
}
