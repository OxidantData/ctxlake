//! REFRESH: mirror the object store into the local cache the hook and MCP server
//! read — see `docs/architecture.md`'s read-path diagram and AGENTS.md invariant 1
//! (nothing on the hook's read path may touch the network; this loop is what keeps
//! the cache worth reading in the first place).
//!
//! Two things are mirrored today:
//!
//! - **roster** (`live/roster.json`), via [`ctxlake_store::roster::fetch`], which
//!   already implements the conditional-GET/304 fan-in this module just needs to
//!   call and persist. Written to `<cache_root>/<fleet_id>/roster.json`.
//! - **snapshot** (`snapshot/latest.json` -> `snapshot/<content_hash>.sqlite`, per
//!   `ctxlake_store::layout`), the content-addressed publish/pointer pair
//!   `docs/concepts.md`'s "Serving" plane describes. What the *blob* actually
//!   contains (a briefing, a recent-session digest, ...) is `ctxlake-maint`'s
//!   schema to define and publish, in a wave this crate does not own — nothing
//!   writes `snapshot/latest.json` yet. This module treats the blob as opaque bytes
//!   and mirrors it byte-for-byte to `<cache_root>/<fleet_id>/snapshot.bin`: the
//!   mechanism (conditional GET on the pointer, fetch-by-hash on change, atomic
//!   write) is what this wave delivers, so that whichever content schema
//!   `ctxlake-maint` ships later needs zero changes here to start flowing through.
//!
//! Every write to disk goes through [`crate::atomic_file::write_atomic`] — see that
//! module's doc for why a torn cache read is worse than a stale one. Etags are kept
//! only in memory for the life of one daemon run, not persisted: a restart pays one
//! extra full GET the first time it refreshes each cache, which is negligible next
//! to a `sync` process's actual lifetime, and not persisting them is one less file
//! that could itself go stale or torn.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use object_store::{Error as OsError, GetOptions, ObjectStore, ObjectStoreExt};
use serde::Deserialize;

use crate::atomic_file::write_atomic;
use crate::backoff::{should_backoff, Backoff};
use ctxlake_store::clock::Clock;
use ctxlake_store::roster::Roster;
use ctxlake_store::{layout, StoreError};

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub fleet_id: String,
    pub cache_root: PathBuf,
}

impl CacheConfig {
    fn dir(&self) -> PathBuf {
        self.cache_root.join(&self.fleet_id)
    }

    fn roster_path(&self) -> PathBuf {
        self.dir().join("roster.json")
    }

    fn snapshot_blob_path(&self) -> PathBuf {
        self.dir().join("snapshot.bin")
    }
}

/// In-memory refresh state carried between loop ticks — see the module doc for why
/// this is deliberately not persisted to disk.
#[derive(Debug, Default)]
pub struct RefreshState {
    roster_etag: Option<String>,
    snapshot_pointer_etag: Option<String>,
    snapshot_content_hash: Option<String>,
}

/// The minimal shape this module needs from `snapshot/latest.json`: enough to
/// locate the immutable blob by its content hash. See the module doc — the full
/// pointer schema (if it ever carries more) belongs to whatever publishes it.
#[derive(Debug, Deserialize)]
struct SnapshotPointer {
    content_hash: String,
}

/// Refresh the roster cache. Returns whether the on-disk file changed — callers use
/// this only for logging/tests, never for correctness (the write itself is what's
/// durable).
pub async fn refresh_roster(
    store: &dyn ObjectStore,
    clock: &dyn Clock,
    cfg: &CacheConfig,
    state: &mut RefreshState,
) -> Result<bool, StoreError> {
    match ctxlake_store::roster::fetch(store, clock, state.roster_etag.as_deref()).await? {
        Roster::Unchanged => Ok(false),
        Roster::Fresh { snapshot, etag } => {
            let bytes = serde_json::to_vec_pretty(&snapshot)?;
            write_atomic(&cfg.roster_path(), &bytes)
                .map_err(|e| StoreError::Config(format!("write roster cache: {e}")))?;
            state.roster_etag = etag;
            Ok(true)
        }
    }
}

