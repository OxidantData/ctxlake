//! Parsing a bronze session's Hive-style partition segments back out of an object
//! key — the inverse of the paths `ctxlake_store::layout` constructs.
//!
//! `layout` owns *building* every key (AGENTS.md: "the single definition of the
//! on-disk layout"); it does not own *reading one back apart*, which is a concern
//! specific to whatever's walking the tree — here, maintenance discovering which
//! sessions exist to compact or digest. Duplicating one small parser here rather
//! than exporting it from `layout` mirrors `ctxlake-sync`'s own
//! `runtime_from_dir_name` (upload.rs): a local, narrow inverse of another crate's
//! forward-only construction, kept in sync by the layout round-trip test in
//! `layout.rs` itself rather than by a shared function neither crate would fully own.

use ctxlake_core::{envelope::Envelope, Runtime};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};

use crate::error::MaintError;

/// The identity of one session, recovered from its `_SEALED` marker's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPartition {
    pub date: String,
    pub fleet_id: String,
    pub runtime: Runtime,
    pub agent_id: String,
    pub session_id: String,
}

fn runtime_from_str(s: &str) -> Runtime {
    match s {
        "claude_code" => Runtime::ClaudeCode,
        "cursor" => Runtime::Cursor,
        "hermes" => Runtime::Hermes,
        _ => Runtime::Other,
    }
}

/// Parse `sessions/dt=<date>/fleet=<fleet>/runtime=<runtime>/agent=<agent>/session=<id>/_SEALED`
/// (or any other key inside that same session directory — `digest.json`, a
/// `seg-*.parquet`) back into its five partition values.
///
/// `None` for anything that doesn't match the shape exactly — a partial write, a
/// future partitioning scheme this build predates, or simply a key from an
/// unrelated prefix a caller listed too broadly. Best-effort on purpose: one
/// unparseable key found while walking a whole fleet's `sessions/` tree should be
/// skipped, not abort discovery for every session that *does* parse, matching the
/// tolerance `ctxlake_store::roster::list_intents_directly` already uses for the
/// same reason.
pub fn parse_session_partition(key: &Path) -> Option<SessionPartition> {
    let parts: Vec<&str> = key.as_ref().split('/').collect();
    // ["sessions", "dt=...", "fleet=...", "runtime=...", "agent=...", "session=...", <leaf>]
    if parts.len() < 7 || parts[0] != "sessions" {
        return None;
    }
    let date = parts[1].strip_prefix("dt=")?.to_string();
    let fleet_id = parts[2].strip_prefix("fleet=")?.to_string();
    let runtime = runtime_from_str(parts[3].strip_prefix("runtime=")?);
    let agent_id = parts[4].strip_prefix("agent=")?.to_string();
    let session_id = parts[5].strip_prefix("session=")?.to_string();
    Some(SessionPartition {
        date,
        fleet_id,
        runtime,
        agent_id,
        session_id,
    })
}

/// The directory a session's own keys (`seg-*.parquet`, `_SEALED`, `digest.json`)
/// live under, given its identity — built from [`ctxlake_store::layout::session_digest`]
/// with the filename trimmed off, rather than re-deriving the partition scheme a
/// third time. Both `layout::session_digest` and this function must agree on where
/// the filename boundary is; `session_dir_round_trips_through_layout` below is the
/// regression test that catches the two drifting apart.
pub fn session_dir(
    date: &str,
    fleet_id: &str,
    runtime: Runtime,
    agent_id: &str,
    session_id: &str,
) -> Path {
    let digest_key =
        ctxlake_store::layout::session_digest(date, fleet_id, runtime, agent_id, session_id);
    let s = digest_key.as_ref();
    let dir = s.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    Path::from(dir)
}

