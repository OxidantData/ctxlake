//! UPLOAD: tail the local spool, batch, write `sessions/.../seg-<n>.parquet`, seal
//! on the `.done` sentinel. See `docs/architecture.md`'s write-path sequence
//! diagram and AGENTS.md invariant 1 — this module, run by [`crate::run`], is the
//! only thing standing between the hook's local spool and the object store.
//!
//! ## Resumability
//!
//! [`watermark::SessionUploadState`] is the durability contract: a byte range is
//! never considered "uploaded" until the store `PUT` for it returns success, and the
//! watermark recording that is never considered "saved" until its own atomic write
//! (`atomic_file`) lands. Between those two facts, a `kill -9` at any instant
//! resumes into one of exactly two states — the batch was never sent (retried in
//! full) or it was sent and recorded (not retried at all) — never a partial replay
//! and never a silent gap. See the integration test at the bottom of this file for
//! the actual kill-and-resume scenario.
//!
//! ## Why sessions can have more than one physical spool file
//!
//! `ctxlake-hook`'s spool rotates a session's `.ndjson` file once it crosses
//! `MAX_SPOOL_FILE_BYTES` (32 MiB), renaming the full file aside and starting a
//! fresh one at the canonical path. [`session_files`] discovers every physical file
//! for a session — rotated ones (processed oldest first) and the canonical one
//! (always last, since it is the only one still being appended to) — while
//! [`watermark::SessionUploadState::next_seg`] keeps segment numbers contiguous
//! across all of them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ctxlake_core::{Envelope, Runtime};
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::backoff::Backoff;
use crate::codec;
use crate::watermark::{self, SessionUploadState};

/// Static configuration the upload loop needs and never mutates.
#[derive(Debug, Clone)]
pub struct UploadConfig {
    pub fleet_id: String,
    pub agent_id: String,
    pub spool_root: PathBuf,
}

/// One `(runtime, session_id)` pair found under the spool root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionKey {
    pub runtime: String,
    pub session_id: String,
}

/// A physical spool file backing (part of) one session's line stream.
#[derive(Debug, Clone)]
struct SpoolFileRef {
    path: PathBuf,
    file_name: String,
    /// Stable identity of the underlying inode (`"<dev>:<ino>"`), used as the
    /// watermark key instead of `file_name` — see that field's replacement in
    /// [`watermark::SessionUploadState::files`] for why a *name*-keyed watermark is
    /// unsound across a rotation.
    identity: String,
}

/// `"<dev>:<ino>"` for a file's metadata. POSIX guarantees `rename(2)` does not
/// change a file's inode — the same guarantee `ctxlake-hook`'s own module doc
/// already leans on for its rotation being safe against a concurrent writer's open
/// handle (`spool.rs`'s `rotate_if_full` doc) — so this identity survives exactly
/// the rename that a file *name* does not, which is the property this module needs:
/// the file that was 90% uploaded under the name `sess.ndjson` is the *same* file,
/// carrying the *same* confirmed offset, after `ctxlake-hook` renames it aside to
/// `sess.ndjson.<ts>` mid-upload-cycle. A freshly-created canonical file after
/// rotation gets a new inode and therefore correctly starts unconfirmed at offset 0
/// — no code here has to special-case "this name looks like a rotation."
///
/// This crate targets the same platforms `ctxlake-hook`'s own spool already
/// assumes (`spool.rs` reads `HOME`, leans on POSIX `O_APPEND` and `rename(2)`
/// atomicity) — Unix only, no Windows fallback attempted.
///
/// One accepted, narrow limitation: if a session's watermark survives long enough
/// (it now does — see [`maybe_seal_and_cleanup`]) that the OS reuses an old,
/// deleted file's exact `(dev, ino)` pair for an unrelated *new* file in the same
/// session, that new file would be misread as already-confirmed. This requires the
/// same session_id to be reused after its files were deleted *and* an inode number
/// collision, both independently unlikely; nothing here defends against it, and if
/// it ever shows up in practice a generation counter alongside the offset is the
/// fix, not a rewrite of this scheme.
fn file_identity(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", meta.dev(), meta.ino())
}

/// The inverse of `ctxlake_core::envelope::Runtime::as_str()`. Kept local rather
/// than pulled from `ctxlake-hook` (a separate *binary* crate this daemon has no
/// other reason to depend on) for one match arm — see that crate's
/// `adapters::parse_runtime` for the canonical version this must stay in sync with.
fn runtime_from_dir_name(s: &str) -> Runtime {
    match s {
        "claude_code" => Runtime::ClaudeCode,
        "cursor" => Runtime::Cursor,
        "hermes" => Runtime::Hermes,
        _ => Runtime::Other,
    }
}

