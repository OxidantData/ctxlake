//! Compaction — rewrite a `(date, fleet)` partition's many small sealed-session
//! Parquet segments into a handful of larger files. See `docs/architecture.md`'s
//! maintenance chain and `docs/scaling.md`'s "small-object explosion" section for
//! why this exists: one Parquet file per session is cheap to write but expensive to
//! `LIST` and read back at scale.
//!
//! Pure Arrow/Parquet, no query engine — this reads segments through
//! `ctxlake_sync::codec` (the same encoder/decoder the daemon uses to write them, so
//! there is exactly one Envelope<->Parquet schema in this codebase, not two that can
//! drift), pools the decoded envelopes, and re-encodes them through the same codec.
//!
//! **Bronze is never touched.** Compaction only ever *adds* files under
//! `sessions/compacted/...` (see `ctxlake_store::layout::sessions_compacted_part`);
//! it never deletes or rewrites a `seg-*.parquet` file or a `_SEALED` marker. A
//! session still being appended to (no `_SEALED` yet) is invisible to this module by
//! construction — it is never listed, so there is no race to guard against.
//!
//! **Idempotent by content, not by re-running arithmetic.** A `(date, fleet)`
//! partition's identity is the sorted set of its sealed sessions
//! ([`sessions_hash`]); a second run that finds the identical set skips straight to
//! [`CompactionOutcome::skipped`] without reading a single segment. This is not an
//! optimization bolted on afterward — the module doc for
//! `ctxlake_store::layout::sessions_compaction_marker` names this as the very
//! contract the marker exists to make checkable.
//!
//! **Deduped on content_hash, but only where content_hash means something.**
//! `Envelope::new`'s default `content_hash` is `hash::content_hash("")` — the same
//! value on *every* envelope that never set `content` (which, per
//! `ctxlake-hook`'s adapters, is every plain `ToolCall`: the call's payload lives in
//! `tool.input`/`tool.result`, not the envelope's own `content` field). Deduping on
//! that shared value would silently collapse every contentless tool call fleet-wide
//! into one row — including the very repeated `cargo test` failures
//! `ctxlake_maint::digest`'s friction detection depends on counting exactly.
//! `dedup_by_content_hash` below refuses to do that: only envelopes whose
//! `content_hash` differs from `hash::content_hash("")` (real, non-empty content —
//! the `AGENTS.md`/`CLAUDE.md` text this module doc's own dedup rationale is about)
//! are ever collapsed against each other; every "empty" row is passed through
//! individually, exactly once, regardless of how many others share that hash.
//!
//! Callers must hold `live/leases/_maintenance` before calling anything here — this
//! module does not acquire it itself, the same convention
//! `ctxlake_store::roster::build` documents for the same reason: a caller already
//! doing its own lease bookkeeping shouldn't be forced through a second, redundant
//! check. See [`crate::run`].

use std::collections::HashSet;

use ctxlake_core::{envelope::Envelope, hash};
use object_store::path::Path;
use object_store::{Error as OsError, ObjectStore, ObjectStoreExt, PutPayload};

use crate::error::MaintError;
use crate::partition::read_session_segments;

/// Target size for one compacted output file. Not a hard cap — the last part of a
/// partition is whatever's left over, however small — just the point at which a
/// growing batch is flushed as its own file. See `docs/summarization.md`'s sibling
/// docs for the "~256MB" figure this implements.
pub const DEFAULT_MAX_PART_BYTES: usize = 256 * 1024 * 1024;

/// What one [`run`] call actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionOutcome {
    pub date: String,
    pub fleet_id: String,
    pub sealed_session_count: usize,
    pub rows_in: usize,
    pub rows_out: usize,
    pub parts_written: usize,
    /// True when this run found the partition already compacted (its sealed-session
    /// set matched the existing marker) and did no reading or writing at all.
    pub skipped: bool,
}

/// List every `_SEALED` marker under one `(date, fleet)` partition, sorted — sort
/// order matters because it is exactly what [`sessions_hash`] hashes, and two runs
/// that list the same set in different orders must still agree it's the same set.
async fn sealed_session_markers(
    store: &dyn ObjectStore,
    date: &str,
    fleet_id: &str,
) -> Result<Vec<Path>, MaintError> {
    use futures::StreamExt;
    let prefix = Path::from("sessions")
        .join(format!("dt={date}"))
        .join(format!("fleet={fleet_id}"));
    let mut stream = store.list(Some(&prefix));
    let mut markers = Vec::new();
    while let Some(meta) = stream.next().await {
        // Best-effort, matching `ctxlake_store::roster::list_intents_directly`'s own
        // tolerance: one bad entry in a LIST stream must not abort discovery for
        // every session that *does* list cleanly.
        let Ok(meta) = meta else { continue };
        if meta.location.as_ref().ends_with("/_SEALED") {
            markers.push(meta.location);
        }
    }
    markers.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
    Ok(markers)
}

