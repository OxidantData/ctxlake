//! Presence: publish this agent's own `live/agents/<id>.json` intent, and
//! periodically build `live/roster.json` — the O(N) fan-in
//! `ctxlake_store::roster`'s module doc explains the cost of *not* having.
//!
//! ## Several daemons building the roster is fine, not a bug
//!
//! An earlier version of this module opportunistically held a fleet-wide
//! maintenance lease so that exactly one daemon built the roster at a time. That
//! lease is gone (see `ctxlake_store`'s crate doc: leases were belt-and-braces on
//! top of work that was already safe). Without it, **every daemon may build the
//! roster**, and that is fine: [`ctxlake_store::roster::build`] publishes with a
//! CAS write, so two daemons racing to publish produce at most one landed write
//! and a harmless `Skipped` for the loser — `roster.json` is fully recomputed
//! from a fresh listing each time, so there is nothing a "losing" build
//! contributed that the winner's doesn't already contain, and a reader following
//! the pointer never observes a torn write either way. If this read like a bug at
//! first — "wait, several hosts write this?" — that CAS write is the whole
//! answer; see `ctxlake_store::roster`'s own module doc for the mechanism.
//!
//! What a lock bought that a CAS write doesn't: avoiding *redundant* O(N) list
//! work when N daemons all rebuild every cycle. [`ROSTER_BUILD_INTERVAL`] is the
//! cheap answer to that — each daemon only attempts a rebuild once every interval
//! (staggered per agent, via [`crate::backoff::XorShift`], so a fleet that started
//! up all at once doesn't converge on rebuilding in lockstep forever) rather than
//! on every presence tick. This is a rate limit on redundant work, not a
//! correctness mechanism — do not read "staggered" as "coordinated": nothing
//! prevents two daemons' independently-chosen schedules from landing in the same
//! cycle, and nothing needs to.
//!
//! ## Publishing this agent's own intent
//!
//! `ctxlake_store::intent` is a plain single-writer overwrite with no expiry of
//! its own (see that module's doc: there is exactly one writer for this key, so
//! CAS buys nothing). A consumer of the roster treats an agent whose `updated_at`
//! hasn't moved in a while as gone, the same reasoning `docs/architecture.md`'s
//! knob table describes for the intent republish cadence, even though nothing
//! enforces that expiry server-side for `live/agents/`.

use std::time::Duration;

use ctxlake_core::Runtime;
use object_store::ObjectStore;
use time::OffsetDateTime;

use ctxlake_store::clock::Clock;
use ctxlake_store::intent::{self, Intent};
use ctxlake_store::{roster, StoreError};

use crate::backoff::{JitterSource, XorShift};

/// The base interval between one daemon's own roster rebuilds — deliberately not
/// the same as the presence tick interval (`docs/architecture.md`'s knob table),
/// or every daemon would redo the full O(N) listing every single tick, which is
/// exactly the request-volume problem `ctxlake_store::roster` exists to avoid. The
/// actual interval used is this plus a per-agent random jitter up to the same
/// amount again — see [`Presence::next_roster_build_at`].
pub const ROSTER_BUILD_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct PresenceConfig {
    pub fleet_id: String,
    pub agent_id: String,
    pub runtime: Runtime,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub cwd: Option<String>,
}

