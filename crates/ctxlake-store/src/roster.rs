//! `live/roster.json` — fan-in of every agent's intent into one object.
//!
//! The problem this solves: if every agent polled `live/agents/*.json` directly to
//! see who else is working, N agents polling means N `LIST`s and up to N `GET`s
//! each, every poll interval — O(N^2) requests as the fleet grows. At 50 agents
//! polling every 5 seconds, that is roughly 500 requests/sec sustained, which is
//! real S3 bill money (~$650/mo range) for infrastructure whose whole premise is
//! "nothing to operate, nothing to pay for when idle."
//!
//! The fix: elect one agent — whoever holds [`crate::layout::lease_maintenance`] —
//! to do the O(N) work once per interval: list every intent, merge them into a
//! single `live/roster.json` ([`build`]). Everyone else does exactly one
//! conditional `GET` per interval ([`fetch`]) with `If-None-Match` set to the etag
//! they last saw; an unchanged roster costs a 304 with no body transfer. That is
//! O(N) requests fleet-wide instead of O(N^2) — at 50 agents, roughly $23/mo
//! instead of ~$650/mo for the identical information.
//!
//! The fallback: the maintenance lease is advisory (AGENTS.md invariant 5), so if
//! nobody currently holds it — the fleet just started, or the sole maintainer died
//! and nobody has stolen the role yet — `roster.json` may be missing or stale, and
//! trusting it would mean showing everyone a roster from before the outage.
//! [`fetch`] checks the lease first and falls back to `list_intents_directly`
//! (the O(N) LIST + GETs, done ad hoc by whichever caller needs an answer right
//! now) whenever no one is building it.

use futures::StreamExt;
use object_store::{
    Error as OsError, GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutPayload,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::StoreError;
use crate::intent::Intent;
use crate::layout;
use crate::lease;

/// A merged view of every agent's intent, plus how it was produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RosterSnapshot {
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub source: RosterSource,
    pub agents: Vec<Intent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RosterSource {
    /// Produced by the maintenance-lease holder's periodic [`build`].
    RosterBuild,
    /// Produced ad hoc by a caller because no one currently holds the maintenance
    /// lease — see the module doc.
    DirectList,
}

/// The outcome of one [`build`] attempt.
#[derive(Debug)]
pub enum BuildOutcome {
    Published(RosterSnapshot),
    /// Another builder's write landed first this cycle (its snapshot is from the
    /// same instant, so nothing of value was lost) — not an error.
    Skipped,
}

/// List every `live/agents/*.json`, merge into a fresh snapshot. Best-effort per
/// entry: one agent's intent failing to parse (a partial write caught mid-flight, a
/// future schema this build predates) does not fail the whole roster — it is simply
/// left out of this cycle, and a healthy agent keeps re-publishing its intent, so a
/// transient miss self-heals within one interval.
async fn list_intents_directly(store: &dyn ObjectStore) -> Result<RosterSnapshot, StoreError> {
    let prefix = layout::agents_prefix();
    let mut stream = store.list(Some(&prefix));
    let mut agents = Vec::new();
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        let Ok(res) = store.get(&meta.location).await else {
            continue;
        };
        let Ok(bytes) = res.bytes().await else {
            continue;
        };
        if let Ok(intent) = serde_json::from_slice::<Intent>(&bytes) {
            agents.push(intent);
        }
    }
    Ok(RosterSnapshot {
        generated_at: OffsetDateTime::now_utc(),
        source: RosterSource::DirectList,
        agents,
    })
}

/// Merge every current intent into `live/roster.json`.
///
/// Callers should only invoke this while holding [`crate::layout::lease_maintenance`]
/// — this function does not check the lease itself, so that a caller doing its own
/// lease bookkeeping (renewing, deciding whether to keep building) isn't forced
/// through a redundant read here too. The write is still CAS'd against whatever
/// version of `roster.json` this call observed: `roster.json` is fully recomputed
/// every cycle rather than accumulated, so a lost race here only ever discards a
/// competing snapshot from the same instant — the CAS exists to stop a builder whose
/// lease already expired from clobbering a *newer* roster with a *staler* one, not
/// to protect against data loss (there isn't any to lose).
pub async fn build(store: &dyn ObjectStore) -> Result<BuildOutcome, StoreError> {
    let mut snapshot = list_intents_directly(store).await?;
    snapshot.source = RosterSource::RosterBuild;

    let path = layout::roster();
    let mode = match store.get(&path).await {
        Ok(res) => PutMode::Update(object_store::UpdateVersion {
            e_tag: res.meta.e_tag.clone(),
            version: res.meta.version.clone(),
        }),
        Err(OsError::NotFound { .. }) => PutMode::Create,
        Err(e) => return Err(e.into()),
    };

    let payload = PutPayload::from(serde_json::to_vec(&snapshot)?);
    match store.put_opts(&path, payload, mode.into()).await {
        Ok(_) => Ok(BuildOutcome::Published(snapshot)),
        Err(OsError::Precondition { .. }) | Err(OsError::AlreadyExists { .. }) => {
            Ok(BuildOutcome::Skipped)
        }
        Err(e) => Err(e.into()),
    }
}