/// Walk `spool_root` and return every distinct `(runtime, session_id)` with at
/// least one spool file on disk. Best-effort: a directory that vanishes mid-walk
/// (a session finishing cleanup concurrently) is treated as empty, not an error —
/// there is nothing to upload from a directory that is no longer there.
pub fn discover_sessions(spool_root: &Path) -> Vec<SessionKey> {
    let mut out: std::collections::BTreeSet<SessionKey> = std::collections::BTreeSet::new();
    let Ok(runtime_dirs) = std::fs::read_dir(spool_root) else {
        return Vec::new();
    };
    for rt_entry in runtime_dirs.filter_map(Result::ok) {
        if !rt_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(runtime) = rt_entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(files) = std::fs::read_dir(rt_entry.path()) else {
            continue;
        };
        for f in files.filter_map(Result::ok) {
            if !f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = f.file_name().to_string_lossy().to_string();
            if let Some(session_id) = session_id_from_file_name(&name) {
                out.insert(SessionKey {
                    runtime: runtime.clone(),
                    session_id,
                });
            }
        }
    }
    out.into_iter().collect()
}

/// `"<session_id>.ndjson"` or `"<session_id>.ndjson.<rotation-suffix>"` -> the
/// session id, or `None` for anything else in the directory (`.done` sentinels, the
/// `.spool_size` sidecar, the `.upload/` state directory).
fn session_id_from_file_name(name: &str) -> Option<String> {
    const MARK: &str = ".ndjson";
    let idx = name.find(MARK)?;
    let (session_id, after) = (&name[..idx], &name[idx + MARK.len()..]);
    if session_id.is_empty() || session_id.starts_with('.') {
        return None;
    }
    let is_canonical = after.is_empty();
    let is_rotated =
        after.len() > 1 && after.starts_with('.') && after[1..].bytes().all(|b| b.is_ascii_digit());
    (is_canonical || is_rotated).then(|| session_id.to_string())
}

/// Sort key for [`session_files`]: rotated files ascending by their numeric suffix
/// (oldest data first), the canonical file always last (it is the only file still
/// being appended to, so it necessarily holds the newest data).
fn rotation_key(file_name: &str, session_id: &str) -> u128 {
    let canonical = format!("{session_id}.ndjson");
    if file_name == canonical {
        return u128::MAX;
    }
    file_name
        .strip_prefix(&format!("{canonical}."))
        .and_then(|suffix| suffix.parse::<u128>().ok())
        .unwrap_or(u128::MAX - 1) // an unrecognized suffix shape: process before canonical, never after.
}

fn session_files(spool_root: &Path, runtime: &str, session_id: &str) -> Vec<SpoolFileRef> {
    let dir = spool_root.join(runtime);
    let canonical = format!("{session_id}.ndjson");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<SpoolFileRef> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name == canonical || name.starts_with(&format!("{canonical}."))) {
                return None;
            }
            // A file that fails to stat here (removed concurrently, e.g. by a
            // cleanup racing this same walk) is skipped, not an error — same
            // best-effort tolerance `discover_sessions`'s own doc states for a
            // directory that vanishes mid-walk.
            let meta = e.metadata().ok()?;
            Some(SpoolFileRef {
                path: e.path(),
                identity: file_identity(&meta),
                file_name: name,
            })
        })
        .collect();
    files.sort_by_key(|f| rotation_key(&f.file_name, session_id));
    files
}

/// Read every *complete* line beyond `from_offset`. A trailing partial line (bytes
/// written by an in-flight append that hasn't reached a newline yet) is left
/// unread and simply picked up next cycle — `spool.rs`'s own docs guarantee a line
/// only ever appears once its one `write_all` syscall for the whole line has
/// completed, so "no trailing newline yet" means "still being written," not
/// "corrupt."
fn read_new_complete_lines(path: &Path, from_offset: u64) -> Result<(Vec<String>, u64), String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = f
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    if from_offset >= len {
        return Ok((Vec::new(), from_offset));
    }
    f.seek(SeekFrom::Start(from_offset))
        .map_err(|e| format!("seek {}: {e}", path.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| format!("read {}: {e}", path.display()))?;

    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok((Vec::new(), from_offset));
    };
    let complete = &buf[..=last_nl];
    let new_offset = from_offset + complete.len() as u64;
    let lines: Vec<String> = String::from_utf8_lossy(complete)
        .lines()
        .map(str::to_string)
        .collect();
    Ok((lines, new_offset))
}

