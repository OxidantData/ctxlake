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

/// `ctxlake sync install` must refuse a host that cannot do the job.
///
/// These go through the real binary because the gate lives at a call site, not in a
/// pure function: deleting the `blocks_service_install()` check from `service::install`
/// compiles cleanly and passes every unit test in `doctor.rs`, because those test the
/// gate's *return value* and nothing asserts the caller consults it. That mutation is
/// exactly how a safety check rots.
mod install_gate {
    use super::*;

    /// A config pointing at a store that cannot exist, in an isolated HOME so the
    /// test can never touch the developer's real service units.
    fn broken_host() -> (tempfile::TempDir, std::path::PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join("ctxlake.toml");
        std::fs::write(
            &cfg,
            "store = \"file:///dev/null/not-a-directory\"\nfleet_id = \"myteam\"\nagent_id = \"cc-01\"\n",
        )
        .unwrap();
        (home, cfg)
    }

    #[test]
    fn an_unreachable_store_refuses_the_install_and_writes_no_unit() {
        let (home, cfg) = broken_host();
        let out = ctxlake()
            .env("HOME", home.path())
            .arg("--config")
            .arg(&cfg)
            .args(["sync", "install"])
            .output()
            .expect("running ctxlake");

        assert!(
            !out.status.success(),
            "install must refuse an unreachable store; stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not ready") && stderr.contains("unreachable"),
            "the refusal must say what is wrong: {stderr}"
        );

        // And nothing was written. A refusal that still leaves a unit behind is worse
        // than no check at all, because the next reboot starts the daemon anyway.
        for unit in [
            home.path()
                .join(".config/systemd/user/ctxlake-sync.service"),
            home.path()
                .join("Library/LaunchAgents/com.oxidantdata.ctxlake-sync.plist"),
        ] {
            assert!(
                !unit.exists(),
                "refused install left {} behind",
                unit.display()
            );
        }
    }

    #[test]
    fn skip_checks_is_an_explicit_override_not_the_default() {
        // The escape hatch has to work — an air-gapped host, or a store reachable
        // only later, is a legitimate reason to install anyway — but it must be
        // something you asked for, which the test above proves.
        let (home, cfg) = broken_host();
        let out = ctxlake()
            .env("HOME", home.path())
            .arg("--config")
            .arg(&cfg)
            .args(["sync", "install", "--no-start", "--skip-checks"])
            .output()
            .expect("running ctxlake");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            combined.contains("skipping pre-install checks"),
            "the override must announce itself: {combined}"
        );
    }
}
