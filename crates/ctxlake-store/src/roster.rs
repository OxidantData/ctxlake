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
//! O(N^2); see `ctxlake_sync::presence`'s module doc for how that cost is now
//! controlled instead (a cheap per-daemon backoff, not a lock).
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
use time::{Duration, OffsetDateTime};

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
    /// Another builder's write landed first this cycle (its snapshot is from the
    /// same instant, so nothing of value was lost) — not an error.
    Skipped,
}

/// List every `live/agents/*.json`, merge into a fresh snapshot. Best-effort per
/// entry: one agent's intent failing to parse (a partial write caught mid-flight, a
/// future schema this build predates) does not fail the whole roster — it is simply
/// left out of this cycle, and a healthy agent keeps re-publishing its intent, so a
/// transient miss self-heals within one interval.
async fn list_intents_directly(
    store: &dyn ObjectStore,
    fleet_id: &str,
) -> Result<RosterSnapshot, StoreError> {
    let prefix = layout::agents_prefix(fleet_id);
    let mut stream = store.list(Some(&prefix));
    let now = OffsetDateTime::now_utc();
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
            // Belt and braces on top of the scoped prefix. The prefix is what makes
            // the listing cheap and correct; this is what makes a mis-keyed write —
            // an older build, a hand-copied object — unable to put another fleet's
            // agent in front of this one's operators.
            if intent.fleet_id != fleet_id {
                continue;
            }
            if is_expired(&intent, now) {
                continue;
            }
            agents.push(intent);
        }
    }
    Ok(RosterSnapshot {
        generated_at: now,
        source: RosterSource::DirectList,
        agents,
    })
}

/// How long after its last heartbeat an agent stays in the roster.
///
/// The daemon refreshes presence about once a minute, so this is five missed
/// refreshes. Generous on purpose: the comparison is one machine's clock against
/// another's, with no shared clock anywhere in this design, and the two failure modes
/// are not symmetric. Expiring a live agent too eagerly makes a working agent
/// invisible to collision checks; keeping a dead one a few minutes too long shows a
/// stale row in `ctxlake status`.
pub const PRESENCE_TTL: Duration = Duration::minutes(5);