/// What one [`upload_session_once`] call actually did, for the loop's own logging
/// and tests — never used to decide correctness (the watermark is what's durable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadOutcome {
    pub segments_written: u32,
    pub sealed: bool,
    pub cleaned_up: bool,
}

/// Process one session's pending spool data exactly once: read new complete lines
/// from every physical file (oldest rotation first), encode and `PUT` one segment
/// per file that had new data, persist the watermark after *each* successful
/// segment (not once at the end — see the module doc), then seal and clean up if
/// the session is done.
pub async fn upload_session_once(
    store: &dyn ObjectStore,
    cfg: &UploadConfig,
    key: &SessionKey,
) -> Result<UploadOutcome, String> {
    let state_path = watermark::state_path(&cfg.spool_root, &key.runtime, &key.session_id);
    let mut state = watermark::read(&state_path)?;
    let runtime = runtime_from_dir_name(&key.runtime);

    let files = session_files(&cfg.spool_root, &key.runtime, &key.session_id);
    let mut segments_written = 0u32;

    for file in &files {
        let confirmed = state.confirmed_bytes(&file.identity);
        let (lines, new_offset) = read_new_complete_lines(&file.path, confirmed)?;
        if new_offset == confirmed {
            continue; // nothing new in this file this cycle.
        }

        let envelopes: Vec<Envelope> = lines
            .iter()
            .filter_map(|line| match serde_json::from_str(line) {
                Ok(e) => Some(e),
                Err(e) => {
                    // A malformed line is dropped, not fatal: bronze already lost
                    // nothing it could have kept (the line was never valid), and
                    // refusing to advance past it would wedge this session's
                    // upload forever on one bad line.
                    tracing::warn!(file = %file.path.display(), error = %e, "dropping unparseable spool line");
                    None
                }
            })
            .collect();

        if !envelopes.is_empty() {
            if state.partition_date.is_none() {
                state.partition_date = Some(partition_date_of(&envelopes[0]));
            }
            let date = state.partition_date.clone().expect("just set above");
            let bytes = codec::encode(&envelopes)?;
            let seg = state.next_seg;
            let key_path = ctxlake_store::layout::session_segment(
                &date,
                &cfg.fleet_id,
                runtime,
                &cfg.agent_id,
                &key.session_id,
                seg,
            );
            store
                .put(&key_path, PutPayload::from(bytes))
                .await
                .map_err(|e| format!("PUT {key_path}: {e}"))?;
            // The write is confirmed; only now does it become safe to say so. If
            // the process dies between here and the watermark write below, restart
            // re-reads the same lines and writes the same `seg-<n>` key again —
            // overwriting it with byte-identical content, which is a no-op on a
            // key only this session's own daemon ever writes (AGENTS.md invariant
            // 3: single-writer append; a single writer overwriting its own key
            // with the same bytes is not a second writer).
            state.next_seg += 1;
            segments_written += 1;
        }

        state.files.insert(file.identity.clone(), new_offset);
        watermark::write(&state_path, &state)?;
    }

    let (sealed, cleaned_up) =
        maybe_seal_and_cleanup(store, cfg, key, &mut state, &state_path, runtime, &files).await?;

    Ok(UploadOutcome {
        segments_written,
        sealed,
        cleaned_up,
    })
}

/// First 10 characters of `emitted_at` (`YYYY-MM-DDTHH:MM:SS.mmmZ` ->
/// `YYYY-MM-DD`) — `ctxlake_hook::clock::now_rfc3339`'s format guarantees this slice
/// is always the calendar date, so no date parsing library is needed for a value
/// this daemon only ever uses as an opaque partition string.
fn partition_date_of(e: &Envelope) -> String {
    e.emitted_at.get(0..10).unwrap_or(&e.emitted_at).to_string()
}

/// Name of `ctxlake-hook`'s tracked-size sidecar (`crates/ctxlake-hook/src/spool.rs`'s
/// `SIZE_SIDECAR_NAME`), duplicated here for the same reason `runtime_from_dir_name`
/// above duplicates one match arm rather than pulling in that crate — must stay in
/// sync with that constant.
const HOOK_SIZE_SIDECAR_NAME: &str = ".spool_size";

