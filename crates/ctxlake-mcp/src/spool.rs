//! Where write-shaped tool calls land: `fleet_claim`, `fleet_release`,
//! `fleet_handoff`, and `memory_propose` all append one JSON line here rather than
//! touching the object store — see `paths.rs` and AGENTS.md invariant 1.
//!
//! Layout: `<spool_root>/mcp/<fleet_id>.ndjson`, one record per line, each tagged
//! with `kind` so a future `ctxlake sync` can dispatch on it (a lease acquire
//! attempt for `claim_request`, a CAS release for `release_request`, an append to
//! `claims/events/` for `claim_propose`, a handoff note for whoever picks up this
//! repo next). Nothing in this codebase drains this file yet — that daemon-side
//! consumer is later work — so every tool that appends here must say so honestly in
//! its own result rather than implying the write already reached the fleet.
//!
//! The append discipline mirrors `ctxlake-hook`'s `spool.rs` exactly, for the exact
//! same reason: `O_APPEND` plus one `write_all` per line keeps concurrent writers
//! (two tool calls in flight at once) from interleaving mid-line, without needing a
//! lock file.
//!
//! ## Disk-fill protection
//!
//! `ctxlake-hook`'s own spool caps a runtime directory's total size and rotates any
//! one session's file once it gets big, because "Wave 1 ships no daemon to drain
//! the spool yet, so an idle laptop must not fill its disk silently" — see that
//! module's docs. The identical gap exists here: nothing drains `mcp/*.ndjson`
//! either, every agent in a fleet shares one file per fleet, and every write-shaped
//! tool call (`fleet_claim`, `fleet_release`, `fleet_handoff`, `memory_propose`)
//! appends to it. An agent looping on any of those grows that one file without
//! bound unless something here refuses to keep growing it. Unlike the hook, this
//! crate has a real return channel to the caller — a JSON-RPC tool result — so
//! hitting the cap comes back as an honest error the calling agent can see and act
//! on, rather than the hook's silent drop (which is right for a path that must
//! never fail the host agent's turn, and wrong for one that can just say no).

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Rotate a fleet's ndjson file once it reaches this size, so one long-lived fleet
/// cannot produce a single file the (not-yet-built) daemon must read whole.
/// Mirrors `ctxlake_hook::spool::MAX_SPOOL_FILE_BYTES`'s value and reasoning.
pub const MAX_SPOOL_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Refuse to grow the `mcp/` spool directory past this and error out instead —
/// see the module doc's "Disk-fill protection" section. Same value as
/// `ctxlake_hook::spool::MAX_RUNTIME_DIR_BYTES`: arbitrary and generous, not a
/// promise of exactness.
pub const MAX_RUNTIME_DIR_BYTES: u64 = 512 * 1024 * 1024;

const SIZE_SIDECAR_NAME: &str = ".spool_size";

/// Sum of file sizes directly inside `dir` (not recursive) — the expensive
/// fallback used only to seed the sidecar once. See
/// `ctxlake_hook::spool::dir_size`'s docs for why the steady-state path must not
/// call this on every append.
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

/// The `mcp/` directory's tracked size, read from a small sidecar file instead of
/// stat-ing every fleet's ndjson file on every single tool call. Falls back to a
/// real scan exactly once (a fresh directory, or one written before this sidecar
/// existed) and persists the result from then on. Concurrent writers can race the
/// read-here / write-in-`write_tracked_size` round trip and under- or over-count by
/// a line or two, which is fine for a cap already documented as advisory.
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

/// Persist the tracked size. Best-effort: a failed write just means the next call
/// falls back to a real scan again, which is correct, if slow.
fn write_tracked_size(dir: &Path, size: u64) {
    let _ = fs::write(dir.join(SIZE_SIDECAR_NAME), size.to_string());
}

/// Rename a full spool file out of the way so the next append starts a fresh one.
/// Best-effort, not a lock — see `ctxlake_hook::spool::rotate_if_full`'s docs for
/// why a losing race here is harmless.
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