/// List and decode every `seg-*.parquet` file living in the same directory as
/// `sealed_marker` — shared by [`crate::compact`] (pooling a whole partition) and
/// [`crate::digest`] (one session at a time), so there is exactly one place that
/// knows a sealed session's segments sit next to its `_SEALED` marker.
pub(crate) async fn read_session_segments(
    store: &dyn ObjectStore,
    sealed_marker: &Path,
) -> Result<Vec<Envelope>, MaintError> {
    use futures::StreamExt;
    let dir = sealed_marker
        .as_ref()
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let prefix = Path::from(dir);
    let mut stream = store.list(Some(&prefix));
    let mut seg_paths = Vec::new();
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        if meta.location.as_ref().ends_with(".parquet") {
            seg_paths.push(meta.location);
        }
    }
    seg_paths.sort_by(|a, b| a.as_ref().cmp(b.as_ref())); // seg-NNNNNN is zero-padded: lexicographic == numeric.

    let mut out = Vec::new();
    for seg in &seg_paths {
        let bytes = store.get(seg).await?.bytes().await?;
        let decoded = ctxlake_sync::codec::decode(&bytes).map_err(MaintError::Codec)?;
        out.extend(decoded);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_sealed_marker_into_every_field() {
        let key = ctxlake_store::layout::session_sealed(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
        );
        let p = parse_session_partition(&key).expect("should parse");
        assert_eq!(p.date, "2026-09-11");
        assert_eq!(p.fleet_id, "oxidant");
        assert_eq!(p.runtime, Runtime::ClaudeCode);
        assert_eq!(p.agent_id, "cc-01");
        assert_eq!(p.session_id, "sess-1");
    }

    #[test]
    fn parses_a_segment_key_the_same_way_as_a_sealed_marker() {
        let key = ctxlake_store::layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::Cursor,
            "cur-01",
            "sess-9",
            3,
        );
        let p = parse_session_partition(&key).expect("should parse");
        assert_eq!(p.runtime, Runtime::Cursor);
        assert_eq!(p.session_id, "sess-9");
    }

    #[test]
    fn refuses_a_key_outside_the_sessions_tree() {
        assert!(parse_session_partition(&Path::from("snapshot/latest.json")).is_none());
        assert!(parse_session_partition(&Path::from(
            "sessions/compacted/dt=2026-09-11/fleet=oxidant/part-000000.parquet"
        ))
        .is_none());
    }

    #[tokio::test]
    async fn read_session_segments_decodes_every_segment_in_order() {
        use ctxlake_core::EventType;
        use object_store::memory::InMemory;
        use object_store::PutPayload;

        let store = InMemory::new();
        let mk = |n: u32| {
            let mut e = ctxlake_core::Envelope::new(
                "oxidant",
                "cc-01",
                Runtime::ClaudeCode,
                "sess-1",
                EventType::ToolCall,
                format!("2026-09-11T18:22:{n:02}.000Z"),
            );
            e.content = Some(format!("line {n}"));
            e
        };
        for (seg, env) in [mk(0), mk(1)].into_iter().enumerate() {
            let bytes = ctxlake_sync::codec::encode(&[env]).unwrap();
            let path = ctxlake_store::layout::session_segment(
                "2026-09-11",
                "oxidant",
                Runtime::ClaudeCode,
                "cc-01",
                "sess-1",
                seg as u32,
            );
            store.put(&path, PutPayload::from(bytes)).await.unwrap();
        }
        let sealed = ctxlake_store::layout::session_sealed(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
        );
        store
            .put(&sealed, PutPayload::from_static(b"{}"))
            .await
            .unwrap();

        let envelopes = read_session_segments(&store, &sealed).await.unwrap();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].content.as_deref(), Some("line 0"));
        assert_eq!(envelopes[1].content.as_deref(), Some("line 1"));
    }

    #[test]
    fn session_dir_round_trips_through_layout() {
        let dir = session_dir("2026-09-11", "oxidant", Runtime::Hermes, "hm-01", "sess-3");
        let expected_sealed = ctxlake_store::layout::session_sealed(
            "2026-09-11",
            "oxidant",
            Runtime::Hermes,
            "hm-01",
            "sess-3",
        );
        assert_eq!(dir.join("_SEALED"), expected_sealed);
    }
}