/// A content hash of the sorted sealed-session set — the partition's identity for
/// idempotency purposes. Two runs over the identical set of sealed sessions always
/// compute the same value, regardless of what order the backend's `LIST` happened to
/// return them in (the sort in [`sealed_session_markers`] guarantees that).
fn sessions_hash(markers: &[Path]) -> String {
    let joined = markers
        .iter()
        .map(|p| p.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    hash::content_hash(joined)
}

/// Collapse envelopes that share a *meaningful* `content_hash` into one
/// representative row each, in a deterministic order. See the module doc for why
/// `hash::content_hash("")` — the shared value of every envelope that never set
/// `content` — is explicitly exempted from this collapse rather than treated as "one
/// more hash to dedup on."
fn dedup_by_content_hash(mut envelopes: Vec<Envelope>) -> Vec<Envelope> {
    // Deterministic output order given an identical input set: by (session_id,
    // event_id). event_id alone is only monotonic within one session's single
    // writer (envelope.rs), so session_id must lead the sort for a stable,
    // reproducible byte-for-byte result across runs and across which order the
    // backend happened to list sessions in.
    envelopes.sort_by(|a, b| (&a.session_id, &a.event_id).cmp(&(&b.session_id, &b.event_id)));

    let empty_hash = hash::content_hash("");
    let mut seen: HashSet<String> = HashSet::new();
    envelopes
        .into_iter()
        .filter(|e| {
            if e.content_hash == empty_hash {
                return true; // never dedup contentless rows against each other.
            }
            seen.insert(e.content_hash.clone())
        })
        .collect()
}

/// Split `envelopes` into Parquet-encoded parts, each targeting roughly
/// `max_part_bytes`. `envelopes` must already be in the final row order — this
/// function does not reorder anything, it only decides where to cut.
///
/// Sizing a batch by re-encoding it to Parquet on every row (or even periodically)
/// would make this quadratic in the partition's row count for no real benefit, so
/// the running total this tracks is each envelope's *serialized JSON* length — the
/// exact string that lands in the `envelope_json` column, before Snappy compresses
/// it. That is a deliberate overestimate of the real encoded size (compression only
/// ever shrinks it further), which biases cuts toward *smaller-than-target* output
/// files rather than larger — the safe direction for a "roughly 256MB" target to err
/// in. Only the finished batches actually get Parquet-encoded, once each.
fn chunk_into_parts(
    envelopes: &[Envelope],
    max_part_bytes: usize,
) -> Result<Vec<Vec<u8>>, MaintError> {
    if envelopes.is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    let mut batch_start = 0usize;
    let mut running_bytes = 0usize;
    for (i, e) in envelopes.iter().enumerate() {
        running_bytes += serde_json::to_vec(e).map(|v| v.len()).unwrap_or(0);
        let batch_len = i + 1 - batch_start;
        if running_bytes >= max_part_bytes && batch_len > 0 {
            let flush = &envelopes[batch_start..=i];
            parts.push(ctxlake_sync::codec::encode(flush).map_err(MaintError::Codec)?);
            batch_start = i + 1;
            running_bytes = 0;
        }
    }
    if batch_start < envelopes.len() {
        let flush = &envelopes[batch_start..];
        parts.push(ctxlake_sync::codec::encode(flush).map_err(MaintError::Codec)?);
    }
    Ok(parts)
}

/// Compact one `(date, fleet)` partition. See the module doc for the full contract.
pub async fn run(
    store: &dyn ObjectStore,
    date: &str,
    fleet_id: &str,
) -> Result<CompactionOutcome, MaintError> {
    run_with_part_size(store, date, fleet_id, DEFAULT_MAX_PART_BYTES).await
}

/// Same as [`run`], with an overridable target part size — the seam
/// [`chunk_into_parts`]'s tests and this module's own multi-part test use to force
/// more than one output file without needing hundreds of megabytes of fixture data.
pub async fn run_with_part_size(
    store: &dyn ObjectStore,
    date: &str,
    fleet_id: &str,
    max_part_bytes: usize,
) -> Result<CompactionOutcome, MaintError> {
    let markers = sealed_session_markers(store, date, fleet_id).await?;
    let current_hash = sessions_hash(&markers);

    let marker_path = ctxlake_store::layout::sessions_compaction_marker(date, fleet_id);
    if let Ok(res) = store.get(&marker_path).await {
        let bytes = res.bytes().await?;
        if let Ok(existing) = serde_json::from_slice::<CompactionMarker>(&bytes) {
            if existing.sessions_hash == current_hash {
                return Ok(CompactionOutcome {
                    date: date.to_string(),
                    fleet_id: fleet_id.to_string(),
                    sealed_session_count: markers.len(),
                    rows_in: existing.rows_in,
                    rows_out: existing.rows_out,
                    parts_written: existing.parts,
                    skipped: true,
                });
            }
        }
    }

    if markers.is_empty() {
        // Nothing sealed yet for this partition — write the empty marker so a
        // second no-op call this cycle also skips, but there is nothing to encode.
        write_marker(store, &marker_path, &current_hash, 0, 0, 0, 0).await?;
        return Ok(CompactionOutcome {
            date: date.to_string(),
            fleet_id: fleet_id.to_string(),
            sealed_session_count: 0,
            rows_in: 0,
            rows_out: 0,
            parts_written: 0,
            skipped: false,
        });
    }

    let mut pooled = Vec::new();
    for marker in &markers {
        pooled.extend(read_session_segments(store, marker).await?);
    }
    let rows_in = pooled.len();
    let deduped = dedup_by_content_hash(pooled);
    let rows_out = deduped.len();

    let parts = chunk_into_parts(&deduped, max_part_bytes)?;
    for (i, bytes) in parts.iter().enumerate() {
        let part_path = ctxlake_store::layout::sessions_compacted_part(date, fleet_id, i as u32);
        store
            .put(&part_path, PutPayload::from(bytes.clone()))
            .await?;
    }

    write_marker(
        store,
        &marker_path,
        &current_hash,
        markers.len(),
        rows_in,
        rows_out,
        parts.len(),
    )
    .await?;

    Ok(CompactionOutcome {
        date: date.to_string(),
        fleet_id: fleet_id.to_string(),
        sealed_session_count: markers.len(),
        rows_in,
        rows_out,
        parts_written: parts.len(),
        skipped: false,
    })
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CompactionMarker {
    sessions_hash: String,
    session_count: usize,
    rows_in: usize,
    rows_out: usize,
    parts: usize,
    built_at: String,
}

#[allow(clippy::too_many_arguments)]
async fn write_marker(
    store: &dyn ObjectStore,
    path: &Path,
    sessions_hash: &str,
    session_count: usize,
    rows_in: usize,
    rows_out: usize,
    parts: usize,
) -> Result<(), MaintError> {
    let marker = CompactionMarker {
        sessions_hash: sessions_hash.to_string(),
        session_count,
        rows_in,
        rows_out,
        parts,
        built_at: crate::now_rfc3339(),
    };
    // Plain overwrite, not CAS: this key is fully recomputed from a fresh listing
    // every call, never accumulated — see the module doc's note on
    // `sessions_compaction_marker` and `roster::build`'s identical reasoning for the
    // same shape of key.
    store
        .put(path, PutPayload::from(serde_json::to_vec(&marker)?))
        .await?;
    Ok(())
}

/// Discover every `dt=<date>` partition under `sessions/` (excluding the
/// `sessions/compacted/` prefix compaction itself writes into) — used by
/// [`crate::run`] to find every date worth attempting compaction on, rather than
/// requiring an operator to name one explicitly. Attempting a date that turns out to
/// have no sessions for `fleet_id` is harmless and cheap (see [`run`]'s
/// empty-partition path); this deliberately does not filter by fleet itself; that
/// exact question is what `run` already answers next.
pub async fn discover_dates(store: &dyn ObjectStore) -> Result<Vec<String>, MaintError> {
    let result = store
        .list_with_delimiter(Some(&Path::from("sessions")))
        .await;
    let result = match result {
        Ok(r) => r,
        Err(OsError::NotFound { .. }) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut dates: Vec<String> = result
        .common_prefixes
        .iter()
        .filter_map(|p| p.as_ref().rsplit('/').next())
        .filter_map(|seg| seg.strip_prefix("dt="))
        .map(str::to_string)
        .collect();
    dates.sort();
    dates.dedup();
    Ok(dates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::{EventType, Runtime};
    use object_store::memory::InMemory;

    fn sample_envelope(session: &str, n: u32, content: Option<&str>) -> Envelope {
        let mut e = Envelope::new(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            session,
            EventType::ToolCall,
            format!("2026-09-11T18:22:{n:02}.000Z"),
        );
        e.content = content.map(str::to_string);
        e.content_hash = hash::content_hash(content.unwrap_or(""));
        e
    }

    async fn seal_session(
        store: &dyn ObjectStore,
        date: &str,
        fleet: &str,
        session: &str,
        envelopes: &[Envelope],
    ) {
        let bytes = ctxlake_sync::codec::encode(envelopes).unwrap();
        let seg = ctxlake_store::layout::session_segment(
            date,
            fleet,
            Runtime::ClaudeCode,
            "cc-01",
            session,
            0,
        );
        store.put(&seg, PutPayload::from(bytes)).await.unwrap();
        let sealed = ctxlake_store::layout::session_sealed(
            date,
            fleet,
            Runtime::ClaudeCode,
            "cc-01",
            session,
        );
        store
            .put(&sealed, PutPayload::from(b"{}".to_vec()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn compacting_an_empty_partition_is_a_clean_no_op() {
        let store = InMemory::new();
        let outcome = run(&store, "2026-09-11", "oxidant").await.unwrap();
        assert_eq!(outcome.sealed_session_count, 0);
        assert_eq!(outcome.parts_written, 0);
        assert!(
            !outcome.skipped,
            "the very first run over nothing still writes a marker"
        );
    }

    #[tokio::test]
    async fn compaction_pools_every_sealed_session_into_a_part() {
        let store = InMemory::new();
        seal_session(
            &store,
            "2026-09-11",
            "oxidant",
            "sess-1",
            &[sample_envelope("sess-1", 0, Some("hello"))],
        )
        .await;
        seal_session(
            &store,
            "2026-09-11",
            "oxidant",
            "sess-2",
            &[sample_envelope("sess-2", 0, Some("world"))],
        )
        .await;

        let outcome = run(&store, "2026-09-11", "oxidant").await.unwrap();
        assert_eq!(outcome.sealed_session_count, 2);
        assert_eq!(outcome.rows_in, 2);
        assert_eq!(outcome.rows_out, 2);
        assert_eq!(outcome.parts_written, 1);

        let part = ctxlake_store::layout::sessions_compacted_part("2026-09-11", "oxidant", 0);
        let bytes = store.get(&part).await.unwrap().bytes().await.unwrap();
        let decoded = ctxlake_sync::codec::decode(&bytes).unwrap();
        assert_eq!(decoded.len(), 2);
    }

    #[tokio::test]
    async fn a_session_still_being_written_never_appears_in_compaction() {
        let store = InMemory::new();
        // No _SEALED marker: this segment must be invisible to compaction.
        let seg = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-live",
            0,
        );
        let bytes =
            ctxlake_sync::codec::encode(&[sample_envelope("sess-live", 0, Some("x"))]).unwrap();
        store.put(&seg, PutPayload::from(bytes)).await.unwrap();

        let outcome = run(&store, "2026-09-11", "oxidant").await.unwrap();
        assert_eq!(
            outcome.sealed_session_count, 0,
            "an unsealed session must not be counted or read"
        );
    }

    #[tokio::test]
    async fn rerunning_over_an_already_compacted_day_is_a_no_op_not_a_duplication() {
        let store = InMemory::new();
        seal_session(
            &store,
            "2026-09-11",
            "oxidant",
            "sess-1",
            &[sample_envelope("sess-1", 0, Some("hello"))],
        )
        .await;

        let first = run(&store, "2026-09-11", "oxidant").await.unwrap();
        assert!(!first.skipped);
        assert_eq!(first.parts_written, 1);

        let part_before = store
            .get(&ctxlake_store::layout::sessions_compacted_part(
                "2026-09-11",
                "oxidant",
                0,
            ))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();

        let second = run(&store, "2026-09-11", "oxidant").await.unwrap();
        assert!(
            second.skipped,
            "an unchanged partition must be recognized and skipped"
        );
        assert_eq!(second.rows_out, first.rows_out);

        let part_after = store
            .get(&ctxlake_store::layout::sessions_compacted_part(
                "2026-09-11",
                "oxidant",
                0,
            ))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(
            part_before, part_after,
            "a skipped run must not have touched the compacted output at all"
        );
    }

    #[tokio::test]
    async fn a_newly_sealed_session_triggers_recompaction_not_a_permanent_skip() {
        let store = InMemory::new();
        seal_session(
            &store,
            "2026-09-11",
            "oxidant",
            "sess-1",
            &[sample_envelope("sess-1", 0, Some("hello"))],
        )
        .await;
        run(&store, "2026-09-11", "oxidant").await.unwrap();

        seal_session(
            &store,
            "2026-09-11",
            "oxidant",
            "sess-2",
            &[sample_envelope("sess-2", 0, Some("world"))],
        )
        .await;
        let second = run(&store, "2026-09-11", "oxidant").await.unwrap();
        assert!(
            !second.skipped,
            "a newly sealed session must invalidate the marker"
        );
        assert_eq!(second.sealed_session_count, 2);
        assert_eq!(second.rows_out, 2);
    }

    #[tokio::test]
    async fn the_same_content_hash_across_50_sessions_is_stored_once() {
        let store = InMemory::new();
        for i in 0..50 {
            let session = format!("sess-{i}");
            seal_session(
                &store,
                "2026-09-11",
                "oxidant",
                &session,
                &[sample_envelope(
                    &session,
                    0,
                    Some("the entire text of AGENTS.md"),
                )],
            )
            .await;
        }

        let outcome = run(&store, "2026-09-11", "oxidant").await.unwrap();
        assert_eq!(outcome.sealed_session_count, 50);
        assert_eq!(outcome.rows_in, 50);
        assert_eq!(
            outcome.rows_out, 1,
            "50 sessions with byte-identical content must dedup to one row"
        );
    }

    #[tokio::test]
    async fn contentless_tool_calls_are_never_deduped_against_each_other() {
        // The regression this module's own doc warns about: every envelope that
        // never set `content` shares hash::content_hash(""). Naive dedup on that
        // value alone would collapse 3 genuinely distinct failing commands into 1
        // row, destroying exactly the arithmetic ctxlake_maint::digest depends on.
        let store = InMemory::new();
        let mut envelopes = Vec::new();
        for n in 0..3 {
            let mut e = sample_envelope("sess-1", n, None);
            e.tool = Some(ctxlake_core::envelope::ToolCall {
                name: "Bash".into(),
                input: Some(format!("cmd-{n}")),
                input_hash: hash::content_hash(format!("cmd-{n}")),
                result: None,
                exit_code: Some(1),
                duration_ms: None,
                paths: vec![],
            });
            envelopes.push(e);
        }
        seal_session(&store, "2026-09-11", "oxidant", "sess-1", &envelopes).await;

        let outcome = run(&store, "2026-09-11", "oxidant").await.unwrap();
        assert_eq!(outcome.rows_in, 3);
        assert_eq!(
            outcome.rows_out, 3,
            "contentless rows must all survive individually, not collapse to one"
        );
    }

    #[tokio::test]
    async fn a_large_partition_is_split_into_more_than_one_part() {
        let store = InMemory::new();
        // 5 sessions of 50 rows each with sizeable, non-dedupable content — enough
        // bytes, at a tiny forced part-size, to prove chunk_into_parts actually cuts.
        for s in 0..5 {
            let session = format!("sess-{s}");
            let envelopes: Vec<Envelope> = (0..50)
                .map(|n| {
                    sample_envelope(
                        &session,
                        n,
                        Some(&format!("payload {s}-{n} {}", "x".repeat(200))),
                    )
                })
                .collect();
            seal_session(&store, "2026-09-11", "oxidant", &session, &envelopes).await;
        }

        let outcome = run_with_part_size(&store, "2026-09-11", "oxidant", 8 * 1024)
            .await
            .unwrap();
        assert_eq!(outcome.rows_in, 250);
        assert_eq!(outcome.rows_out, 250);
        assert!(
            outcome.parts_written > 1,
            "expected more than one part at an 8KiB target, got {}",
            outcome.parts_written
        );

        // Every row must still be recoverable, exactly once, across all parts.
        let mut total = 0usize;
        for i in 0..outcome.parts_written {
            let part =
                ctxlake_store::layout::sessions_compacted_part("2026-09-11", "oxidant", i as u32);
            let bytes = store.get(&part).await.unwrap().bytes().await.unwrap();
            total += ctxlake_sync::codec::decode(&bytes).unwrap().len();
        }
        assert_eq!(total, 250);
    }

    #[tokio::test]
    async fn discover_dates_finds_every_dt_partition_and_ignores_compacted() {
        let store = InMemory::new();
        seal_session(
            &store,
            "2026-09-10",
            "oxidant",
            "sess-1",
            &[sample_envelope("sess-1", 0, Some("x"))],
        )
        .await;
        seal_session(
            &store,
            "2026-09-11",
            "oxidant",
            "sess-2",
            &[sample_envelope("sess-2", 0, Some("y"))],
        )
        .await;
        run(&store, "2026-09-10", "oxidant").await.unwrap(); // writes into sessions/compacted/dt=2026-09-10/...

        let dates = discover_dates(&store).await.unwrap();
        assert_eq!(
            dates,
            vec!["2026-09-10".to_string(), "2026-09-11".to_string()]
        );
    }
}