/// Append one record to `<root>/mcp/<fleet_id>.ndjson`. `record` is serialized as a
/// single compact JSON line; the caller is responsible for making sure its shape
/// carries a `kind` field, since that is what a future reader dispatches on.
///
/// Returns the serialized line so callers (and tests) can echo back exactly what
/// was queued without re-reading the file. Returns `Err` — surfaced to the calling
/// agent as a normal tool-call failure, not a silent drop — once the `mcp/`
/// directory is at or over [`MAX_RUNTIME_DIR_BYTES`]; see the module doc.
pub fn append_at<T: Serialize>(root: &Path, fleet_id: &str, record: &T) -> Result<String, String> {
    crate::paths::validate_segment("fleet_id", fleet_id)?;
    let dir = root.join("mcp");
    fs::create_dir_all(&dir).map_err(|e| format!("create spool dir {}: {e}", dir.display()))?;

    let tracked = read_or_seed_tracked_size(&dir);
    if tracked >= MAX_RUNTIME_DIR_BYTES {
        return Err(format!(
            "mcp spool directory {} is at or over {MAX_RUNTIME_DIR_BYTES} bytes; \
             refusing to queue this write until ctxlake sync drains it",
            dir.display()
        ));
    }

    let path = dir.join(format!("{fleet_id}.ndjson"));
    rotate_if_full(&path);

    let line = serde_json::to_string(record).map_err(|e| format!("encode record: {e}"))?;
    let bytes = format!("{line}\n");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open spool file {}: {e}", path.display()))?;
    f.write_all(bytes.as_bytes())
        .map_err(|e| format!("append to {}: {e}", path.display()))?;
    write_tracked_size(&dir, tracked + bytes.len() as u64);
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn appends_one_json_line_per_call() {
        let dir = tempfile::tempdir().unwrap();
        append_at(dir.path(), "oxidant", &json!({"kind": "a"})).unwrap();
        append_at(dir.path(), "oxidant", &json!({"kind": "b"})).unwrap();

        let contents = fs::read_to_string(dir.path().join("mcp").join("oxidant.ndjson")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"a\""));
        assert!(lines[1].contains("\"b\""));
    }

    #[test]
    fn distinct_fleets_get_distinct_files() {
        let dir = tempfile::tempdir().unwrap();
        append_at(dir.path(), "fleet-a", &json!({"kind": "x"})).unwrap();
        append_at(dir.path(), "fleet-b", &json!({"kind": "y"})).unwrap();
        assert!(dir.path().join("mcp").join("fleet-a.ndjson").exists());
        assert!(dir.path().join("mcp").join("fleet-b.ndjson").exists());
    }

    #[test]
    fn traversal_fleet_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = append_at(dir.path(), "../../pwned", &json!({"kind": "x"}));
        assert!(result.is_err());
        assert!(!dir.path().parent().unwrap().join("pwned.ndjson").exists());
    }

    #[test]
    fn full_file_rotates_and_a_later_append_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_dir = dir.path().join("mcp");
        fs::create_dir_all(&mcp_dir).unwrap();
        let path = mcp_dir.join("oxidant.ndjson");
        // Fabricate a file already at the rotation threshold rather than writing
        // 32MB one line at a time.
        fs::write(&path, vec![b'x'; MAX_SPOOL_FILE_BYTES as usize]).unwrap();

        append_at(dir.path(), "oxidant", &json!({"kind": "fresh"})).unwrap();

        let rotated: Vec<_> = fs::read_dir(&mcp_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("oxidant.ndjson.")
            })
            .collect();
        assert_eq!(rotated.len(), 1, "expected exactly one rotated file");
        let fresh = fs::read_to_string(&path).unwrap();
        assert!(fresh.contains("\"fresh\""));
        assert!(!fresh.contains('x'));
    }

    /// Regression for the finding: nothing previously stopped a fleet's spool
    /// directory from growing without bound. One agent looping on any
    /// write-shaped tool must eventually get told no, not silently keep
    /// appending to a file heading for "fills the disk."
    #[test]
    fn over_cap_mcp_dir_refuses_new_writes_instead_of_growing_further() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_dir = dir.path().join("mcp");
        fs::create_dir_all(&mcp_dir).unwrap();
        // One file already over the cap, standing in for "many proposals' worth."
        fs::write(
            mcp_dir.join("huge.ndjson"),
            vec![b'x'; MAX_RUNTIME_DIR_BYTES as usize + 1],
        )
        .unwrap();

        let result = append_at(dir.path(), "oxidant", &json!({"kind": "should be refused"}));
        assert!(result.is_err());
        assert!(
            !mcp_dir.join("oxidant.ndjson").exists(),
            "a new fleet file must not be created once the mcp spool dir is over cap"
        );
    }
}
