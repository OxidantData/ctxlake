//! `live/roster.json` — fan-in of every agent's intent into one object.
//!
//! The problem this solves: if every agent polled `live/agents/*.json` directly to
//! see who else is working, N agents polling means N `LIST`s and up to N `GET`s
//! each, every poll interval — O(N^2) requests as the fleet grows. At 50 agents
//! polling every 5 seconds, that is roughly 500 requests/sec sustained, which is
//! real S3 bill money (~$650/mo range) for infrastructure whose whole premise is
//! "nothing to operate, nothing to pay for when idle."
//!
//! The fix: any daemon may do the O(N) work — list every intent, merge them into
//! one `live/roster.json` ([`build`]) — and everyone else does exactly one
//! conditional `GET` per interval ([`fetch`]) with `If-None-Match` set to the etag
//! they last saw; an unchanged roster costs a 304 with no body transfer.
//!
//! **Several daemons building this concurrently is fine, not a bug.** `build`
//! publishes with a CAS write against whatever version it observed: two builders
//! racing produce two writes, at most one lands (the other's `412` is reported as
//! [`BuildOutcome::Skipped`], not an error), and because `roster.json` is fully
//! recomputed from a fresh listing every time rather than accumulated, there is
//! nothing for the loser's write to have contributed that the winner's doesn't
//! already contain. A reader following the pointer never sees a torn write either
//! way — an earlier version of this design elected exactly one builder via a
//! maintenance lease so that fleet-wide request volume stayed O(N) instead of
//! O(N^2). That lease is gone, and *correctness* does not need it back — but
//! naively letting every one of N daemons attempt this O(N) `build` on the same
//! fixed schedule would silently bring the O(N^2) cost back too (N daemons ×
//! O(N) work each, every interval), so [`BuildOutcome`] carries the *size* of
//! every build attempt's freshly-listed snapshot — win or lose the CAS race —
//! specifically so `ctxlake_sync::presence` can scale how often *this* daemon
//! tries again by how many daemons it just learned are actually out there. See
//! that module's doc for the arithmetic: the target is the same one build
//! fleet-wide per base interval the lease used to guarantee, reached without
//! ever agreeing on who's in charge.
//!
//! The only real fallback left is bootstrap: nobody has ever published
//! `roster.json` yet (a fleet that just started, before any daemon's first
//! build has landed). [`fetch`] falls back to [`list_intents_directly`] — the
//! O(N) LIST + GETs, done ad hoc by whichever caller needs an answer right now —
//! exactly in that case, and self-heals the moment any daemon's first `build`
//! succeeds.

use futures::StreamExt;
use object_store::{
    Error as OsError, GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutPayload,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::StoreError;
use crate::intent::Intent;
use crate::layout;

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
    /// Produced by some daemon's periodic [`build`] — any number of daemons may
    /// produce this, see the module doc.
    RosterBuild,
    /// Produced ad hoc by a caller because nobody has ever published
    /// `roster.json` yet — see the module doc's bootstrap case.
    DirectList,
}

/// The outcome of one [`build`] attempt.
#[derive(Debug)]
pub enum BuildOutcome {
    Published(RosterSnapshot),
    /// Another builder's write landed first this cycle — not an error, and not
    /// an empty-handed result either: the snapshot carried here is this call's
    /// own freshly-listed view (computed before the losing CAS attempt), so a
    /// caller that only cares about *shape* — how many agents are out there
    /// right now, say — has it regardless of which of two racing builders'
    /// bytes actually landed. See `ctxlake_sync::presence`'s module doc for the
    /// caller that relies on exactly this: it is how a daemon learns the
    /// current fleet size to scale its own next rebuild attempt without ever
    /// winning a race.
    Skipped(RosterSnapshot),
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
/// Any caller may invoke this at any time — see the module doc for why several
/// daemons doing so concurrently is fine. The write is CAS'd against whatever
/// version of `roster.json` this call observed: `roster.json` is fully recomputed
/// every cycle rather than accumulated, so a lost race here only ever discards a
/// competing snapshot from the same instant — the CAS exists to stop a builder
/// working from a stale read from clobbering a *newer* roster with a *staler* one,
/// not to protect against data loss (there isn't any to lose).
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
        Err(OsError::Precondition { .. }) => Ok(BuildOutcome::Skipped(snapshot)),
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
/// only when nobody has ever published `roster.json` yet — see the module doc.
pub async fn fetch(
    store: &dyn ObjectStore,
    prior_etag: Option<&str>,
) -> Result<Roster, StoreError> {
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
            // Bootstrap: nobody has ever published `roster.json` yet. Self-heals
            // the moment any daemon's `build` succeeds.
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
    use ctxlake_core::Runtime;
    use object_store::memory::InMemory;

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
    async fn fetch_before_any_publish_falls_back_to_direct_list() {
        let store = InMemory::new();
        seed_intent(&store, "cc-01").await;
        // Nobody has ever called build() against this store yet.
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
        // This is the proof that several daemons building the roster concurrently
        // (the module doc's "fine, not a bug") can't corrupt it: build() itself
        // always CASes against a version it just read, so it can never observe
        // this race internally. What we're really guarding is the primitive
        // build() relies on: prove directly that a version read *before* a
        // fresher publish is rejected by the store, not merely trusted to be.
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
