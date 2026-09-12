//! `live/agents/<id>.json` — one agent's declaration of what it is doing right now.
//!
//! This key is single-writer by construction: the agent id *is* the key, and an
//! agent only ever writes its own file. CAS exists to arbitrate between two writers
//! racing for the same key; there is exactly one writer here, so guarding this write
//! with a read-modify-CAS cycle would buy nothing — a stale reader here isn't racing
//! anyone, it's just an old copy of a key nobody else touches, which is the ordinary
//! and expected case in a polling system (see `roster.rs`). A plain
//! [`PutMode::Overwrite`](object_store::PutMode::Overwrite) is not a shortcut taken
//! under time pressure; it is the correct write pattern for this specific key. This
//! narrows, rather than contradicts, AGENTS.md's "`live/` — CAS only": that rule is
//! about keys with real contention (leases); intents don't have any.

use object_store::{Error as OsError, ObjectStore, ObjectStoreExt, PutPayload};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::StoreError;
use crate::layout;

/// What an agent is doing right now, as it wants the rest of the fleet to see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Intent {
    pub agent_id: String,
    pub fleet_id: String,
    pub runtime: ctxlake_core::Runtime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// A short human-readable summary of the current task, for `ctxlake status` and
    /// collision warnings. Free text written by the agent (or its operator) — render
    /// it as untrusted, per AGENTS.md's house rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Paths this agent is actively touching, feeding collision detection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Overwrite `intent.agent_id`'s live record.
pub async fn write(store: &dyn ObjectStore, intent: &Intent) -> Result<(), StoreError> {
    let path = layout::agent_intent(&intent.agent_id);
    let payload = PutPayload::from(serde_json::to_vec(intent)?);
    store.put(&path, payload).await?;
    Ok(())
}

/// Read one agent's intent. `Ok(None)` means the agent has never published one (not
/// yet started, or has been torn down) — not an error.
pub async fn read(store: &dyn ObjectStore, agent_id: &str) -> Result<Option<Intent>, StoreError> {
    let path = layout::agent_intent(agent_id);
    match store.get(&path).await {
        Ok(res) => Ok(Some(serde_json::from_slice(&res.bytes().await?)?)),
        Err(OsError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::Runtime;
    use object_store::memory::InMemory;

    fn sample(agent_id: &str) -> Intent {
        Intent {
            agent_id: agent_id.to_string(),
            fleet_id: "oxidant".into(),
            runtime: Runtime::ClaudeCode,
            session_id: Some("sess-1".into()),
            repo: Some("github.com/OxidantData/ctxlake".into()),
            branch: Some("wave1/store".into()),
            cwd: None,
            task: Some("implementing ctxlake-store".into()),
            paths: vec!["crates/ctxlake-store/src/roster.rs".into()],
            updated_at: OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let store = InMemory::new();
        let intent = sample("cc-01");
        write(&store, &intent).await.unwrap();
        let back = read(&store, "cc-01").await.unwrap().unwrap();
        assert_eq!(back, intent);
    }

    #[tokio::test]
    async fn reading_an_agent_that_never_wrote_is_not_an_error() {
        let store = InMemory::new();
        assert_eq!(read(&store, "nobody-home").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_second_write_overwrites_rather_than_conflicting() {
        // The whole point of this module: no CAS, so a second write from the same
        // agent never fails, even against a version it never read.
        let store = InMemory::new();
        let mut intent = sample("cc-01");
        write(&store, &intent).await.unwrap();
        intent.task = Some("moved on to a different file".into());
        write(&store, &intent).await.unwrap();
        let back = read(&store, "cc-01").await.unwrap().unwrap();
        assert_eq!(back.task.as_deref(), Some("moved on to a different file"));
    }
}
