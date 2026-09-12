//! The local spool: hook -> spool -> daemon -> store (AGENTS.md invariant 1 — the
//! store is never reachable from here, in either direction).
//!
//! Layout: one NDJSON file per (runtime, session_id):
//! `<spool_root>/<runtime>/<session_id>.ndjson`, rotated at [`MAX_SPOOL_FILE_BYTES`]
//! and marked done with a sibling `.done` file on session end.
//!
//! Every public function here takes an explicit root/path rather than reading
//! `CTXLAKE_SPOOL_DIR` itself, and the env-reading convenience wrappers at the bottom
//! are the only things that touch process environment. That split exists for tests:
//! `cargo test` runs many tests in one process, and two tests racing to set the same
//! env var is a real flake, not a hypothetical one — see `hostinfo.rs` for the same
//! pattern.
//!
//! ## Concurrency
//!
//! Two sessions never share a file (the path includes `session_id`), but *one*
//! session's parallel tool calls can: Claude Code and Cursor can both run more than
//! one tool at once, and each spawns its own `ctxlake-hook` process. Every append
//! opens the file with `.append(true)` (`O_APPEND` on Unix) and issues exactly one
//! `write_all` call for the whole line. POSIX guarantees that `O_APPEND` makes the
//! seek-to-end-and-write of a single `write()` syscall atomic with respect to other
//! `O_APPEND` writers on the same file — so as long as the write completes in one
//! syscall, two writers' lines cannot interleave. `write_all` only loops on a short
//! write, which practically does not happen for local-disk writes at the sizes one
//! event produces; [`crate::adapters::common::MAX_FIELD_BYTES`] keeps those sizes
//! small on purpose, as defense in depth rather than a proof.
//!
//! ## Ordering is append order, not `event_id`
//!
//! `ctxlake-hook` is spawned fresh per event (see `main.rs`): whatever monotonicity
//! `Envelope::new`'s ULID generation offers is scoped to a single call in a single
//! process, so it buys nothing across two events from the same session — each is a
//! different process with its own generator state. Claude Code, Cursor, and Hermes
//! all spawn this same binary once per event now (Hermes stopped running in-process
//! once its Python plugin was retired for `adapters::hermes` — see that module's
//! doc), so the only ordering this design gets for any of the three is *append
//! order*: the order in which processes reach [`append_event_at`] for a given
//! session's file, not `event_id`'s sort order.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Rotate a session's ndjson file once it reaches this size, so one long-running
/// session cannot produce a single file the (not-yet-built) daemon must read whole.
pub const MAX_SPOOL_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Refuse to grow a runtime's spool directory past this and drop new events instead.
/// Wave 1 ships no daemon to drain the spool yet, so an idle laptop must not fill its
/// disk silently while that wave is still being built. Arbitrary and generous; make it
/// configurable if it turns out to bind before the daemon ships.
pub const MAX_RUNTIME_DIR_BYTES: u64 = 512 * 1024 * 1024;

fn session_file(root: &Path, runtime: &str, session_id: &str) -> PathBuf {
    root.join(runtime).join(format!("{session_id}.ndjson"))
}

/// Reject a `session_id` that is not a single safe path segment.
///
/// `session_id` comes straight from untrusted hook stdin (and, via `ctxlake import`
/// in a later wave, from historical transcripts — an even less trustworthy source).
/// It is joined into a filesystem path unchecked otherwise, so `"../../pwned"` or a
/// bare `".."` escapes the spool root entirely: `<root>/<runtime>/../../pwned.ndjson`
/// lands two directories above `<root>`, and with the real default root
/// (`~/.ctxlake/spool`) that reaches anywhere the user can write. A single `/`, `\`,
/// or NUL is enough to break out too, so all three are rejected outright rather than
/// only the two dot-segments.
fn validate_session_id(session_id: &str) -> Result<(), String> {
    if session_id.is_empty() {
        return Err("session_id must not be empty".to_string());
    }
    if session_id == "." || session_id == ".." {
        return Err(format!("session_id must not be {session_id:?}"));
    }
    if session_id.contains(['/', '\\', '\0']) {
        return Err(format!(
            "session_id must be a single path segment, got {session_id:?}"
        ));
    }
    Ok(())
}