/// Build the intent this heartbeat wants to publish, carrying forward whatever
/// session-level detail `existing` already held.
///
/// Per-session task/path detail is written by whatever is actually running the
/// session (a later wave's job — see the module doc); this daemon's own republish
/// is the coarse "I am alive" heartbeat, not a task update, so it must never
/// *clear* a richer intent something else just wrote. Because `intent::write` is a
/// whole-object overwrite (that module's own doc: no CAS, exactly one *kind* of
/// writer for this key, so no read-modify-*write* was ever needed to avoid a race)
/// — but "no race to avoid" is not "no read to do first": omitting a field from
/// this struct is indistinguishable, on the wire, from actively clearing it
/// (`#[serde(skip_serializing_if)]` drops it from the JSON either way). So this
/// reads the previous object and folds its `session_id`/`task`/`paths` forward
/// unconditionally, and only ever refreshes the fields this heartbeat actually
/// owns: `repo`/`branch`/`cwd` (which track *this process*, not the session) and
/// `updated_at`.
fn intent_snapshot(cfg: &PresenceConfig, existing: Option<&Intent>) -> Intent {
    Intent {
        agent_id: cfg.agent_id.clone(),
        fleet_id: cfg.fleet_id.clone(),
        runtime: cfg.runtime,
        session_id: existing.and_then(|i| i.session_id.clone()),
        repo: cfg.repo.clone(),
        branch: cfg.branch.clone(),
        cwd: cfg.cwd.clone(),
        task: existing.and_then(|i| i.task.clone()),
        paths: existing.map(|i| i.paths.clone()).unwrap_or_default(),
        updated_at: OffsetDateTime::now_utc(),
    }
}

/// Publish this agent's own intent once: read whatever is already there (so a
/// richer intent's `session_id`/`task`/`paths` survive this heartbeat — see
/// [`intent_snapshot`]), then overwrite. Not CAS — `intent.rs`'s module doc
/// explains why this key never needs it — so the write itself is still cheap and
/// always safe; only the read is new cost, once per heartbeat interval, not once
/// per hook.
pub async fn publish_intent(
    store: &dyn ObjectStore,
    cfg: &PresenceConfig,
) -> Result<(), StoreError> {
    let existing = intent::read(store, &cfg.fleet_id, &cfg.agent_id).await?;
    intent::write(store, &intent_snapshot(cfg, existing.as_ref())).await
}

/// One daemon's presence loop state: when it last decided to rebuild the roster,
/// and the jitter source that decides when to try again — see the module doc for
/// why this is a rate limit on redundant work, not a lock.
#[derive(Debug)]
pub struct Presence<J: JitterSource = XorShift> {
    next_roster_build_at: Option<OffsetDateTime>,
    jitter: J,
}

impl Default for Presence<XorShift> {
    fn default() -> Self {
        Self::new()
    }
}

impl Presence<XorShift> {
    pub fn new() -> Self {
        Self::with_jitter(XorShift::seeded())
    }
}

impl<J: JitterSource> Presence<J> {
    pub fn with_jitter(jitter: J) -> Self {
        Self {
            next_roster_build_at: None,
            jitter,
        }
    }

    /// One cycle: publish intent, and rebuild the roster iff this daemon's own
    /// schedule (see the module doc) says it's due — never gated on anyone else's
    /// state, because there is no "anyone else's state" left to gate on.
    pub async fn tick(
        &mut self,
        store: &dyn ObjectStore,
        clock: &dyn Clock,
        cfg: &PresenceConfig,
    ) -> Result<(), StoreError> {
        publish_intent(store, cfg).await?;

        let now = OffsetDateTime::from(clock.now().await?);
        if self.next_roster_build_at.is_none_or(|due| now >= due) {
            match roster::build(store, &cfg.fleet_id).await? {
                roster::BuildOutcome::Published(_) => {}
                roster::BuildOutcome::Skipped => {
                    // A concurrent builder's snapshot from the same instant landed
                    // first — see `roster::build`'s doc. Nothing lost.
                }
            }
            self.next_roster_build_at = Some(now + self.jittered_interval());
        }
        Ok(())
    }

    /// `ROSTER_BUILD_INTERVAL` plus a random extra amount up to the same interval
    /// again, so this daemon's cadence doesn't stay locked in phase with every
    /// other daemon that happened to start at the same moment.
    fn jittered_interval(&mut self) -> Duration {
        ROSTER_BUILD_INTERVAL + self.jitter.uniform_up_to(ROSTER_BUILD_INTERVAL)
    }
}