/// Whether this intent is too old to belong in a roster generated at `now`.
///
/// Without this there was no expiry at all — nothing in the system ever removed an
/// agent that stopped writing. `docs/how-it-works.md` said an agent that stops
/// "simply ages out", and on a real lake a host that had been off for **226 minutes**
/// was still being reported as active, in a fleet it did not even belong to.
///
/// A timestamp in the future is not treated as expired: that is a clock ahead of this
/// one, and dropping a live agent over skew is the worse of the two errors.
fn is_expired(intent: &Intent, now: OffsetDateTime) -> bool {
    now - intent.updated_at > PRESENCE_TTL
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
pub async fn build(store: &dyn ObjectStore, fleet_id: &str) -> Result<BuildOutcome, StoreError> {
    let mut snapshot = list_intents_directly(store, fleet_id).await?;
    snapshot.source = RosterSource::RosterBuild;

    let path = layout::roster(fleet_id);
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
/// only when nobody has ever published `roster.json` yet — see the module doc.
pub async fn fetch(
    store: &dyn ObjectStore,
    fleet_id: &str,
    prior_etag: Option<&str>,
) -> Result<Roster, StoreError> {
    let opts = GetOptions {
        if_none_match: prior_etag.map(str::to_string),
        ..Default::default()
    };
    match store.get_opts(&layout::roster(fleet_id), opts).await {
        Ok(res) => {
            let etag = res.meta.e_tag.clone();
            let snapshot = serde_json::from_slice(&res.bytes().await?)?;
            Ok(Roster::Fresh { snapshot, etag })
        }
        Err(OsError::NotModified { .. }) => Ok(Roster::Unchanged),
        Err(OsError::NotFound { .. }) => {
            // Bootstrap: nobody has ever published `roster.json` yet. Self-heals
            // the moment any daemon's `build` succeeds.
            let snapshot = list_intents_directly(store, fleet_id).await?;
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
        seed_intent_in(store, "oxidant", agent_id, OffsetDateTime::now_utc()).await;
    }

    async fn seed_intent_in(
        store: &dyn ObjectStore,
        fleet_id: &str,
        agent_id: &str,
        updated_at: OffsetDateTime,
    ) {
        let intent = Intent {
            agent_id: agent_id.to_string(),
            fleet_id: fleet_id.into(),
            runtime: Runtime::ClaudeCode,
            session_id: None,
            repo: None,
            branch: None,
            cwd: None,
            task: None,
            paths: vec![],
            updated_at,
        };
        crate::intent::write(store, &intent).await.unwrap();
    }

    #[tokio::test]
    async fn one_fleets_roster_never_contains_another_fleets_agents() {
        // Seen on a real lake: `ctxlake status` reported "fleet oxidantdata-dev · 3
        // agent(s) active" with one of the three belonging to a fleet called `demo`.
        // `docs/getting-started.md` calls --fleet "the boundary of who sees whom", and
        // this was the boundary not existing — in `status`, and in the briefing, which
        // means another fleet's agents were being described into context windows.
        let store = InMemory::new();
        let now = OffsetDateTime::now_utc();
        seed_intent_in(&store, "ours", "cc-01", now).await;
        seed_intent_in(&store, "theirs", "cc-99", now).await;

        let BuildOutcome::Published(snapshot) = build(&store, "ours").await.unwrap() else {
            panic!("expected a fresh publish");
        };
        let ids: Vec<_> = snapshot.agents.iter().map(|a| &a.agent_id).collect();
        assert_eq!(ids, vec!["cc-01"], "another fleet's agent leaked in");

        // And the fallback path, which is a separate listing and leaked identically.
        let other = InMemory::new();
        seed_intent_in(&other, "ours", "cc-01", now).await;
        seed_intent_in(&other, "theirs", "cc-99", now).await;
        let Roster::Fresh { snapshot, .. } = fetch(&other, "ours", None).await.unwrap() else {
            panic!("expected the direct-list fallback");
        };
        assert_eq!(snapshot.source, RosterSource::DirectList);
        let ids: Vec<_> = snapshot.agents.iter().map(|a| &a.agent_id).collect();
        assert_eq!(ids, vec!["cc-01"], "the fallback leaked another fleet");
    }

    #[tokio::test]
    async fn an_object_mis_keyed_into_this_fleets_prefix_is_still_excluded() {
        // The scoped prefix is the mechanism; this is the check behind it. An object
        // whose *body* names another fleet can land in this prefix from an older
        // build that wrote the flat layout, a hand-copied key, or a restored backup —
        // and the prefix cannot help with any of those. Without the body check the
        // leak comes straight back, which is why the test writes the bad key
        // directly rather than through `intent::write`.
        let store = InMemory::new();
        seed_intent_in(&store, "ours", "cc-01", OffsetDateTime::now_utc()).await;

        let interloper = Intent {
            agent_id: "cc-99".into(),
            fleet_id: "theirs".into(),
            runtime: Runtime::ClaudeCode,
            session_id: None,
            repo: None,
            branch: None,
            cwd: None,
            task: None,
            paths: vec![],
            updated_at: OffsetDateTime::now_utc(),
        };
        store
            .put(
                &layout::agent_intent("ours", "cc-99"),
                object_store::PutPayload::from(serde_json::to_vec(&interloper).unwrap()),
            )
            .await
            .unwrap();

        let BuildOutcome::Published(snapshot) = build(&store, "ours").await.unwrap() else {
            panic!("expected a fresh publish");
        };
        let ids: Vec<_> = snapshot.agents.iter().map(|a| &a.agent_id).collect();
        assert_eq!(
            ids,
            vec!["cc-01"],
            "a record claiming another fleet must be excluded wherever it is keyed"
        );
    }

    #[tokio::test]
    async fn two_fleets_sharing_an_agent_id_do_not_share_a_key() {
        // The worse half of the flat layout: not a display bug but a data one. `cc-01`
        // in two fleets wrote the same object, so each host silently erased the
        // other's presence.
        let store = InMemory::new();
        let now = OffsetDateTime::now_utc();
        seed_intent_in(&store, "ours", "cc-01", now).await;
        seed_intent_in(&store, "theirs", "cc-01", now).await;

        for fleet in ["ours", "theirs"] {
            let got = crate::intent::read(&store, fleet, "cc-01")
                .await
                .unwrap()
                .expect("each fleet keeps its own record");
            assert_eq!(got.fleet_id, fleet);
        }
    }

    #[tokio::test]
    async fn an_agent_that_stopped_writing_ages_out_of_the_roster() {
        // There was no expiry at all — nothing in the system ever dropped an agent
        // that stopped heartbeating, while `docs/how-it-works.md` said one "simply
        // ages out". On a real lake a host that had been off for 226 minutes was
        // still being reported as active.
        let store = InMemory::new();
        let now = OffsetDateTime::now_utc();
        seed_intent_in(&store, "ours", "alive", now - Duration::seconds(90)).await;
        seed_intent_in(&store, "ours", "long-gone", now - Duration::hours(4)).await;

        let BuildOutcome::Published(snapshot) = build(&store, "ours").await.unwrap() else {
            panic!("expected a fresh publish");
        };
        let ids: Vec<_> = snapshot.agents.iter().map(|a| &a.agent_id).collect();
        assert_eq!(
            ids,
            vec!["alive"],
            "an agent past the TTL must not be reported as active"
        );
    }

    #[tokio::test]
    async fn a_clock_running_ahead_never_hides_a_live_agent() {
        // There is no shared clock in this design, so `updated_at` is another
        // machine's idea of the time. Of the two ways to be wrong, dropping a live
        // agent is the expensive one — it makes a working agent invisible to
        // collision checks — so a future timestamp is kept, not expired.
        let store = InMemory::new();
        let now = OffsetDateTime::now_utc();
        seed_intent_in(&store, "ours", "skewed-ahead", now + Duration::hours(2)).await;

        let BuildOutcome::Published(snapshot) = build(&store, "ours").await.unwrap() else {
            panic!("expected a fresh publish");
        };
        assert_eq!(snapshot.agents.len(), 1, "a clock ahead is not an absence");
    }

    #[tokio::test]
    async fn build_merges_every_intent() {
        let store = InMemory::new();
        seed_intent(&store, "cc-01").await;
        seed_intent(&store, "cc-02").await;
        let BuildOutcome::Published(snapshot) = build(&store, "oxidant").await.unwrap() else {
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
        let Roster::Fresh { snapshot, .. } = fetch(&store, "oxidant", None).await.unwrap() else {
            panic!("expected a fresh (fallback) roster");
        };
        assert_eq!(snapshot.source, RosterSource::DirectList);
        assert_eq!(snapshot.agents.len(), 1);
    }

    #[tokio::test]
    async fn fetch_reports_unchanged_on_a_matching_etag() {
        let store = InMemory::new();
        seed_intent(&store, "cc-01").await;
        build(&store, "oxidant").await.unwrap();

        let Roster::Fresh { etag, .. } = fetch(&store, "oxidant", None).await.unwrap() else {
            panic!("expected a fresh roster on the first fetch");
        };
        let etag = etag.expect("a real backend roster should carry an etag");

        let second = fetch(&store, "oxidant", Some(&etag)).await.unwrap();
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
        build(&store, "oxidant").await.unwrap();

        // A slow builder reads roster.json here...
        let stale = store.get(&layout::roster("oxidant")).await.unwrap();
        let stale_version = object_store::UpdateVersion {
            e_tag: stale.meta.e_tag.clone(),
            version: stale.meta.version.clone(),
        };

        // ...but a second intent lands and a fresher roster is published before
        // the slow builder gets to write.
        seed_intent(&store, "cc-02").await;
        build(&store, "oxidant").await.unwrap();

        // The slow builder's write, still holding the pre-fresher-publish version,
        // must be rejected — a lost race here would silently regress the roster
        // back to one agent.
        let result = store
            .put_opts(
                &layout::roster("oxidant"),
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

        let outcome = build(&store, "oxidant").await.unwrap();
        let BuildOutcome::Published(snapshot) = outcome else {
            panic!("expected the first-ever roster publish to succeed");
        };
        assert_eq!(snapshot.agents.len(), 1);

        // A second cycle must also succeed as a normal CAS Update, proving the
        // very first write didn't quietly leave the key unwritten.
        seed_intent(&store, "cc-02").await;
        let BuildOutcome::Published(snapshot) = build(&store, "oxidant").await.unwrap() else {
            panic!("expected the second publish to succeed too");
        };
        assert_eq!(snapshot.agents.len(), 2);
    }
}