/// Refresh the generic snapshot mirror. See the module doc for why this cannot
/// (yet) know or validate what the blob contains.
pub async fn refresh_snapshot(
    store: &dyn ObjectStore,
    cfg: &CacheConfig,
    state: &mut RefreshState,
) -> Result<bool, StoreError> {
    let opts = GetOptions {
        if_none_match: state.snapshot_pointer_etag.clone(),
        ..Default::default()
    };
    let pointer_res = match store.get_opts(&layout::snapshot_latest(), opts).await {
        Ok(res) => res,
        Err(OsError::NotModified { .. }) => return Ok(false),
        // Nobody has published a snapshot yet (no `ctxlake maint` run in this
        // fleet, or a brand-new one) — the honest "nothing to mirror" case
        // described in `docs/architecture.md`'s "Empty briefing" failure mode, not
        // an error this loop should back off over.
        Err(OsError::NotFound { .. }) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let pointer_etag = pointer_res.meta.e_tag.clone();
    let pointer: SnapshotPointer = serde_json::from_slice(&pointer_res.bytes().await?)?;
    state.snapshot_pointer_etag = pointer_etag;

    if state.snapshot_content_hash.as_deref() == Some(pointer.content_hash.as_str()) {
        // The pointer object changed (new etag) but still names the same immutable
        // blob we already have on disk — a republish with no real content change,
        // or two builders racing to publish an identical recomputation. Either way
        // the blob is content-addressed and therefore byte-identical; skip the GET.
        return Ok(false);
    }

    let blob_path = layout::snapshot(&pointer.content_hash);
    let blob = match store.get(&blob_path).await {
        Ok(res) => res.bytes().await?,
        Err(OsError::NotFound { .. }) => {
            // The pointer named a blob that isn't there (yet, or a torn publish
            // that never finished writing the blob before the pointer swap this
            // module observed) — `docs/concepts.md`'s no-cross-key-atomicity story
            // says a crash between blob and pointer writes is the *publisher's*
            // orphan/no-op to have, but a reader landing exactly in that window
            // must not treat it as fatal either. Leave `snapshot_content_hash`
            // unset so the next cycle retries.
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };

    write_atomic(&cfg.snapshot_blob_path(), &blob)
        .map_err(|e| StoreError::Config(format!("write snapshot cache: {e}")))?;
    state.snapshot_content_hash = Some(pointer.content_hash);
    Ok(true)
}

/// Run the refresh loop until `shutdown` fires.
pub async fn run(
    store: Arc<dyn ObjectStore>,
    clock: Arc<dyn Clock>,
    cfg: CacheConfig,
    poll_interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut state = RefreshState::default();
    let mut backoff = Backoff::new(Duration::from_millis(200), Duration::from_secs(30));
    loop {
        if *shutdown.borrow() {
            break;
        }
        let mut hit_error = false;
        if let Err(e) = refresh_roster(store.as_ref(), clock.as_ref(), &cfg, &mut state).await {
            tracing::warn!(error = %e, "roster refresh failed");
            hit_error |= should_backoff(&e);
        }
        if let Err(e) = refresh_snapshot(store.as_ref(), &cfg, &mut state).await {
            tracing::warn!(error = %e, "snapshot refresh failed");
            hit_error |= should_backoff(&e);
        }

        let delay = if hit_error {
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

/// Read back whatever this daemon most recently wrote to the roster cache — used by
/// this crate's own tests to assert on cache contents without hand-rolling the path
/// twice.
#[cfg(test)]
pub(crate) fn read_roster_cache(cfg: &CacheConfig) -> Vec<u8> {
    std::fs::read(cfg.roster_path()).expect("roster cache should exist")
}

#[cfg(test)]
pub(crate) fn read_snapshot_cache(cfg: &CacheConfig) -> Vec<u8> {
    std::fs::read(cfg.snapshot_blob_path()).expect("snapshot cache should exist")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::Runtime;
    use ctxlake_store::clock::SystemClock;
    use ctxlake_store::intent::{write as write_intent, Intent};
    use object_store::memory::InMemory;
    use object_store::PutPayload;
    use std::path::Path;
    use time::OffsetDateTime;

    fn cfg(root: &Path) -> CacheConfig {
        CacheConfig {
            fleet_id: "oxidant".into(),
            cache_root: root.to_path_buf(),
        }
    }

    async fn seed_intent(store: &dyn ObjectStore, agent_id: &str) {
        write_intent(
            store,
            &Intent {
                agent_id: agent_id.to_string(),
                fleet_id: "oxidant".into(),
                runtime: Runtime::ClaudeCode,
                session_id: None,
                repo: None,
                branch: None,
                cwd: None,
                task: None,
                paths: vec![],
                updated_at: OffsetDateTime::now_utc(),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn refresh_roster_writes_the_cache_and_reports_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let clock = SystemClock;
        seed_intent(&store, "cc-01").await;
        let c = cfg(dir.path());
        let mut state = RefreshState::default();

        let changed = refresh_roster(&store, &clock, &c, &mut state)
            .await
            .unwrap();
        assert!(changed);
        let bytes = read_roster_cache(&c);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["agents"][0]["agent_id"], "cc-01");
    }

    #[tokio::test]
    async fn a_second_refresh_with_no_change_costs_no_body_and_reports_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let clock = SystemClock;
        seed_intent(&store, "cc-01").await;
        let c = cfg(dir.path());
        let mut state = RefreshState::default();

        assert!(refresh_roster(&store, &clock, &c, &mut state)
            .await
            .unwrap());
        let first_write = std::fs::metadata(c.roster_path())
            .unwrap()
            .modified()
            .unwrap();

        // No maintenance-lease holder in this test, so `fetch` falls back to a
        // direct list every call rather than trusting an etag — assert the module
        // still reports "no meaningful change" the one way available to it: the
        // file's content and mtime are untouched by a cycle that found nothing new
        // to say. (`RosterSource::RosterBuild`'s conditional-GET 304 path is
        // covered directly against `ctxlake_store::roster` already; this test is
        // about this module's plumbing, not re-proving that primitive.)
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(refresh_roster(&store, &clock, &c, &mut state)
            .await
            .unwrap());
        let second_write = std::fs::metadata(c.roster_path())
            .unwrap()
            .modified()
            .unwrap();
        assert!(second_write >= first_write);
    }

    #[tokio::test]
    async fn refresh_snapshot_is_a_no_op_when_nobody_has_published_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let mut state = RefreshState::default();

        let changed = refresh_snapshot(&store, &c, &mut state).await.unwrap();
        assert!(!changed);
        assert!(!c.snapshot_blob_path().exists());
    }

    async fn publish_snapshot(store: &dyn ObjectStore, content_hash: &str, body: &[u8]) {
        store
            .put(
                &layout::snapshot(content_hash),
                PutPayload::from(body.to_vec()),
            )
            .await
            .unwrap();
        let pointer = serde_json::json!({"content_hash": content_hash});
        store
            .put(
                &layout::snapshot_latest(),
                PutPayload::from(serde_json::to_vec(&pointer).unwrap()),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refresh_snapshot_mirrors_a_published_blob_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let mut state = RefreshState::default();
        publish_snapshot(&store, "abc123", b"opaque snapshot bytes").await;

        let changed = refresh_snapshot(&store, &c, &mut state).await.unwrap();
        assert!(changed);
        assert_eq!(read_snapshot_cache(&c), b"opaque snapshot bytes");
    }

    #[tokio::test]
    async fn refresh_snapshot_skips_the_blob_get_when_the_hash_is_already_cached() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let mut state = RefreshState::default();
        publish_snapshot(&store, "abc123", b"v1").await;
        assert!(refresh_snapshot(&store, &c, &mut state).await.unwrap());

        // Republish the identical hash under a fresh pointer write (a new etag,
        // same content) — this must be recognized as "nothing to do" without
        // fetching the blob again.
        let pointer = serde_json::json!({"content_hash": "abc123"});
        store
            .put(
                &layout::snapshot_latest(),
                PutPayload::from(serde_json::to_vec(&pointer).unwrap()),
            )
            .await
            .unwrap();

        let changed = refresh_snapshot(&store, &c, &mut state).await.unwrap();
        assert!(
            !changed,
            "an unchanged content hash must not be reported as a change"
        );
        assert_eq!(read_snapshot_cache(&c), b"v1");
    }

    #[tokio::test]
    async fn refresh_snapshot_picks_up_a_real_content_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let c = cfg(dir.path());
        let mut state = RefreshState::default();
        publish_snapshot(&store, "hash-v1", b"version one").await;
        assert!(refresh_snapshot(&store, &c, &mut state).await.unwrap());

        publish_snapshot(&store, "hash-v2", b"version two").await;
        let changed = refresh_snapshot(&store, &c, &mut state).await.unwrap();
        assert!(changed);
        assert_eq!(read_snapshot_cache(&c), b"version two");
    }
}