/// Run the presence loop until `shutdown` fires.
pub async fn run(
    store: std::sync::Arc<dyn ObjectStore>,
    clock: std::sync::Arc<dyn Clock>,
    cfg: PresenceConfig,
    poll_interval: Duration,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut presence = Presence::new();
    let mut backoff =
        crate::backoff::Backoff::new(Duration::from_millis(200), Duration::from_secs(30));
    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        let delay = match presence.tick(store.as_ref(), clock.as_ref(), &cfg).await {
            Ok(()) => {
                backoff.reset();
                poll_interval
            }
            Err(e) => {
                tracing::warn!(error = %e, "presence tick failed");
                if crate::backoff::should_backoff(&e) {
                    backoff.next_delay()
                } else {
                    poll_interval
                }
            }
        };

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_store::clock::SystemClock;
    use futures::future::BoxFuture;
    use object_store::memory::InMemory;
    use object_store::ObjectStoreExt;
    use std::sync::Mutex;

    fn cfg() -> PresenceConfig {
        PresenceConfig {
            fleet_id: "oxidant".into(),
            agent_id: "cc-01".into(),
            runtime: Runtime::ClaudeCode,
            repo: Some("github.com/OxidantData/ctxlake".into()),
            branch: Some("wave2/sync".into()),
            cwd: None,
        }
    }

    /// A clock the test advances by hand, so the roster-build backoff can be
    /// asserted exactly instead of racing real wall-clock time.
    #[derive(Debug)]
    struct ManualClock(Mutex<std::time::SystemTime>);
    impl ManualClock {
        fn new() -> Self {
            Self(Mutex::new(std::time::SystemTime::now()))
        }
        fn advance(&self, d: Duration) {
            *self.0.lock().unwrap() += d;
        }
    }
    impl Clock for ManualClock {
        fn now(&self) -> BoxFuture<'_, Result<std::time::SystemTime, StoreError>> {
            Box::pin(async move { Ok(*self.0.lock().unwrap()) })
        }
    }

    /// A jitter source that always returns zero, so a test can assert the
    /// schedule at the exact base interval rather than "somewhere in a range."
    #[derive(Debug)]
    struct NoJitter;
    impl JitterSource for NoJitter {
        fn uniform_up_to(&mut self, _cap: Duration) -> Duration {
            Duration::ZERO
        }
    }

    #[tokio::test]
    async fn publish_intent_writes_a_readable_intent() {
        let store = InMemory::new();
        publish_intent(&store, &cfg()).await.unwrap();
        let back = intent::read(&store, "oxidant", "cc-01")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back.agent_id, "cc-01");
        assert_eq!(back.branch.as_deref(), Some("wave2/sync"));
    }

    #[tokio::test]
    async fn a_heartbeat_never_clears_a_richer_intent_something_else_already_wrote() {
        // Regression: `intent_snapshot` used to hardcode `session_id: None, task:
        // None, paths: Vec::new()` on every call. Because `intent::write` is a
        // whole-object `PutMode::Overwrite`, omitting a field IS clearing it (the
        // `skip_serializing_if` attributes drop it from the JSON either way) — so
        // this daemon's own heartbeat republish erased a richer intent (session_id,
        // task, paths) that any session-aware writer had just set, exactly
        // contradicting this module's own doc comment.
        let store = InMemory::new();
        let rich = Intent {
            agent_id: "cc-01".into(),
            fleet_id: "oxidant".into(),
            runtime: Runtime::ClaudeCode,
            session_id: Some("sess-42".into()),
            repo: Some("github.com/OxidantData/ctxlake".into()),
            branch: Some("wave2/sync".into()),
            cwd: None,
            task: Some("implementing the upload loop".into()),
            paths: vec!["crates/ctxlake-sync/src/upload.rs".into()],
            updated_at: OffsetDateTime::now_utc(),
        };
        intent::write(&store, &rich).await.unwrap();

        let mut presence = Presence::new();
        presence.tick(&store, &SystemClock, &cfg()).await.unwrap();

        let back = intent::read(&store, "oxidant", "cc-01")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            back.session_id.as_deref(),
            Some("sess-42"),
            "a heartbeat tick must not clear session_id"
        );
        assert_eq!(
            back.task.as_deref(),
            Some("implementing the upload loop"),
            "a heartbeat tick must not clear task"
        );
        assert_eq!(
            back.paths,
            vec!["crates/ctxlake-sync/src/upload.rs".to_string()],
            "a heartbeat tick must not clear paths"
        );
    }

    #[tokio::test]
    async fn the_first_tick_builds_the_roster_immediately() {
        // No lease to acquire any more — a brand-new daemon's very first tick
        // should still get a roster published promptly, not wait out a full
        // interval before anyone answers "who else is here."
        let store = InMemory::new();
        let mut presence = Presence::new();

        presence.tick(&store, &SystemClock, &cfg()).await.unwrap();

        assert!(
            store
                .get(&ctxlake_store::layout::roster("oxidant"))
                .await
                .is_ok(),
            "roster.json should have been built on the very first tick"
        );
    }

    #[tokio::test]
    async fn a_second_agent_also_builds_the_roster_independently() {
        // The property that replaced the lock: several daemons ticking is fine,
        // not a conflict — neither is refused, neither errors.
        let store = InMemory::new();
        let mut a = Presence::new();
        let mut b = Presence::new();

        a.tick(&store, &SystemClock, &cfg()).await.unwrap();
        let cfg_b = PresenceConfig {
            agent_id: "cc-02".into(),
            ..cfg()
        };
        b.tick(&store, &SystemClock, &cfg_b).await.unwrap();

        assert!(intent::read(&store, "oxidant", "cc-01")
            .await
            .unwrap()
            .is_some());
        assert!(intent::read(&store, "oxidant", "cc-02")
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get(&ctxlake_store::layout::roster("oxidant"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_tick_before_the_interval_elapses_does_not_rebuild_the_roster() {
        // The backoff that replaced the lock's request-volume savings: a second
        // tick shortly after the first must not pay the O(N) listing cost again,
        // observed the only way available from outside — a roster rebuilt in
        // between would have picked up cc-02's intent, so its *absence* proves no
        // rebuild happened.
        let store = InMemory::new();
        let clock = ManualClock::new();
        let mut presence = Presence::with_jitter(NoJitter);

        presence.tick(&store, &clock, &cfg()).await.unwrap();
        clock.advance(ROSTER_BUILD_INTERVAL / 2);

        // A second agent's intent lands directly (not through a tick of its own),
        // simulating something changing in the fleet between this daemon's builds.
        let cfg_b = PresenceConfig {
            agent_id: "cc-02".into(),
            ..cfg()
        };
        publish_intent(&store, &cfg_b).await.unwrap();

        presence.tick(&store, &clock, &cfg()).await.unwrap();

        let roster = store
            .get(&ctxlake_store::layout::roster("oxidant"))
            .await
            .unwrap();
        let snapshot: roster::RosterSnapshot =
            serde_json::from_slice(&roster.bytes().await.unwrap()).unwrap();
        assert_eq!(
            snapshot.agents.len(),
            1,
            "a tick inside the backoff window must not have rebuilt the roster"
        );
    }

    #[tokio::test]
    async fn a_tick_after_the_interval_elapses_rebuilds_the_roster() {
        let store = InMemory::new();
        let clock = ManualClock::new();
        let mut presence = Presence::with_jitter(NoJitter);

        presence.tick(&store, &clock, &cfg()).await.unwrap();

        let cfg_b = PresenceConfig {
            agent_id: "cc-02".into(),
            ..cfg()
        };
        publish_intent(&store, &cfg_b).await.unwrap();

        clock.advance(ROSTER_BUILD_INTERVAL + Duration::from_secs(1));
        presence.tick(&store, &clock, &cfg()).await.unwrap();

        let roster = store
            .get(&ctxlake_store::layout::roster("oxidant"))
            .await
            .unwrap();
        let snapshot: roster::RosterSnapshot =
            serde_json::from_slice(&roster.bytes().await.unwrap()).unwrap();
        let mut ids: Vec<_> = snapshot.agents.iter().map(|a| a.agent_id.clone()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["cc-01".to_string(), "cc-02".to_string()],
            "a tick past the interval must have rebuilt the roster and picked up cc-02"
        );
    }
}
