//! Guards the removal named in the wave-2 task brief: the in-process Python Hermes
//! plugin (`adapters/hermes/`) is deleted, not merely undocumented. Hermes reaches
//! ctxlake exclusively through `ctxlake-hook` now — see
//! `src/adapters/hermes.rs` and `docs/runtimes/hermes.md`.
//!
//! Why this is worth a real filesystem assertion rather than trusting `git rm`: a
//! rebase, a bad merge, or someone restoring the directory "to compare against"
//! could silently bring back a second, hand-ported redaction implementation with no
//! shared source — exactly the divergence risk AGENTS.md invariant 7 exists to rule
//! out. This test fails loudly the moment that happens again.

use std::path::PathBuf;

#[test]
fn the_python_hermes_plugin_directory_no_longer_exists() {
    // CARGO_MANIFEST_DIR is crates/ctxlake-hook; the repo root is two levels up.
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root must exist");
    let removed = repo_root.join("adapters").join("hermes");
    assert!(
        !removed.exists(),
        "adapters/hermes/ must stay deleted — Hermes goes through ctxlake-hook now \
         (src/adapters/hermes.rs), see that module's doc for why a second redactor \
         implementation is a liability: {}",
        removed.display()
    );
}

#[test]
fn no_second_redact_py_lingers_anywhere_in_the_tree() {
    // A narrower net than the directory check above: even a *partial* restore (just
    // `_redact.py` copied somewhere else "for reference") is the same divergence risk.
    //
    // Scope is TRACKED FILES, asked of git directly, rather than a filesystem walk.
    // A walk answers "does this byte sequence exist anywhere under the repo root",
    // which is not the question — it reports untracked scratch, vendored sources, and
    // sibling git worktrees as violations. This test failed exactly that way once, on
    // worktrees holding other branches' pre-deletion state, and a test that fails on
    // something the repo does not contain trains people to ignore it.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["ls-files", "--", "*_redact.py", "*_ulid.py", "*_spool.py"])
        .output();

    let Ok(out) = out else {
        // No git (a source tarball, say). Absence of the tool is not evidence of a
        // violation, so skip rather than fail — but say so, because a silent skip is
        // how a guard quietly stops guarding.
        eprintln!("skipping: git unavailable, cannot enumerate tracked files");
        return;
    };
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let hits: Vec<&str> = std::str::from_utf8(&out.stdout)
        .unwrap_or("")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert!(
        hits.is_empty(),
        "the retired Hermes Python plugin's modules are tracked again: {hits:?}. \
         Hermes goes through ctxlake-hook now (src/adapters/hermes.rs); a second \
         redactor implementation is the divergence risk that removal was for."
    );
}
