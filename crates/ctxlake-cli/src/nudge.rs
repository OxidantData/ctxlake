//! Tier 1's once-per-session nudge marker (docs/summarization.md: "at turn end a
//! hook returns a ONE-TIME nudge and the agent calls the `fleet_handoff` MCP
//! tool").
//!
//! **What lives here, and why not in `ctxlake-hook`.** The hook is what actually
//! decides, per turn-end event, whether to emit the nudge text — but `ctxlake-hook`
//! (AGENTS.md invariant 2) is a separate binary crate this one cannot be linked
//! into: `ctxlake-cli` ships only a `[[bin]]`, no library target, so nothing else
//! in the workspace can depend on it. This module cannot be *called* by the hook.
//!
//! What it *can* do, and does: define the marker convention precisely enough that
//! `ctxlake-hook`'s own implementation (a separate track of this wave) and
//! `ctxlake doctor`'s reporting agree on the same on-disk shape —
//! `<cache_root>/_nudged/<sanitized-session-id>.nudged`, a zero-byte file whose
//! mere existence means "already nudged." `doctor.rs` reads this directory to
//! report how many sessions have fired their nudge; see its module doc. The
//! exactly-once contract itself — a session's first check sees "not yet," every
//! later check on the same id sees "already," a different id is independent — is
//! fully specified and tested here, so wiring it into the hook is a mechanical
//! translation of this file into `ctxlake-hook`'s own dependency-light code, not a
//! design problem to re-solve there.
//!
//! Every function takes `cache_root` explicitly rather than reading
//! `ctxlake_core::paths::cache_root()` (`$CTXLAKE_CACHE_DIR`) internally — that
//! env var is a single process-wide value, and mutating it per-test is exactly the
//! flakiness AGENTS.md's house rules and this crate's own `paths.rs` module doc
//! warn against.
//!
//! [`count_nudged`] is this crate's one real production caller (`doctor.rs`'s
//! Tier 1 line — a read-only report of how many sessions have fired). Nothing in
//! `ctxlake-cli` itself ever *marks* a session nudged — only a running hook
//! process, mid-turn, is in a position to decide that — so [`should_nudge`] and
//! [`mark_nudged`] have no caller in this binary today. They stay `pub(crate)` and
//! `#[allow(dead_code)]`, each annotated at its own definition with why: they are
//! the tested reference implementation of the marker contract, not dead weight —
//! see each function's doc.

use std::path::{Path, PathBuf};

fn marker_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("_nudged")
}

/// A session id is opaque, externally-generated text (a ULID from one runtime, a
/// UUID from another) — sanitized into a safe filename component the same way
/// `ctxlake_store::layout::join` guards a dynamic path segment, rather than
/// trusting it not to contain `/` or `..`.
fn marker_path(cache_root: &Path, session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    marker_dir(cache_root).join(format!("{safe}.nudged"))
}

/// True if `session_id` has not yet been nudged under `cache_root`. Never errors:
/// a marker directory that doesn't exist yet reads as "not nudged," exactly like
/// one that does but lacks this session's file.
///
/// `#[allow(dead_code)]`: no code path in `ctxlake-cli` calls this today — only a
/// running hook process, mid-turn, is ever in a position to ask "have I already
/// nudged this session?" — and `ctxlake-hook` cannot import it (see the module
/// doc: this crate has no library target). This function, and its exhaustive test
/// coverage below, is the tested specification `ctxlake-hook`'s own copy needs to
/// match; keeping it here (rather than only in a comment) means the contract is
/// something a regression test can actually break.
#[allow(dead_code)]
pub(crate) fn should_nudge(cache_root: &Path, session_id: &str) -> bool {
    !marker_path(cache_root, session_id).exists()
}

/// Record that `session_id` has been nudged. Best-effort: a failed write (a
/// read-only filesystem, say) is swallowed rather than surfaced — the caller
/// already emitted its nudge for this turn, and the hook contract this mirrors
/// (`ctxlake-hook`'s own `main.rs`: "whatever goes wrong, exit 0") never fails a
/// turn over a diagnostic write.
///
/// `#[allow(dead_code)]`: see [`should_nudge`]'s doc — same reason, same fix.
#[allow(dead_code)]
pub(crate) fn mark_nudged(cache_root: &Path, session_id: &str) {
    let path = marker_path(cache_root, session_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, b"");
}

/// How many sessions currently show a fired nudge marker under `cache_root` —
/// `ctxlake doctor`'s Tier 1 visibility line (its one real call site). `0` for
/// "the directory doesn't exist yet" (nobody has nudged), same as an empty one.
pub fn count_nudged(cache_root: &Path) -> usize {
    std::fs::read_dir(marker_dir(cache_root))
        .map(|entries| entries.flatten().count())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_exactly_once_per_session_not_once_per_turn() {
        let dir = tempfile::tempdir().unwrap();

        // Three "turns" of the same session: only the first observes "not yet
        // nudged." This is the property Tier 1 depends on — see docs/
        // summarization.md's "nudging on every turn would be intolerable."
        assert!(should_nudge(dir.path(), "sess-1"), "turn 1 must nudge");
        mark_nudged(dir.path(), "sess-1");
        assert!(
            !should_nudge(dir.path(), "sess-1"),
            "turn 2 must not nudge again"
        );
        mark_nudged(dir.path(), "sess-1");
        assert!(
            !should_nudge(dir.path(), "sess-1"),
            "turn 3 must still not nudge"
        );
    }

    #[test]
    fn different_sessions_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        mark_nudged(dir.path(), "sess-a");
        assert!(!should_nudge(dir.path(), "sess-a"));
        assert!(
            should_nudge(dir.path(), "sess-b"),
            "a different session must still get its own first nudge"
        );
    }

    #[test]
    fn a_hostile_session_id_cannot_escape_the_marker_directory() {
        let dir = tempfile::tempdir().unwrap();
        let p = marker_path(dir.path(), "../../etc/passwd");
        assert!(
            p.starts_with(marker_dir(dir.path())),
            "escaped its directory: {p:?}"
        );
        // And it must still be usable as an ordinary file, not accidentally
        // resolve into a directory traversal that `create_dir_all`/`write` chokes
        // on.
        mark_nudged(dir.path(), "../../etc/passwd");
        assert!(!should_nudge(dir.path(), "../../etc/passwd"));
    }

    #[test]
    fn count_nudged_reports_zero_before_any_marker_and_the_right_count_after() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_nudged(dir.path()), 0, "no marker dir yet");
        mark_nudged(dir.path(), "sess-1");
        mark_nudged(dir.path(), "sess-2");
        assert_eq!(count_nudged(dir.path()), 2);
        // Re-marking the same session must not double-count it.
        mark_nudged(dir.path(), "sess-1");
        assert_eq!(count_nudged(dir.path()), 2);
    }
}