/// Append one line (without a trailing newline) to the session's spool file, creating
/// the runtime directory and file as needed. Returns `Err` only for a genuine I/O
/// failure the caller should log — callers must still exit 0 either way (AGENTS.md
/// invariant 1 says the hook must never fail the agent's turn; this crate applies
/// that to *any* failure, not only a store failure, since there is no store here).
pub fn append_event_at(
    root: &Path,
    runtime: &str,
    session_id: &str,
    line: &str,
) -> Result<(), String> {
    validate_session_id(session_id)?;
    let dir = root.join(runtime);
    fs::create_dir_all(&dir).map_err(|e| format!("create spool dir {}: {e}", dir.display()))?;

    let tracked = read_or_seed_tracked_size(&dir);
    if tracked >= MAX_RUNTIME_DIR_BYTES {
        eprintln!(
            "ctxlake-hook: WARNING spool dir {} is at or over {} bytes; dropping event rather than filling the disk",
            dir.display(),
            MAX_RUNTIME_DIR_BYTES
        );
        return Ok(());
    }

    let path = session_file(root, runtime, session_id);
    rotate_if_full(&path);

    let bytes = format!("{line}\n");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open spool file {}: {e}", path.display()))?;
    f.write_all(bytes.as_bytes())
        .map_err(|e| format!("append to {}: {e}", path.display()))?;
    write_tracked_size(&dir, tracked + bytes.len() as u64);
    Ok(())
}

/// Rename a full spool file out of the way so the next append starts a fresh one.
///
/// This is a best-effort race, not a lock: if two processes both see the file at or
/// over the size threshold, both may attempt the rename, and the second will fail
/// with `NotFound` because the first already moved it — that's fine, it just means
/// the second writer opens (or creates) the fresh canonical path instead. A rename
/// never corrupts data a concurrent writer already has an open handle to, because
/// POSIX rename does not affect file descriptors a process already holds on the old
/// inode; that process keeps appending to what is now the rotated file.
fn rotate_if_full(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return; // no file yet — nothing to rotate.
    };
    if meta.len() < MAX_SPOOL_FILE_BYTES {
        return;
    }
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let rotated = path.with_extension(format!("ndjson.{suffix}"));
    let _ = fs::rename(path, rotated);
}

/// Sum of file sizes directly inside `dir` (not recursive). This is the expensive,
/// O(files in dir) fallback used only to seed [`read_or_seed_tracked_size`]'s
/// sidecar once — see that function's docs for why the steady-state cap check must
/// not call this on every event.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0; // directory doesn't exist yet — nothing counted, nothing to drop.
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Name of the sidecar file that tracks a runtime directory's approximate total size.
const SIZE_SIDECAR_NAME: &str = ".spool_size";

/// The runtime directory's tracked size, read from a small sidecar file instead of
/// stat-ing every entry in the directory.
///
/// `ctxlake-hook` is a fresh process per event (`main.rs`'s docs) — there is no
/// long-lived process to cache a directory scan in, so without this sidecar,
/// [`append_event_at`] paid for a full `read_dir` + `stat` of every file in the
/// runtime directory on *every single tool call*. That is fine at a handful of
/// files and a real cost once a laptop has run this for a few weeks: each session
/// leaves an `.ndjson` and a `.done` behind forever (wave 1 ships no daemon to drain
/// them — see `MAX_RUNTIME_DIR_BYTES`'s docs), so the file count only grows, and the
/// scan's cost grows with it on a path budgeted at 5ms p99.
///
/// The sidecar is missing on a fresh directory (or one written by a build that
/// predates this file), so this falls back to a real scan exactly once and persists
/// the result — every call after that is a few bytes' read. Concurrent writers can
/// race on the read-here / write-in-`write_tracked_size` round trip and under- or
/// over-count by a line or two; that is acceptable for a cap that is already
/// documented as "arbitrary and generous" advisory disk-fill protection, not for
/// anything that needs to be exact.
fn read_or_seed_tracked_size(dir: &Path) -> u64 {
    if let Some(cached) = fs::read_to_string(dir.join(SIZE_SIDECAR_NAME))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        return cached;
    }
    let scanned = dir_size(dir);
    write_tracked_size(dir, scanned);
    scanned
}