/// What a poller sees after one [`fetch`].
#[derive(Debug)]
pub enum Roster {
    /// A fresh roster, with the etag to pass back in as `prior_etag` next time.
    Fresh {
        snapshot: RosterSnapshot,
        etag: Option<String>,
    },
    /// The roster is unchanged since `prior_etag` — the 304 outcome this whole
    /// module exists to make cheap.
    Unchanged,
}

/// Fetch the roster the cheap way when possible, falling back to a direct listing
/// when there is no maintenance-lease holder to trust `roster.json`'s freshness —
/// see the module doc.
pub async fn fetch(
    store: &dyn ObjectStore,
    prior_etag: Option<&str>,
) -> Result<Roster, StoreError> {
    let maintenance = lease::read(store, &layout::lease_maintenance()).await?;
    if maintenance.holder.is_none() {
        let snapshot = list_intents_directly(store).await?;
        return Ok(Roster::Fresh {
            snapshot,
            etag: None,
        });
    }

    let opts = GetOptions {
        if_none_match: prior_etag.map(str::to_string),
        ..Default::default()
    };
    match store.get_opts(&layout::roster(), opts).await {
        Ok(res) => {
            let etag = res.meta.e_tag.clone();
            let snapshot = serde_json::from_slice(&res.bytes().await?)?;
            Ok(Roster::Fresh { snapshot, etag })
        }
        Err(OsError::NotModified { .. }) => Ok(Roster::Unchanged),
        Err(OsError::NotFound { .. }) => {
            // A builder holds the lease but hasn't published its first roster yet
            // (just took over). Same remedy as no builder at all.
            let snapshot = list_intents_directly(store).await?;
            Ok(Roster::Fresh {
                snapshot,
                etag: None,
            })
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use ctxlake_core::Runtime;
    use object_store::memory::InMemory;
    use std::time::Duration;

    async fn seed_intent(store: &dyn ObjectStore, agent_id: &str) {
        let intent = Intent {
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
        };
        crate::intent::write(store, &intent).await.unwrap();
    }

    #[tokio::test]
    async fn build_merges_every_intent() {
        let store = InMemory::new();
        seed_intent(&store, "cc-01").await;
        seed_intent(&store, "cc-02").await;
        let BuildOutcome::Published(snapshot) = build(&store).await.unwrap() else {
            panic!("expected a fresh publish");
        };
        let mut ids: Vec<_> = snapshot.agents.iter().map(|a| a.agent_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["cc-01".to_string(), "cc-02".to_string()]);
        assert_eq!(snapshot.source, RosterSource::RosterBuild);
    }

    #[tokio::test]
    async fn fetch_without_a_maintainer_falls_back_to_direct_list() {
        let store = InMemory::new();
        seed_intent(&store, "cc-01").await;
        // No one has ever acquired the maintenance lease.
        let Roster::Fresh { snapshot, .. } = fetch(&store, None).await.unwrap() else {
            panic!("expected a fresh (fallback) roster");
        };
        assert_eq!(snapshot.source, RosterSource::DirectList);
        assert_eq!(snapshot.agents.len(), 1);
    }

    #[tokio::test]
    async fn fetch_reports_unchanged_on_a_matching_etag() {
        let store = InMemory::new();
        seed_intent(&store, "cc-01").await;
        let clock = SystemClock;
        let outcome = lease::acquire(
            &store,
            &clock,
            &layout::lease_maintenance(),
            "maintainer-1",
            Some("roster-builder"),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, lease::AcquireOutcome::Acquired(_)));
        build(&store).await.unwrap();

        let Roster::Fresh { etag, .. } = fetch(&store, None).await.unwrap() else {
            panic!("expected a fresh roster on the first fetch");
        };
        let etag = etag.expect("a real backend roster should carry an etag");

        let second = fetch(&store, Some(&etag)).await.unwrap();
        assert!(matches!(second, Roster::Unchanged));
    }

    #[tokio::test]
    async fn a_stale_read_cannot_win_a_cas_write_after_a_fresher_publish() {
        // build() itself always CASes against a version it just read, so it can
        // never observe this race internally. What we're really guarding is the
        // primitive build() relies on: prove directly that a version read *before*
        // a fresher publish is rejected by the store, not merely trusted to be.
        let store = InMemory::new();
        seed_intent(&store, "cc-01").await;
        build(&store).await.unwrap();

        // A slow builder reads roster.json here...
        let stale = store.get(&layout::roster()).await.unwrap();
        let stale_version = object_store::UpdateVersion {
            e_tag: stale.meta.e_tag.clone(),
            version: stale.meta.version.clone(),
        };

        // ...but a second intent lands and a fresher roster is published before
        // the slow builder gets to write.
        seed_intent(&store, "cc-02").await;
        build(&store).await.unwrap();

        // The slow builder's write, still holding the pre-fresher-publish version,
        // must be rejected — a lost race here would silently regress the roster
        // back to one agent.
        let result = store
            .put_opts(
                &layout::roster(),
                PutPayload::from(b"{}".to_vec()),
                PutMode::Update(stale_version).into(),
            )
            .await;
        assert!(matches!(result, Err(OsError::Precondition { .. })));
    }
}
