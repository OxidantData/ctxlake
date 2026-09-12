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
//! nobody currently *and actually* holds it — the fleet just started, or the sole
//! maintainer died and nobody has stolen the role yet — `roster.json` may be
//! missing or stale, and trusting it would mean showing everyone a roster from
//! before the outage. [`fetch`] checks the lease's TTL, not merely whether it has
//! a `holder` on file, and falls back to `list_intents_directly` (the O(N) LIST +
//! GETs, done ad hoc by whichever caller needs an answer right now) whenever no
//! one is *live* maintaining it — a `holder` whose TTL already lapsed counts as
//! nobody, because the doc's own claim above ("the sole maintainer died") is
//! exactly a lease left `Some(dead_agent)` past its expiry, not a lease that
//! reverted to `None`. Checking only `holder.is_none()` would keep trusting a
//! roster nobody has refreshed since the outage started — see the regression test.

use futures::StreamExt;
use object_store::{
    Error as OsError, GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutPayload,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::clock::Clock;
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
///
/// The first-ever publish (no `roster.json` yet) uses `PutMode::Overwrite`, not
/// `Create` — unlike a lease, there is no "someone already holds this, don't
/// clobber it" state to protect against here (see the paragraph above: any prior
/// content this build might overwrite is itself disposable), so there is nothing
/// `Create`'s create-if-absent semantics would buy that a plain write doesn't
/// already give for free, and `Create` is the one primitive AGENTS.md invariant 4
/// says never to depend on (MinIO rejects `If-None-Match: *` outright).
pub async fn build(store: &dyn ObjectStore) -> Result<BuildOutcome, StoreError> {
    let mut snapshot = list_intents_directly(store).await?;
    snapshot.source = RosterSource::RosterBuild;

    let path = layout::roster();
    let mode = match store.get(&path).await {
        Ok(res) => PutMode::Update(object_store::UpdateVersion {
            e_tag: res.meta.e_tag.clone(),
            version: res.meta.version.clone(),
        }),
        Err(OsError::NotFound { .. }) => PutMode::Overwrite,
        Err(e) => return Err(e.into()),
    };

    let payload = PutPayload::from(serde_json::to_vec(&snapshot)?);
    match store.put_opts(&path, payload, mode.into()).await {
        Ok(_) => Ok(BuildOutcome::Published(snapshot)),
        Err(OsError::Precondition { .. }) => Ok(BuildOutcome::Skipped),
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
/// when there is no *live* maintenance-lease holder to trust `roster.json`'s
/// freshness — see the module doc. `clock` answers "is the lease's TTL still
/// current," the same question `lease::acquire` asks before stealing, and for the
/// same reason (AGENTS.md invariant 6): a lapsed maintainer must be judged lapsed
/// by the store's clock, not by whichever caller happens to be polling.
pub async fn fetch(
    store: &dyn ObjectStore,
    clock: &dyn Clock,
    prior_etag: Option<&str>,
) -> Result<Roster, StoreError> {
    let maintenance = lease::read(store, &layout::lease_maintenance()).await?;
    let now = OffsetDateTime::from(clock.now().await?);
    if maintenance.stealable(now) {
        // Nobody is *live*-maintaining the roster right now: either nobody has
        // ever held the lease, or the last holder's TTL has already lapsed. A
        // `holder` on file past its own expiry (the sole-maintainer-died case the
        // module doc describes) must be treated exactly like no holder at all —
        // trusting `roster.json` here means trusting a snapshot nobody has
        // refreshed since the outage began.
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
        let clock = SystemClock;
        seed_intent(&store, "cc-01").await;
        // No one has ever acquired the maintenance lease.
        let Roster::Fresh { snapshot, .. } = fetch(&store, &clock, None).await.unwrap() else {
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
        lease::provision(&store, &layout::lease_maintenance())
            .await
            .unwrap();
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

        let Roster::Fresh { etag, .. } = fetch(&store, &clock, None).await.unwrap() else {
            panic!("expected a fresh roster on the first fetch");
        };
        let etag = etag.expect("a real backend roster should carry an etag");

        let second = fetch(&store, &clock, Some(&etag)).await.unwrap();
        assert!(matches!(second, Roster::Unchanged));
    }

    #[tokio::test]
    async fn fetch_falls_back_once_the_sole_maintainer_dies_past_its_ttl() {
        // Regression test: a maintainer that dies mid-hold leaves `holder:
        // Some(..)` on a lease whose TTL has lapsed — exactly the "sole
        // maintainer died and nobody has stolen the role yet" scenario the module
        // doc names. The old `fetch` checked only `holder.is_none()`, so it kept
        // trusting the pre-outage roster forever; a dead maintainer never looks
        // like "no maintainer" under that check.
        let store = InMemory::new();
        let clock = SystemClock;
        seed_intent(&store, "cc-01").await;

        lease::provision(&store, &layout::lease_maintenance())
            .await
            .unwrap();
        let outcome = lease::acquire(
            &store,
            &clock,
            &layout::lease_maintenance(),
            "maintainer-1",
            None,
            Duration::from_millis(10), // short TTL, deliberately left to lapse
        )
        .await
        .unwrap();
        assert!(matches!(outcome, lease::AcquireOutcome::Acquired(_)));
        build(&store).await.unwrap(); // publishes a roster containing only cc-01

        tokio::time::sleep(Duration::from_millis(30)).await; // TTL lapses; no release()

        // A new agent starts up after the outage began — nobody has rebuilt the
        // roster since, so a correct fetch must not report a roster that predates
        // cc-02 even existing.
        seed_intent(&store, "cc-02").await;

        let Roster::Fresh { snapshot, .. } = fetch(&store, &clock, None).await.unwrap() else {
            panic!("expected a fresh (fallback) roster once the maintainer's TTL lapsed");
        };
        assert_eq!(
            snapshot.source,
            RosterSource::DirectList,
            "a lapsed maintainer must be treated as no maintainer, not as a still-valid one"
        );
        let mut ids: Vec<_> = snapshot.agents.iter().map(|a| a.agent_id.clone()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["cc-01".to_string(), "cc-02".to_string()],
            "the fallback must see every currently-live agent, not the stale roster"
        );
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

    /// A minimal stand-in for MinIO's actual behavior (minio/minio#20346): every
    /// `PutMode::Create` fails with a plain backend error, not `AlreadyExists` —
    /// even the very first `Create` against a key nobody has ever written.
    /// Everything else passes straight through to `InMemory`.
    #[derive(Debug)]
    struct CreateAlwaysRejectingStore {
        inner: InMemory,
    }
    impl std::fmt::Display for CreateAlwaysRejectingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "CreateAlwaysRejectingStore({})", self.inner)
        }
    }
    #[async_trait::async_trait]
    impl ObjectStore for CreateAlwaysRejectingStore {
        async fn put_opts(
            &self,
            location: &object_store::path::Path,
            payload: PutPayload,
            opts: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            if matches!(opts.mode, PutMode::Create) {
                return Err(OsError::Generic {
                    store: "CreateAlwaysRejectingStore",
                    source: "If-None-Match: * is not supported (minio/minio#20346)".into(),
                });
            }
            self.inner.put_opts(location, payload, opts).await
        }
        async fn put_multipart_opts(
            &self,
            location: &object_store::path::Path,
            opts: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }
        async fn get_opts(
            &self,
            location: &object_store::path::Path,
            options: GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            self.inner.get_opts(location, options).await
        }
        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<
                'static,
                object_store::Result<object_store::path::Path>,
            >,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
        {
            self.inner.delete_stream(locations)
        }
        fn list(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }
        async fn list_with_delimiter(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }
        async fn copy_opts(
            &self,
            from: &object_store::path::Path,
            to: &object_store::path::Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    #[tokio::test]
    async fn build_publishes_the_first_roster_even_when_create_never_succeeds() {
        // Regression test: build() used to fall back to PutMode::Create for the
        // first-ever publish, which is exactly the request MinIO rejects outright
        // (minio/minio#20346) — not only on conflict, on a key that has never been
        // written either. roster.json needs no create-if-absent semantics at all
        // (it's fully recomputed every cycle, see build()'s doc), so there is no
        // reason to depend on the one primitive AGENTS.md invariant 4 forbids.
        let store = CreateAlwaysRejectingStore {
            inner: InMemory::new(),
        };
        seed_intent(&store, "cc-01").await;

        let outcome = build(&store).await.unwrap();
        let BuildOutcome::Published(snapshot) = outcome else {
            panic!("expected the first-ever roster publish to succeed");
        };
        assert_eq!(snapshot.agents.len(), 1);

        // A second cycle must also succeed as a normal CAS Update, proving the
        // very first write didn't quietly leave the key unwritten.
        seed_intent(&store, "cc-02").await;
        let BuildOutcome::Published(snapshot) = build(&store).await.unwrap() else {
            panic!("expected the second publish to succeed too");
        };
        assert_eq!(snapshot.agents.len(), 2);
    }
}