/// Persist the runtime directory's tracked size. Best-effort: a failed write just
/// means the next call falls back to a real scan again, which is correct, if slow.
fn write_tracked_size(dir: &Path, size: u64) {
    let _ = fs::write(dir.join(SIZE_SIDECAR_NAME), size.to_string());
}

/// Write the `.done` sentinel marking a session's spool file as finished. The daemon
/// (a later wave) uses this to know a session's ndjson file is safe to ship without
/// racing a writer that might still append to it.
pub fn mark_session_done_at(root: &Path, runtime: &str, session_id: &str) -> Result<(), String> {
    validate_session_id(session_id)?;
    let dir = root.join(runtime);
    fs::create_dir_all(&dir).map_err(|e| format!("create spool dir {}: {e}", dir.display()))?;
    let path = dir.join(format!("{session_id}.done"));
    File::create(&path)
        .map(|_| ())
        .map_err(|e| format!("write done sentinel {}: {e}", path.display()))
}

/// Append a timestamped line to a local error log. Best-effort: if even this fails,
/// there is nowhere left to report it but stderr, which the caller already does.
pub fn log_error_at(path: &Path, msg: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = writeln!(f, "{ts} {msg}");
}

// --- process-environment wrappers, used only by main.rs ---

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn spool_root() -> PathBuf {
    std::env::var_os("CTXLAKE_SPOOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".ctxlake").join("spool"))
}

fn error_log_path() -> PathBuf {
    std::env::var_os("CTXLAKE_HOOK_ERROR_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".ctxlake").join("hook-errors.log"))
}

pub fn append_event(runtime: &str, session_id: &str, line: &str) -> Result<(), String> {
    append_event_at(&spool_root(), runtime, session_id, line)
}

pub fn mark_session_done(runtime: &str, session_id: &str) -> Result<(), String> {
    mark_session_done_at(&spool_root(), runtime, session_id)
}

pub fn log_error(msg: &str) {
    log_error_at(&error_log_path(), msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn appended_lines_read_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        append_event_at(dir.path(), "claude_code", "sess-1", "one").unwrap();
        append_event_at(dir.path(), "claude_code", "sess-1", "two").unwrap();
        append_event_at(dir.path(), "claude_code", "sess-1", "three").unwrap();

        let contents =
            fs::read_to_string(session_file(dir.path(), "claude_code", "sess-1")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines, vec!["one", "two", "three"]);
    }

    #[test]
    fn different_sessions_get_different_files() {
        let dir = tempfile::tempdir().unwrap();
        append_event_at(dir.path(), "cursor", "a", "x").unwrap();
        append_event_at(dir.path(), "cursor", "b", "y").unwrap();
        assert!(session_file(dir.path(), "cursor", "a").exists());
        assert!(session_file(dir.path(), "cursor", "b").exists());
        assert_ne!(
            fs::read_to_string(session_file(dir.path(), "cursor", "a")).unwrap(),
            fs::read_to_string(session_file(dir.path(), "cursor", "b")).unwrap()
        );
    }

    #[test]
    fn full_file_rotates_and_a_later_append_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path(), "claude_code", "sess-big");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Fabricate a file already at the rotation threshold rather than writing
        // 32MB one event at a time.
        fs::write(&path, vec![b'x'; MAX_SPOOL_FILE_BYTES as usize]).unwrap();

        append_event_at(dir.path(), "claude_code", "sess-big", "fresh-line").unwrap();

        // The old, full file was moved aside...
        let rotated: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("sess-big.ndjson.")
            })
            .collect();
        assert_eq!(rotated.len(), 1, "expected exactly one rotated file");
        // ...and the canonical path holds only the new line, not the old 32MB.
        let fresh = fs::read_to_string(&path).unwrap();
        assert_eq!(fresh, "fresh-line\n");
    }

    #[test]
    fn mark_session_done_writes_a_sentinel_next_to_the_ndjson() {
        let dir = tempfile::tempdir().unwrap();
        append_event_at(dir.path(), "claude_code", "sess-1", "line").unwrap();
        mark_session_done_at(dir.path(), "claude_code", "sess-1").unwrap();
        assert!(dir.path().join("claude_code").join("sess-1.done").exists());
    }

    #[test]
    fn over_cap_runtime_dir_drops_events_instead_of_growing_further() {
        let dir = tempfile::tempdir().unwrap();
        let rt_dir = dir.path().join("claude_code");
        fs::create_dir_all(&rt_dir).unwrap();
        // One file already over the cap, standing in for "many sessions' worth".
        fs::write(
            rt_dir.join("huge.ndjson"),
            vec![b'x'; MAX_RUNTIME_DIR_BYTES as usize + 1],
        )
        .unwrap();

        append_event_at(
            dir.path(),
            "claude_code",
            "new-session",
            "should be dropped",
        )
        .unwrap();

        assert!(
            !session_file(dir.path(), "claude_code", "new-session").exists(),
            "a new session file must not be created once the runtime dir is over cap"
        );
    }

    #[test]
    fn concurrent_appends_to_one_session_never_interleave_within_a_line() {
        // The realistic case this guards: two parallel tool calls in the same
        // session each spawn their own hook process and append to the same file.
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(dir.path().to_path_buf());
        const THREADS: usize = 8;
        const LINES_PER_THREAD: usize = 200;

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let root = Arc::clone(&root);
                thread::spawn(move || {
                    for i in 0..LINES_PER_THREAD {
                        // A payload wide enough that a byte-level interleave would be
                        // visible as a line that doesn't match any expected value.
                        let line = format!("thread-{t:02}-line-{i:04}-{}", "x".repeat(200));
                        append_event_at(&root, "claude_code", "shared-session", &line).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let contents =
            fs::read_to_string(session_file(&root, "claude_code", "shared-session")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(
            lines.len(),
            THREADS * LINES_PER_THREAD,
            "a line went missing or two lines merged"
        );

        let mut expected: HashSet<String> = HashSet::new();
        for t in 0..THREADS {
            for i in 0..LINES_PER_THREAD {
                expected.insert(format!("thread-{t:02}-line-{i:04}-{}", "x".repeat(200)));
            }
        }
        let seen: HashSet<&str> = lines.iter().copied().collect();
        assert_eq!(
            seen.len(),
            lines.len(),
            "a duplicate or merged line appeared"
        );
        for line in &lines {
            assert!(
                expected.contains(*line),
                "line does not match any writer's output — interleaved write: {line:?}"
            );
        }
    }

    #[test]
    fn session_id_with_path_traversal_is_rejected_and_writes_nothing_outside_root() {
        // Regression: `session_file` used to join `session_id` into a path with no
        // validation at all, so a crafted id walked out of the spool root.
        let dir = tempfile::tempdir().unwrap();
        // `<root>/claude_code/../../pwned.ndjson` lands here — one level above `root`.
        let escape_target = dir.path().parent().unwrap().join("pwned.ndjson");
        let _ = fs::remove_file(&escape_target);

        let result = append_event_at(dir.path(), "claude_code", "../../pwned", "line");

        assert!(
            result.is_err(),
            "a path-traversal session_id must be rejected, not written"
        );
        assert!(
            !escape_target.exists(),
            "must never write outside the spool root: {}",
            escape_target.display()
        );
        let _ = fs::remove_file(&escape_target); // leave no trace either way
    }

    #[test]
    fn session_id_containing_a_slash_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(append_event_at(dir.path(), "claude_code", "a/b", "line").is_err());
        assert!(!dir.path().join("claude_code").join("a").exists());
    }

    #[test]
    fn session_id_of_bare_dot_dot_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(append_event_at(dir.path(), "claude_code", "..", "line").is_err());
    }

    #[test]
    fn session_id_containing_nul_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(append_event_at(dir.path(), "claude_code", "a\0b", "line").is_err());
    }

    #[test]
    fn mark_session_done_rejects_an_unsafe_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let escape_target = dir.path().parent().unwrap().join("pwned.done");
        let _ = fs::remove_file(&escape_target);

        let result = mark_session_done_at(dir.path(), "claude_code", "../../pwned");

        assert!(result.is_err());
        assert!(!escape_target.exists());
        let _ = fs::remove_file(&escape_target);
    }

    #[test]
    fn ordinary_session_ids_with_dots_or_dashes_still_work() {
        // The validator must reject traversal, not merely "any non-alphanumeric" —
        // real session ids look like UUIDs or `sess.1234`.
        let dir = tempfile::tempdir().unwrap();
        append_event_at(dir.path(), "claude_code", "8f3e-a1.2", "line").unwrap();
        assert!(session_file(dir.path(), "claude_code", "8f3e-a1.2").exists());
    }

    #[test]
    fn size_check_uses_the_tracked_sidecar_not_a_live_directory_scan() {
        // Regression: the cap check used to call `dir_size`, which stats every file
        // in the runtime directory on every event — an O(files) syscall storm on a
        // path budgeted at 5ms p99, and the file count only grows (wave 1 ships no
        // daemon to drain it). Seed a small tracked size and plant a decoy file that
        // would blow the cap if anything actually scanned the directory for real
        // bytes on disk; the append must succeed because it trusts the sidecar.
        let dir = tempfile::tempdir().unwrap();
        let rt_dir = dir.path().join("claude_code");
        fs::create_dir_all(&rt_dir).unwrap();
        fs::write(rt_dir.join(".spool_size"), "10").unwrap();
        fs::write(
            rt_dir.join("decoy.ndjson"),
            vec![b'x'; MAX_RUNTIME_DIR_BYTES as usize + 1],
        )
        .unwrap();

        append_event_at(dir.path(), "claude_code", "sess-1", "line").unwrap();

        assert!(
            session_file(dir.path(), "claude_code", "sess-1").exists(),
            "the tracked sidecar size (10 bytes), not a scan of the decoy file, must \
             decide whether the cap is exceeded"
        );
    }

    #[test]
    fn size_tracking_self_heals_from_a_missing_sidecar() {
        // No sidecar yet (fresh directory, or an upgrade from a build that predates
        // it): the real over-cap file must still be caught by falling back to a scan
        // once, exactly like `over_cap_runtime_dir_drops_events_instead_of_growing_further`.
        let dir = tempfile::tempdir().unwrap();
        let rt_dir = dir.path().join("claude_code");
        fs::create_dir_all(&rt_dir).unwrap();
        fs::write(
            rt_dir.join("huge.ndjson"),
            vec![b'x'; MAX_RUNTIME_DIR_BYTES as usize + 1],
        )
        .unwrap();

        append_event_at(dir.path(), "claude_code", "new-session", "dropped").unwrap();

        assert!(!session_file(dir.path(), "claude_code", "new-session").exists());
        assert!(
            rt_dir.join(".spool_size").exists(),
            "the scan's result should be persisted so the next call is O(1)"
        );
    }

    #[test]
    fn log_error_appends_a_timestamped_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook-errors.log");
        log_error_at(&path, "first failure");
        log_error_at(&path, "second failure");
        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("first failure"));
        assert!(lines[1].ends_with("second failure"));
    }
}
