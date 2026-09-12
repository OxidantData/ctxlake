//! Per-session upload watermark: how much of a spool file this daemon has already
//! confirmed written to the store, and the next segment number to use.
//!
//! This is the resumability contract the task brief asks for in one sentence: "no
//! duplicate or lost events" after `kill -9`. The watermark only ever advances
//! *after* [`crate::upload`] observes the store `PUT` succeed — never before, and
//! never speculatively — so a crash at any point replays exactly the lines that
//! were never confirmed, and no others. See [`crate::atomic_file`] for why the
//! write of *this file itself* can't be allowed to tear: a torn watermark on
//! restart is a worse failure than a torn cache, because it can silently turn into
//! re-uploading already-confirmed data as new (duplicate) rows instead of merely
//! serving one stale read.
//!
//! Stored at `<spool_root>/<runtime>/.upload/<session_id>.json` — the leading dot
//! and the extra directory keep it out of the plain `*.ndjson` glob a spool listing
//! walks, so the state file is never mistaken for a session to upload.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_file::write_atomic;

/// One session's upload progress.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionUploadState {
    /// `dt=` partition value, fixed the first time this session is ever uploaded.
    /// Without pinning it, a session that straddles midnight would split its
    /// segment stream across two `dt=` partitions mid-stream depending on when each
    /// batch happened to run — harmless for correctness (every segment is still
    /// findable by `session=` regardless of which `dt=` it landed under) but
    /// surprising for anyone partition-pruning by date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_date: Option<String>,
    /// Next `seg-<n>` to use. Shared across every physical spool file this
    /// session's rotation produced (see `spool.rs`'s `MAX_SPOOL_FILE_BYTES`) so
    /// segment numbers stay contiguous even though `files` tracks each physical
    /// file's own read offset independently.
    #[serde(default)]
    pub next_seg: u32,
    /// Per-physical-file byte offset already confirmed written to the store, keyed
    /// by file name only (the directory is implied by where this state file lives).
    #[serde(default)]
    pub files: BTreeMap<String, u64>,
    /// Set once `_SEALED` has been written to the store for this session. Once
    /// true, [`crate::upload`] treats every file as closed and moves on to local
    /// cleanup instead of re-scanning them for new bytes every cycle.
    #[serde(default)]
    pub sealed_in_store: bool,
}

impl SessionUploadState {
    pub fn confirmed_bytes(&self, file_name: &str) -> u64 {
        self.files.get(file_name).copied().unwrap_or(0)
    }
}

/// Where a session's watermark lives, given the spool root and (runtime, session).
pub fn state_path(spool_root: &Path, runtime: &str, session_id: &str) -> PathBuf {
    spool_root
        .join(runtime)
        .join(".upload")
        .join(format!("{session_id}.json"))
}

/// Read a session's watermark. A never-touched session (no prior upload attempt)
/// reads back as the default state, not an error — this is the ordinary case for
/// every session's first cycle. A file that exists but fails to parse is a
/// different situation entirely: our own writes are atomic (`atomic_file`), so a
/// corrupt file here did not come from our own crash path, and silently treating it
/// as "never touched" risks re-uploading already-confirmed lines as duplicate
/// segments — the one thing this module exists to prevent. Surface it instead.
pub fn read(path: &Path) -> Result<SessionUploadState, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("corrupt upload watermark {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SessionUploadState::default()),
        Err(e) => Err(format!("read upload watermark {}: {e}", path.display())),
    }
}

/// Persist a session's watermark atomically.
pub fn write(path: &Path, state: &SessionUploadState) -> Result<(), String> {
    let bytes = serde_json::to_vec(state).map_err(|e| e.to_string())?;
    write_atomic(path, &bytes)
        .map_err(|e| format!("write upload watermark {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_never_touched_session_reads_as_the_default_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path(), "claude_code", "sess-1");
        let state = read(&path).unwrap();
        assert_eq!(state, SessionUploadState::default());
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path(), "claude_code", "sess-1");
        let mut state = SessionUploadState {
            partition_date: Some("2026-09-11".into()),
            next_seg: 3,
            ..Default::default()
        };
        state.files.insert("sess-1.ndjson".into(), 4096);
        write(&path, &state).unwrap();

        let back = read(&path).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn a_corrupt_watermark_is_an_error_not_a_silent_reset() {
        // The regression this guards: silently treating unreadable state as "start
        // over" would re-upload already-confirmed lines as new, duplicate segments
        // — exactly the failure mode "resumable with no duplicates" forbids.
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path(), "claude_code", "sess-1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not valid json").unwrap();

        let err = read(&path).unwrap_err();
        assert!(err.contains("corrupt"), "got: {err}");
    }

    #[test]
    fn state_path_keeps_sessions_and_watermarks_from_colliding() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = state_path(dir.path(), "claude_code", "sess-1");
        let p2 = state_path(dir.path(), "cursor", "sess-1");
        assert_ne!(p1, p2, "different runtimes must not share a watermark file");
        assert!(p1.to_string_lossy().contains(".upload"));
    }
}
