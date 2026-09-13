//! Removing objects nothing reads any more.
//!
//! Everything else in this crate only ever *adds* to the lake, which is what makes
//! concurrent maintenance safe. Deleting is different in kind, so it is opt-in
//! (`ctxlake maint --prune`), narrow, and refuses to touch anything a reader could
//! still be using.
//!
//! **What this does not prune, and why.** Sessions, claim events, digests and
//! compaction generations are all either irreplaceable history or content-addressed
//! inputs that a lagging reader may still resolve. `layout::snapshot`'s own comment
//! records the same reasoning for compaction generations. Only two categories are
//! safe:
//!
//! - **Superseded snapshot blobs.** `snapshot/<hash>.sqlite` is republished whenever
//!   the lake changes, and nothing ever removed the previous one — a fleet running
//!   maintenance every five minutes accumulates a 40KB object per change forever.
//!   The blob `latest.json` points at is never a candidate, and neither is one
//!   younger than [`SNAPSHOT_GRACE`], because a reader that fetched the pointer a
//!   moment ago is about to fetch the blob it named.
//! - **The pre-fleet-scoping `live/` keys.** `live/agents/*.json` and
//!   `live/roster.json` are written by no version from here on; a build that still
//!   wrote them could not have been reading a fleet-scoped roster either.

use futures::StreamExt;
use object_store::{Error as OsError, ObjectStore, ObjectStoreExt};
use std::time::Duration;

use ctxlake_store::layout;

/// How long a superseded snapshot blob is kept before it can be pruned.
///
/// The hazard is small but real: a reader resolves `latest.json`, then fetches the
/// blob it named, and a prune between those two requests turns into a 404. A day is
/// many orders of magnitude more than that gap, and still bounds growth to something
/// a bucket does not notice.
pub const SNAPSHOT_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Superseded snapshot blobs removed (or that would be, on a dry run).
    pub snapshots: Vec<String>,
    /// Pre-fleet-scoping `live/` objects removed.
    pub legacy_live: Vec<String>,
    /// Blobs old enough to consider but held back by [`SNAPSHOT_GRACE`].
    pub snapshots_within_grace: usize,
}

impl PruneReport {
    pub fn total(&self) -> usize {
        self.snapshots.len() + self.legacy_live.len()
    }
}

/// Prune what is safe to prune. `dry_run` reports without deleting.
pub async fn run(
    store: &dyn ObjectStore,
    fleet_id: &str,
    dry_run: bool,
) -> Result<PruneReport, ctxlake_store::StoreError> {
    let mut report = PruneReport::default();

    // The blob the pointer names is off limits regardless of age. Resolved first and
    // treated as fatal if unreadable: pruning snapshots without knowing which one is
    // current could delete the live one, and "the pointer is unreadable" is not a
    // state in which to start guessing.
    let current = current_snapshot_hash(store, fleet_id).await?;
    // Unix seconds on both sides rather than naming `chrono`'s types. `object_store`
    // exposes `last_modified` as a `chrono::DateTime`, but this workspace uses `time`
    // everywhere else and adding a second date library as a declared dependency to
    // subtract two instants is not a trade worth making.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    let mut stream = store.list(Some(&object_store::path::Path::from("snapshot")));
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        let key = meta.location.as_ref().to_string();
        let Some(hash) = key
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".sqlite"))
        else {
            continue; // latest.json, or anything else that is not a blob
        };
        if Some(hash) == current.as_deref() {
            continue;
        }
        // `last_modified` is the store's clock, `now` is ours. A blob that looks
        // newer than now (skew, or a store that stamps ahead) reads as age zero and
        // is therefore kept — the safe direction, since keeping a prunable object
        // costs 40KB and deleting a live one costs a reader a 404.
        let age = Duration::from_secs((now - meta.last_modified.timestamp()).max(0) as u64);
        if age < SNAPSHOT_GRACE {
            report.snapshots_within_grace += 1;
            continue;
        }
        if !dry_run {
            store.delete(&meta.location).await?;
        }
        report.snapshots.push(key);
    }

    for prefix in [layout::legacy_agents_prefix()] {
        let mut stream = store.list(Some(&prefix));
        while let Some(meta) = stream.next().await {
            let Ok(meta) = meta else { continue };
            if !dry_run {
                store.delete(&meta.location).await?;
            }
            report.legacy_live.push(meta.location.as_ref().to_string());
        }
    }
    let legacy_roster = layout::legacy_roster();
    match store.head(&legacy_roster).await {
        Ok(_) => {
            if !dry_run {
                store.delete(&legacy_roster).await?;
            }
            report.legacy_live.push(legacy_roster.as_ref().to_string());
        }
        Err(OsError::NotFound { .. }) => {}
        Err(e) => return Err(e.into()),
    }

    report.snapshots.sort();
    report.legacy_live.sort();
    Ok(report)
}