/// Subtract `freed` bytes from a runtime directory's tracked-size sidecar.
///
/// `ctxlake-hook`'s `append_event_at` only ever increments this counter
/// (`spool.rs`'s `write_tracked_size`) — it has no reason to decrement it, because
/// wave 1 shipped no drainer. This crate *is* that drainer, and cleanup deletes the
/// very files those bytes were counted for; without this call the counter only
/// ratchets upward forever; once enough uploaded-and-reclaimed sessions push it past
/// `MAX_RUNTIME_DIR_BYTES` (512 MiB), the hook silently drops every new event
/// against an empty spool directory, with no error any runtime surfaces (the hook's
/// warning goes to stderr, which callers discard).
///
/// Best-effort, like the sidecar itself already is documented to be (`spool.rs`:
/// "under- or over-count by a line or two ... acceptable for a cap that is already
/// ... advisory disk-fill protection, not for anything that needs to be exact"): a
/// concurrent hook append between this read and this write loses a few bytes of the
/// decrement, which cannot compound into a permanent drift, because the hook only
/// ever trusts this sidecar's *value* — it never re-derives it from a real scan
/// unless the sidecar file is missing entirely.
fn decrement_tracked_size(dir: &Path, freed: u64) {
    if freed == 0 {
        return;
    }
    let sidecar = dir.join(HOOK_SIZE_SIDECAR_NAME);
    let current = std::fs::read_to_string(&sidecar)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let _ = std::fs::write(&sidecar, current.saturating_sub(freed).to_string());
}

/// If the session's `.done` sentinel exists and every known physical file has been
/// fully consumed, write `_SEALED` (idempotent: a repeat write to a key only this
/// daemon ever produces for this session is harmless) and delete the local spool
/// files — the whole point of "removed only after confirmed" (module doc): nothing
/// is deleted here that isn't already durable in the store.
async fn maybe_seal_and_cleanup(
    store: &dyn ObjectStore,
    cfg: &UploadConfig,
    key: &SessionKey,
    state: &mut SessionUploadState,
    state_path: &Path,
    runtime: Runtime,
    files: &[SpoolFileRef],
) -> Result<(bool, bool), String> {
    let dir = cfg.spool_root.join(&key.runtime);
    let done_marker = dir.join(format!("{}.done", key.session_id));
    if !done_marker.exists() {
        return Ok((false, false));
    }

    let fully_consumed = files.iter().all(|f| {
        let on_disk = std::fs::metadata(&f.path)
            .map(|m| m.len())
            .unwrap_or(u64::MAX);
        state.confirmed_bytes(&f.identity) == on_disk
    });
    if !fully_consumed {
        return Ok((false, false));
    }

    let mut just_sealed = false;
    if !state.sealed_in_store {
        // No envelopes at all is a legitimate (if unusual) sealed session — an
        // empty `.ndjson` with only a `.done` next to it, e.g. a session that
        // exited before any tool call landed. `partition_date` would be unset in
        // that case; fall back to the store's own clock via a plain "now" so
        // `_SEALED` still has a home instead of skipping the seal forever.
        let date = state.partition_date.clone().unwrap_or_else(today_utc_date);
        let sealed_path = ctxlake_store::layout::session_sealed(
            &date,
            &cfg.fleet_id,
            runtime,
            &cfg.agent_id,
            &key.session_id,
        );
        let marker = serde_json::json!({ "sealed_at": today_rfc3339() });
        let bytes = serde_json::to_vec(&marker).map_err(|e| e.to_string())?;
        store
            .put(&sealed_path, PutPayload::from(bytes))
            .await
            .map_err(|e| format!("PUT {sealed_path}: {e}"))?;
        state.sealed_in_store = true;
        watermark::write(state_path, state)?;
        just_sealed = true;
    }

    // Sizes must be read before deletion — there is nothing left to stat after.
    let freed: u64 = files
        .iter()
        .map(|f| std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0))
        .sum();
    for f in files {
        let _ = std::fs::remove_file(&f.path);
    }
    decrement_tracked_size(&dir, freed);
    let _ = std::fs::remove_file(&done_marker);
    // The watermark itself is deliberately NOT deleted here (a past version of this
    // function did, and it was a bronze-corrupting bug): `docs/runtimes/claude-code.md`
    // documents that `--resume`/`--continue` reuses `session_id`, so a `.ndjson` can
    // reappear at this same path days later. Deleting the watermark would reset
    // `next_seg` to 0 and `sealed_in_store` to `false`, and the very first segment the
    // resumed session uploads would `PUT` straight over `seg-000000` — a key this
    // session already told the store, via `_SEALED`, was finished — destroying
    // whatever was in it. Keeping the (tiny — a handful of JSON fields) watermark
    // file forever means a resumed session's next upload sees `sealed_in_store:
    // true` and `next_seg` picking up where it left off, so a new segment is
    // *appended* under a fresh number rather than *overwriting* an old one. The
    // traded-off cost is one small file per session_id that has ever existed,
    // persisting past cleanup — `ctxlake maint` is the natural place to eventually
    // garbage-collect these against `sessions/.../_SEALED` ages, not this loop.

    Ok((just_sealed || state.sealed_in_store, true))
}

