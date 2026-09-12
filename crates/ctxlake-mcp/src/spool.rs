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

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use serde::Serialize;

/// Append one record to `<root>/mcp/<fleet_id>.ndjson`. `record` is serialized as a
/// single compact JSON line; the caller is responsible for making sure its shape
/// carries a `kind` field, since that is what a future reader dispatches on.
///
/// Returns the serialized line so callers (and tests) can echo back exactly what
/// was queued without re-reading the file.
pub fn append_at<T: Serialize>(root: &Path, fleet_id: &str, record: &T) -> Result<String, String> {
    crate::paths::validate_segment("fleet_id", fleet_id)?;
    let dir = root.join("mcp");
    fs::create_dir_all(&dir).map_err(|e| format!("create spool dir {}: {e}", dir.display()))?;
    let path = dir.join(format!("{fleet_id}.ndjson"));

    let line = serde_json::to_string(record).map_err(|e| format!("encode record: {e}"))?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open spool file {}: {e}", path.display()))?;
    f.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("append to {}: {e}", path.display()))?;
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
}