/// The content hash `snapshot/latest.json` currently points at.
async fn current_snapshot_hash(
    store: &dyn ObjectStore,
    fleet_id: &str,
) -> Result<Option<String>, ctxlake_store::StoreError> {
    match store.get(&layout::snapshot_latest(fleet_id)).await {
        Ok(res) => {
            let bytes = res.bytes().await?;
            let v: serde_json::Value = serde_json::from_slice(&bytes)?;
            Ok(v.get("content_hash")
                .and_then(|h| h.as_str())
                .map(str::to_string))
        }
        // No pointer yet means no published snapshot, so every blob present is an
        // orphan from an interrupted publish — still subject to the grace window.
        Err(OsError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::{memory::InMemory, PutPayload};

    async fn put(store: &InMemory, key: &str, body: &str) {
        store
            .put(
                &object_store::path::Path::from(key),
                PutPayload::from(body.as_bytes().to_vec()),
            )
            .await
            .unwrap();
    }

    async fn keys(store: &InMemory) -> Vec<String> {
        let mut out: Vec<String> = store
            .list(None)
            .filter_map(|m| async move { m.ok().map(|m| m.location.as_ref().to_string()) })
            .collect()
            .await;
        out.sort();
        out
    }

    #[tokio::test]
    async fn the_published_snapshot_is_never_a_candidate() {
        // The one object in `snapshot/` that a reader is guaranteed to want.
        let store = InMemory::new();
        put(&store, "snapshot/aaa.sqlite", "current").await;
        put(
            &store,
            "snapshot/fleets/myteam/latest.json",
            r#"{"content_hash":"aaa"}"#,
        )
        .await;

        let report = run(&store, "myteam", false).await.unwrap();
        assert!(report.snapshots.is_empty(), "{report:?}");
        assert!(keys(&store).await.contains(&"snapshot/aaa.sqlite".into()));
    }

    #[tokio::test]
    async fn a_superseded_blob_inside_the_grace_window_is_kept() {
        // A reader resolves latest.json and then fetches the blob it named. Pruning
        // between those two requests turns a normal read into a 404, so recency alone
        // protects a blob even once it is superseded.
        let store = InMemory::new();
        put(&store, "snapshot/old.sqlite", "superseded").await;
        put(&store, "snapshot/new.sqlite", "current").await;
        put(
            &store,
            "snapshot/fleets/myteam/latest.json",
            r#"{"content_hash":"new"}"#,
        )
        .await;

        let report = run(&store, "myteam", false).await.unwrap();
        assert!(report.snapshots.is_empty(), "{report:?}");
        assert_eq!(report.snapshots_within_grace, 1);
        assert!(keys(&store).await.contains(&"snapshot/old.sqlite".into()));
    }

    #[tokio::test]
    async fn the_legacy_live_keys_are_removed_and_the_scoped_ones_are_not() {
        // `live/agents/<id>.json` and `live/roster.json` are the pre-fleet-scoping
        // layout. Nothing writes them from here on, and a build old enough to write
        // them was not reading a fleet-scoped roster either.
        let store = InMemory::new();
        put(&store, "live/agents/cc-01.json", "{}").await;
        put(&store, "live/agents/mac-01.json", "{}").await;
        put(&store, "live/roster.json", "{}").await;
        put(&store, "live/fleets/myteam/agents/cc-01.json", "{}").await;
        put(&store, "live/fleets/myteam/roster.json", "{}").await;

        let report = run(&store, "myteam", false).await.unwrap();
        assert_eq!(
            report.legacy_live,
            vec![
                "live/agents/cc-01.json".to_string(),
                "live/agents/mac-01.json".to_string(),
                "live/roster.json".to_string(),
            ]
        );
        let left = keys(&store).await;
        assert_eq!(
            left,
            vec![
                "live/fleets/myteam/agents/cc-01.json".to_string(),
                "live/fleets/myteam/roster.json".to_string(),
            ],
            "the current layout must be untouched"
        );
    }

    #[tokio::test]
    async fn a_dry_run_reports_exactly_what_it_would_delete_and_deletes_nothing() {
        let store = InMemory::new();
        put(&store, "live/roster.json", "{}").await;
        put(&store, "live/agents/cc-01.json", "{}").await;

        let before = keys(&store).await;
        let dry = run(&store, "myteam", true).await.unwrap();
        assert_eq!(dry.legacy_live.len(), 2);
        assert_eq!(keys(&store).await, before, "a dry run must delete nothing");

        let wet = run(&store, "myteam", false).await.unwrap();
        assert_eq!(
            wet.legacy_live, dry.legacy_live,
            "the dry run must predict the real one exactly"
        );
        assert!(keys(&store).await.is_empty());
    }

    #[tokio::test]
    async fn sessions_and_claims_are_never_touched() {
        // The irreplaceable half of the lake. `prune` is opt-in and narrow precisely
        // so that a mistake here is impossible rather than unlikely.
        let store = InMemory::new();
        let protected = [
            "sessions/dt=2026-09-12/fleet=f/runtime=claude_code/agent=a/session=s/seg-000000.parquet",
            "sessions/dt=2026-09-12/fleet=f/runtime=claude_code/agent=a/session=s/_SEALED",
            "claims/events/dt=2026-09-12/agent=a/01J.json",
            "claims/extracted/session-1",
            "_meta/fleet.json",
        ];
        for k in protected {
            put(&store, k, "{}").await;
        }
        let report = run(&store, "myteam", false).await.unwrap();
        assert_eq!(report.total(), 0, "{report:?}");
        assert_eq!(keys(&store).await.len(), protected.len());
    }
}