fn today_utc_date() -> String {
    today_rfc3339()
        .get(0..10)
        .unwrap_or("1970-01-01")
        .to_string()
}

fn today_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Run the upload loop until `shutdown` fires: each tick, discover every session
/// under the spool root and upload whatever is pending, backing off on a genuine
/// store error rather than hot-looping against an outage (module doc; the "spool
/// keeps growing" symptom in `docs/architecture.md`'s failure table is exactly this
/// backoff doing its job, not a hang).
pub async fn run(
    store: Arc<dyn ObjectStore>,
    cfg: UploadConfig,
    poll_interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = Backoff::new(Duration::from_millis(200), Duration::from_secs(30));
    loop {
        if *shutdown.borrow() {
            break;
        }
        let sessions = discover_sessions(&cfg.spool_root);
        let mut had_error = false;
        for key in &sessions {
            match upload_session_once(store.as_ref(), &cfg, key).await {
                Ok(_) => {}
                Err(e) => {
                    had_error = true;
                    tracing::warn!(runtime = %key.runtime, session_id = %key.session_id, error = %e, "upload cycle failed for session");
                }
            }
        }

        let delay = if had_error {
            backoff.next_delay()
        } else {
            backoff.reset();
            poll_interval
        };

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::EventType;
    use object_store::memory::InMemory;
    use std::io::Write as _;

    fn cfg(spool_root: &Path) -> UploadConfig {
        UploadConfig {
            fleet_id: "oxidant".into(),
            agent_id: "cc-01".into(),
            spool_root: spool_root.to_path_buf(),
        }
    }

    fn append_line(spool_root: &Path, runtime: &str, session_id: &str, line: &str) {
        let dir = spool_root.join(runtime);
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{session_id}.ndjson")))
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn envelope_line(session_id: &str, n: u32) -> String {
        let mut e = Envelope::new(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            session_id,
            EventType::ToolCall,
            format!("2026-09-11T18:22:{n:02}.000Z"),
        );
        e.content = Some(format!("line {n}"));
        e.to_ndjson().unwrap()
    }

    fn mark_done(spool_root: &Path, runtime: &str, session_id: &str) {
        let dir = spool_root.join(runtime);
        std::fs::write(dir.join(format!("{session_id}.done")), b"").unwrap();
    }

    async fn read_segment_envelopes(
        store: &dyn ObjectStore,
        path: &object_store::path::Path,
    ) -> Vec<Envelope> {
        let bytes = store.get(path).await.unwrap().bytes().await.unwrap();
        codec::decode(&bytes).unwrap()
    }

    #[test]
    fn discover_sessions_finds_canonical_and_ignores_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        append_line(dir.path(), "claude_code", "sess-1", "{}");
        std::fs::write(dir.path().join("claude_code").join("sess-1.done"), b"").unwrap();
        std::fs::write(dir.path().join("claude_code").join(".spool_size"), b"10").unwrap();
        std::fs::create_dir_all(dir.path().join("claude_code").join(".upload")).unwrap();

        let sessions = discover_sessions(dir.path());
        assert_eq!(
            sessions,
            vec![SessionKey {
                runtime: "claude_code".into(),
                session_id: "sess-1".into()
            }]
        );
    }

    #[test]
    fn session_files_orders_rotated_before_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let rt_dir = dir.path().join("claude_code");
        std::fs::create_dir_all(&rt_dir).unwrap();
        std::fs::write(rt_dir.join("sess-1.ndjson.100"), b"old").unwrap();
        std::fs::write(rt_dir.join("sess-1.ndjson.50"), b"older").unwrap();
        std::fs::write(rt_dir.join("sess-1.ndjson"), b"newest").unwrap();

        let files = session_files(dir.path(), "claude_code", "sess-1");
        let names: Vec<&str> = files.iter().map(|f| f.file_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["sess-1.ndjson.50", "sess-1.ndjson.100", "sess-1.ndjson"]
        );
    }

    #[tokio::test]
    async fn spool_to_store_round_trip_uploads_and_advances_the_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-1".into(),
        };

        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 0),
        );
        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 1),
        );

        let outcome = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(outcome.segments_written, 1);
        assert!(!outcome.sealed);

        let seg_path = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        let envelopes = read_segment_envelopes(&store, &seg_path).await;
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].content.as_deref(), Some("line 0"));
        assert_eq!(envelopes[1].content.as_deref(), Some("line 1"));

        // A second cycle with nothing new must not re-upload or advance seg again.
        let outcome2 = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(
            outcome2.segments_written, 0,
            "no new lines: nothing to upload"
        );
    }

    #[tokio::test]
    async fn resumes_after_a_simulated_kill_9_with_no_duplicates_and_no_loss() {
        // The scenario the task brief names explicitly: two batches are written
        // across two independent `upload_session_once` calls against fresh state
        // read from disk each time — exactly what a process restart looks like,
        // since nothing here is held in memory across calls except what the
        // watermark file itself durably records.
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-1".into(),
        };

        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 0),
        );
        let outcome1 = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(outcome1.segments_written, 1);

        // "Kill -9": drop everything in-process, re-derive state purely from disk +
        // store, exactly like a fresh process start would.
        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 1),
        );
        let outcome2 = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(
            outcome2.segments_written, 1,
            "only the new line becomes a new segment"
        );

        // Every event landed exactly once, across the two segments.
        let seg0 = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        let seg1 = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            1,
        );
        let mut all: Vec<String> = read_segment_envelopes(&store, &seg0)
            .await
            .into_iter()
            .chain(read_segment_envelopes(&store, &seg1).await)
            .map(|e| e.content.unwrap())
            .collect();
        all.sort();
        assert_eq!(all, vec!["line 0".to_string(), "line 1".to_string()]);
    }

    #[tokio::test]
    async fn a_batch_retried_after_crash_before_the_watermark_saved_does_not_duplicate() {
        // The other half of "no duplicates": simulate a crash *between* the store
        // PUT succeeding and the watermark being persisted, by re-running the
        // upload against a watermark that still says "nothing confirmed" while the
        // store already has segment 0. Because `PUT` to a `seg-<n>` key this
        // session's own daemon exclusively owns is idempotent for identical bytes,
        // retrying must not create a second, divergent segment 0 or skip ahead —
        // it must reproduce the exact same object.
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-1".into(),
        };
        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 0),
        );

        let outcome1 = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(outcome1.segments_written, 1);
        let seg_path = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        let first_bytes = store.get(&seg_path).await.unwrap().bytes().await.unwrap();

        // Roll the watermark back to "nothing confirmed yet" to simulate the crash
        // window, then retry.
        let state_path = watermark::state_path(dir.path(), "claude_code", "sess-1");
        watermark::write(&state_path, &SessionUploadState::default()).unwrap();
        let outcome2 = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(
            outcome2.segments_written, 1,
            "the retried batch still counts as one segment write"
        );

        let second_bytes = store.get(&seg_path).await.unwrap().bytes().await.unwrap();
        assert_eq!(
            first_bytes, second_bytes,
            "a retried PUT to the same session-owned key must be a no-op, not a divergent write"
        );

        // No stray seg-000001 was created by the retry believing it needed a *new* segment.
        let seg1 = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            1,
        );
        assert!(
            store.get(&seg1).await.is_err(),
            "the retry must not have advanced past seg-000000"
        );
    }

    #[tokio::test]
    async fn seals_and_cleans_up_local_files_once_done_and_fully_uploaded() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-1".into(),
        };

        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 0),
        );
        mark_done(dir.path(), "claude_code", "sess-1");

        let outcome = upload_session_once(&store, &c, &key).await.unwrap();
        assert!(outcome.sealed);
        assert!(outcome.cleaned_up);

        let sealed_path = ctxlake_store::layout::session_sealed(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
        );
        assert!(
            store.get(&sealed_path).await.is_ok(),
            "_SEALED must be written"
        );

        let rt_dir = dir.path().join("claude_code");
        assert!(
            !rt_dir.join("sess-1.ndjson").exists(),
            "the fully-uploaded ndjson must be removed"
        );
        assert!(!rt_dir.join("sess-1.done").exists());
        // The watermark is deliberately KEPT (see `maybe_seal_and_cleanup`'s doc): a
        // reused session_id (Claude Code `--resume`/`--continue`) must see
        // `sealed_in_store: true` and a `next_seg` that keeps counting up, not a
        // deleted-and-reset state that would let it overwrite `seg-000000`.
        let state =
            watermark::read(&watermark::state_path(dir.path(), "claude_code", "sess-1")).unwrap();
        assert!(
            state.sealed_in_store,
            "the retained watermark must still record that this session was sealed"
        );
    }

    #[tokio::test]
    async fn does_not_seal_while_done_exists_but_a_line_is_still_unconfirmed() {
        // `.done` alone is not enough — a race where SessionEnd's hook writes
        // `.done` while an earlier parallel tool call's append is still landing
        // must not seal a session that is missing data.
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-1".into(),
        };

        let dir_path = dir.path().join("claude_code");
        std::fs::create_dir_all(&dir_path).unwrap();
        // Write a line with NO trailing newline: an in-flight append, by this
        // module's own "complete line" definition.
        std::fs::write(dir_path.join("sess-1.ndjson"), envelope_line("sess-1", 0)).unwrap();
        mark_done(dir.path(), "claude_code", "sess-1");

        let outcome = upload_session_once(&store, &c, &key).await.unwrap();
        assert!(!outcome.sealed, "must not seal past an incomplete line");
        assert!(
            dir_path.join("sess-1.ndjson").exists(),
            "must not delete unconfirmed data"
        );
    }

    #[tokio::test]
    async fn a_session_with_no_events_still_seals_from_just_the_done_marker() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-empty".into(),
        };

        let dir_path = dir.path().join("claude_code");
        std::fs::create_dir_all(&dir_path).unwrap();
        std::fs::write(dir_path.join("sess-empty.ndjson"), b"").unwrap();
        mark_done(dir.path(), "claude_code", "sess-empty");

        let outcome = upload_session_once(&store, &c, &key).await.unwrap();
        assert!(outcome.sealed, "an empty-but-done session must still seal");
    }

    #[tokio::test]
    async fn a_rotation_between_cycles_neither_duplicates_nor_strands_events() {
        // Regression: the watermark used to key `state.files` by file *name*.
        // `ctxlake-hook`'s rotation renames the fully-uploaded canonical file aside
        // and starts a brand-new one at the same name — so a name-keyed watermark
        // attached the OLD confirmed offset to the NEW (small) file at that name
        // (stranding its real new bytes forever, since `from_offset >= len`) while
        // treating the just-renamed file, under its new name, as never-uploaded
        // (re-uploading its already-confirmed lines as a duplicate segment).
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-1".into(),
        };
        let rt_dir = dir.path().join("claude_code");
        let canonical = rt_dir.join("sess-1.ndjson");

        // Cycle 1: two lines land in the canonical file and get confirmed as seg-0.
        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 0),
        );
        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 1),
        );
        let outcome1 = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(outcome1.segments_written, 1);

        // Simulate `ctxlake-hook`'s own rotation exactly (`spool.rs::rotate_if_full`):
        // rename the canonical file aside, then a fresh append recreates it. `rename`
        // preserves the inode; the fresh file is a brand-new one.
        std::fs::rename(&canonical, rt_dir.join("sess-1.ndjson.1700000000")).unwrap();
        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 2),
        );

        // Cycle 2: must upload exactly the one truly-new line, not re-upload lines
        // 0/1 under the rotated name, and not strand line 2 under the fresh name.
        let outcome2 = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(
            outcome2.segments_written, 1,
            "exactly one new segment for the one truly new line"
        );

        let seg0 = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        let seg1 = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            1,
        );
        let mut all: Vec<String> = read_segment_envelopes(&store, &seg0)
            .await
            .into_iter()
            .chain(read_segment_envelopes(&store, &seg1).await)
            .map(|e| e.content.unwrap())
            .collect();
        all.sort();
        assert_eq!(
            all,
            vec![
                "line 0".to_string(),
                "line 1".to_string(),
                "line 2".to_string()
            ],
            "every line must appear exactly once across segments — no duplicate, no loss"
        );
        // Confirm no third segment (which a re-upload of the rotated file's already-
        // confirmed lines as a *fresh* stream would have produced instead of a clean
        // 1-line seg-1).
        let seg2 = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            2,
        );
        assert!(store.get(&seg2).await.is_err(), "no third segment expected");
    }

    #[tokio::test]
    async fn a_resumed_session_after_sealing_appends_new_segments_rather_than_overwriting_seg0() {
        // Regression: cleanup used to delete the watermark file once a session
        // sealed. Claude Code's `--resume`/`--continue` reuses `session_id`
        // (docs/runtimes/claude-code.md), so a session's `.ndjson` can reappear at
        // the same spool path later. With the watermark gone, that reappearance
        // read back as `next_seg: 0, sealed_in_store: false` and its first upload
        // PUT straight over the already-sealed `seg-000000`, destroying it.
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-1".into(),
        };

        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 0),
        );
        mark_done(dir.path(), "claude_code", "sess-1");
        let outcome1 = upload_session_once(&store, &c, &key).await.unwrap();
        assert!(outcome1.sealed);
        assert!(outcome1.cleaned_up);

        let seg0 = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        let before = read_segment_envelopes(&store, &seg0).await;
        assert_eq!(before[0].content.as_deref(), Some("line 0"));

        // The session resumes: the same session_id gets a fresh `.ndjson` with new
        // content, exactly as `--resume` would produce.
        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 9),
        );
        let outcome2 = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(
            outcome2.segments_written, 1,
            "the resumed line must upload as a new segment"
        );

        // seg-000000 must be untouched — the original event must still be there.
        let after = read_segment_envelopes(&store, &seg0).await;
        assert_eq!(
            after[0].content.as_deref(),
            Some("line 0"),
            "a resumed session_id must never overwrite an already-sealed segment"
        );

        let seg1 = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            1,
        );
        let seg1_envelopes = read_segment_envelopes(&store, &seg1).await;
        assert_eq!(seg1_envelopes[0].content.as_deref(), Some("line 9"));
    }

    #[tokio::test]
    async fn cleanup_gives_back_the_bytes_it_freed_to_the_hooks_size_sidecar() {
        // Regression: cleanup deleted a session's spool files but never touched
        // `ctxlake-hook`'s `.spool_size` counter (`spool.rs`'s
        // `read_or_seed_tracked_size`), so the counter only ever grew. Once enough
        // uploaded-and-reclaimed sessions pushed it past `MAX_RUNTIME_DIR_BYTES`,
        // the hook would silently drop every future event against an empty spool
        // directory.
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-1".into(),
        };
        let rt_dir = dir.path().join("claude_code");

        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 0),
        );
        mark_done(dir.path(), "claude_code", "sess-1");
        let on_disk_before = std::fs::metadata(rt_dir.join("sess-1.ndjson"))
            .unwrap()
            .len();

        // Seed the sidecar the way `ctxlake-hook` would have left it after writing
        // exactly this session's one line (plus a little headroom to prove the
        // decrement is a *subtraction*, not a reset to zero).
        std::fs::write(
            rt_dir.join(".spool_size"),
            (on_disk_before + 1000).to_string(),
        )
        .unwrap();

        let outcome = upload_session_once(&store, &c, &key).await.unwrap();
        assert!(outcome.cleaned_up);

        let sidecar_after: u64 = std::fs::read_to_string(rt_dir.join(".spool_size"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            sidecar_after, 1000,
            "cleanup must give back exactly the bytes it freed, not leave the \
             counter at its pre-cleanup value"
        );
    }

    #[tokio::test]
    async fn a_malformed_line_is_dropped_but_does_not_block_the_rest_of_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let key = SessionKey {
            runtime: "claude_code".into(),
            session_id: "sess-1".into(),
        };

        append_line(dir.path(), "claude_code", "sess-1", "{not valid json");
        append_line(
            dir.path(),
            "claude_code",
            "sess-1",
            &envelope_line("sess-1", 1),
        );

        let outcome = upload_session_once(&store, &c, &key).await.unwrap();
        assert_eq!(outcome.segments_written, 1);
        let seg_path = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        let envelopes = read_segment_envelopes(&store, &seg_path).await;
        assert_eq!(
            envelopes.len(),
            1,
            "only the one valid line should have been written"
        );
        assert_eq!(envelopes[0].content.as_deref(), Some("line 1"));
    }
}
