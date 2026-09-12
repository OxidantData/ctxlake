//! Write-then-rename: the one local-file write pattern this daemon uses for
//! anything a hook or another thread might read concurrently.
//!
//! `ctxlake-hook` reads the local cache with no coordination at all (AGENTS.md
//! invariant 1 keeps it off the network, but it still runs concurrently with this
//! daemon on the same host) — a plain `File::create` + `write_all` leaves a window
//! where a reader sees a truncated-then-partially-written file, and a torn read is
//! worse than a stale one (the task brief's own framing). `rename` on the same
//! filesystem is atomic on every OS this project ships to (POSIX `rename(2)`;
//! Windows `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`, which the standard
//! library's `fs::rename` already uses) — a reader that opens the destination path
//! either gets the old complete file or the new complete file, never a mix.
//!
//! The temp file is written as a sibling of the destination, in the same directory,
//! specifically so the rename is same-filesystem — a rename across filesystems is
//! not atomic (it degrades to copy+delete on most platforms), which a temp file
//! under `/tmp` while the destination lives under `~/.ctxlake/cache` would silently
//! violate.

use std::fs;
use std::io;
use std::path::Path;

/// Write `bytes` to `path` atomically: a temp file next to `path`, then a rename
/// over it. Creates parent directories as needed.
///
/// The temp name includes the calling process's id and a monotonic counter (not
/// just a random suffix) so two *threads* in the same process racing to refresh the
/// same cache file never collide on the same temp path — a collision there would
/// have the second writer's `File::create` truncate the first's still-being-written
/// temp file out from under it before either has renamed anything.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_atomic: path has no parent",
        )
    })?;
    fs::create_dir_all(dir)?;

    let file_name = path
        .file_name()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "write_atomic: path has no file name",
            )
        })?
        .to_string_lossy();
    let tmp_path = dir.join(format!(".{file_name}.{}.tmp", next_tmp_id()));

    // A prior crash mid-write can leave a stale temp file at this exact name once
    // the pid/counter wrap all the way around; `create(true).truncate(true)` (the
    // default for `File::create`) makes that harmless rather than an error.
    fs::write(&tmp_path, bytes).inspect_err(|_| {
        let _ = fs::remove_file(&tmp_path);
    })?;
    fs::rename(&tmp_path, path)
}

/// A per-process monotonic counter, mixed with the pid, so concurrent writers in
/// this process (and across processes, via the pid) don't pick the same temp name.
/// Not a security property — just enough entropy that two racing writers land on
/// two different temp files instead of one clobbering the other's in-flight write.
fn next_tmp_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn writes_and_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("cache.json");
        write_atomic(&path, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn a_second_write_replaces_the_first_and_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files must not survive a successful write: {leftovers:?}"
        );
    }

    #[test]
    fn a_concurrent_reader_never_observes_a_partial_file() {
        // The property this module exists for: a reader looping on `fs::read` while
        // a writer repeatedly replaces the file with much larger content must only
        // ever see one of the two complete byte strings, never a prefix of one
        // glued to a suffix of the other (what a truncate-then-write-in-place would
        // eventually produce under enough concurrent iterations).
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("cache.json"));

        let small = vec![b'A'; 16];
        let large = vec![b'B'; 4096];
        write_atomic(&path, &small).unwrap();

        let writer_path = Arc::clone(&path);
        let large_clone = large.clone();
        let small_clone = small.clone();
        let writer = thread::spawn(move || {
            for i in 0..500 {
                let bytes = if i % 2 == 0 {
                    &large_clone
                } else {
                    &small_clone
                };
                write_atomic(&writer_path, bytes).unwrap();
            }
        });

        let reader_path = Arc::clone(&path);
        let reader = thread::spawn(move || {
            for _ in 0..2000 {
                if let Ok(bytes) = fs::read(reader_path.as_path()) {
                    let is_all_a = bytes.iter().all(|&b| b == b'A') && bytes.len() == 16;
                    let is_all_b = bytes.iter().all(|&b| b == b'B') && bytes.len() == 4096;
                    assert!(
                        is_all_a || is_all_b,
                        "reader observed a torn write: {} bytes, not matching either complete value",
                        bytes.len()
                    );
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }

    #[test]
    fn a_stale_leftover_temp_file_at_the_same_name_does_not_break_the_next_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        // Simulate a crash that left a temp file behind at the id this process will
        // reuse (the pid/counter can only repeat after enough activity, but the
        // write path must tolerate it regardless of how it happened).
        let stale_tmp = dir
            .path()
            .join(format!(".cache.json.{}-0.tmp", std::process::id()));
        fs::write(&stale_tmp, b"leftover garbage").unwrap();

        write_atomic(&path, b"fresh").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"fresh");
    }
}
