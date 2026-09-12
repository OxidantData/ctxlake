//! Guards the actual decision this wave made: the lease/lock abstraction is
//! **deleted**, not deprecated or hidden. A future patch that quietly reintroduces
//! `mod lease` (or a `live/leases/` key) to "fix" a race that was never actually
//! racy — every unit of work here is already idempotent by content, see
//! `ctxlake_store`'s crate doc — should fail a test, not slip through review.
//!
//! Scans this crate's own `src`/`tests`, plus `ctxlake-sync` and `ctxlake-maint`
//! (the other two crates this removal touched) by relative path from
//! `CARGO_MANIFEST_DIR` — all three live as sibling crate directories under the
//! same workspace `crates/` folder, so that layout is stable to depend on here.
//! Looks for the specific tokens the removed abstraction actually used, never a
//! bare substring match on "lease" — that would false-positive on ordinary English
//! words this codebase uses all the time, like "release" and "released".

use std::path::Path;

/// Tokens that only ever appeared as part of the removed lease abstraction.
/// Deliberately specific (never a bare "lease") so this cannot flag a legitimate
/// historical mention like "an earlier version of this design used a lease" in a
/// module doc explaining why one is no longer needed.
const FORBIDDEN_TOKENS: &[&str] = &[
    "mod lease",
    "lease::",
    "LeaseHandle",
    "LeaseState",
    "AcquireOutcome",
    "lease_maintenance",
    "leases_prefix",
    "live/leases",
    "require_maintenance_lease",
    "MAINTENANCE_LEASE_TTL",
    "LeaseNotProvisioned",
];

fn scan_dir(dir: &Path, violations: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, violations);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // This test file itself necessarily names every forbidden token (to check
        // for it) — exclude it from its own scan rather than let it flag itself.
        if path.file_name().and_then(|n| n.to_str()) == Some("no_lease_regression.rs") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for token in FORBIDDEN_TOKENS {
            if content.contains(token) {
                violations.push(format!(
                    "{}: found forbidden token {token:?}",
                    path.display()
                ));
            }
        }
    }
}

/// Scoped to the three engine crates on purpose, not the whole workspace.
///
/// `ctxlake-cli` and `ctxlake-mcp` deliberately contain these tokens *inside their own
/// regression assertions* — `status.rs` asserts its output never contains "lease",
/// `init.rs` asserts no `live/leases/` prefix is ever created — so a token scan there
/// would flag the very guards that prove the removal stuck. Those crates are covered
/// by behaviour instead, which is the stronger check: the tools catalog, the JSON-RPC
/// method list, `ctxlake status`'s output and the hook's PreToolUse response are each
/// asserted lease-free directly.
#[test]
fn no_lease_module_or_live_leases_path_remains_in_the_engine_crates() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();

    for crate_dir in ["", "../ctxlake-sync", "../ctxlake-maint"] {
        let base = manifest_dir.join(crate_dir);
        for sub in ["src", "tests"] {
            scan_dir(&base.join(sub), &mut violations);
        }
    }

    assert!(
        violations.is_empty(),
        "the lease abstraction was supposed to be deleted, not hidden:\n{}",
        violations.join("\n")
    );
}
