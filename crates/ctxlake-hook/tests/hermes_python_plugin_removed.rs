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
    // `_redact.py` copied somewhere else "for reference") is the same divergence
    // risk. Walk the repo root's tracked-looking source directories, skipping the
    // usual bulk (target/, node_modules/, .git/) that would make this slow and
    // noisy without adding any real coverage.
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root must exist");

    let mut hits = Vec::new();
    let mut stack = vec![repo_root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), "target" | "node_modules" | ".git" | "site") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if name == "_redact.py" || name == "_ulid.py" || name == "_spool.py" {
                hits.push(path);
            }
        }
    }
    assert!(
        hits.is_empty(),
        "found a lingering copy of the retired Hermes Python plugin's modules: {hits:?}"
    );
}
