//! Presence: publish this agent's own `live/agents/<id>.json` intent, and
//! opportunistically hold `live/leases/_maintenance` to build `live/roster.json` —
//! the O(N) fan-in `ctxlake_store::roster`'s module doc explains the cost of
//! *not* having — when nobody else currently does.
//!
//! ## "Renew at 60s with a 5-minute TTL" for a primitive with no TTL field
//!
//! `ctxlake_store::intent` is a plain single-writer overwrite with no expiry of its
//! own (see that module's doc: there is exactly one writer for this key, so CAS —
//! and the TTL bookkeeping CAS-based leases need — buys nothing). The 60s/5min
//! cadence named in the task brief is the *lease* renewal ratio from
//! `docs/architecture.md`'s knobs table (renew at 1/5 of TTL, so one missed cycle
//! doesn't expire anything), reused here for the one lease this module actually
//! does hold — [`layout::lease_maintenance`] — and applied *by convention* to the
//! intent republish too: a consumer of the roster treats an agent whose
//! `updated_at` hasn't moved in 5 minutes as gone, the same reasoning as a lapsed
//! lease, even though nothing enforces that expiry server-side for `live/agents/`.
//! Both cadences share one poll interval here because there is no reason to wake
//! twice as often for one over the other.
//!
//! ## Graceful shutdown
//!
//! [`Presence::shutdown`] releases the maintenance lease if held. An un-released
//! lease is not incorrect — it is advisory (AGENTS.md invariant 5) and simply
//! expires on its own TTL — but releasing it promptly lets another agent pick up
//! roster maintenance immediately instead of waiting out however much of the TTL
//! is left, which matters more the smaller a fleet is.

use std::time::Duration;

use ctxlake_core::Runtime;
use object_store::ObjectStore;
use time::OffsetDateTime;

use ctxlake_store::clock::Clock;
use ctxlake_store::intent::{self, Intent};
use ctxlake_store::lease::{self, AcquireOutcome, LeaseHandle};
use ctxlake_store::{layout, roster, StoreError};

/// The lease TTL this daemon requests when it holds `_maintenance` — matches
/// `docs/architecture.md`'s documented default. Renewed every `poll_interval` the
/// caller passes to [`Presence::tick`], which should be 1/5 of this (60s here) for
/// the same missed-cycle safety margin the docs describe.
pub const MAINTENANCE_LEASE_TTL: Duration = Duration::from_secs(300);

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
    let existing = intent::read(store, &cfg.agent_id).await?;
    intent::write(store, &intent_snapshot(cfg, existing.as_ref())).await
}

/// Tracks whether this process currently holds the maintenance lease, across
/// calls to [`Presence::tick`].
#[derive(Debug, Default)]
pub struct Presence {
    held: Option<LeaseHandle>,
}

impl Presence {
    pub fn new() -> Self {
        Self { held: None }
    }

    pub fn holds_maintenance_lease(&self) -> bool {
        self.held.is_some()
    }

    /// One cycle: publish intent, then try to acquire (or renew) the maintenance
    /// lease, and build the roster iff this call ends up holding it.
    ///
    /// [`StoreError::LeaseNotProvisioned`] — the lease key has never been
    /// materialized, because `ctxlake init` (a different wave's command) hasn't run
    /// against this bucket yet — is swallowed rather than propagated as a tick
    /// failure: it is an expected, self-healing state (the *next* fleet member to
    /// run `ctxlake init` fixes it for everyone), not evidence the store is having
    /// trouble, so it must not drive [`crate::backoff`] the way a real store fault
    /// should.
    pub async fn tick(
        &mut self,
        store: &dyn ObjectStore,
        clock: &dyn Clock,
        cfg: &PresenceConfig,
    ) -> Result<(), StoreError> {
        publish_intent(store, cfg).await?;

        match self.try_hold_maintenance(store, clock, cfg).await {
            Ok(()) => {}
            Err(StoreError::LeaseNotProvisioned(_)) => {
                tracing::debug!(
                    "maintenance lease not provisioned yet; skipping roster build this cycle"
                );
            }
            Err(e) => return Err(e),
        }

        if self.held.is_some() {
            match roster::build(store).await? {
                roster::BuildOutcome::Published(_) => {}
                roster::BuildOutcome::Skipped => {
                    // Another builder's snapshot from the same instant landed
                    // first — see `roster::build`'s doc. Nothing lost.
                }
            }
        }
        Ok(())
    }

    async fn try_hold_maintenance(
        &mut self,
        store: &dyn ObjectStore,
        clock: &dyn Clock,
        cfg: &PresenceConfig,
    ) -> Result<(), StoreError> {
        if let Some(handle) = self.held.as_mut() {
            if lease::renew(store, clock, handle, MAINTENANCE_LEASE_TTL).await? {
                return Ok(());
            }
            // Renew reported false: someone else's steal already landed (see
            // `lease::renew`'s doc) — we no longer hold it, full stop.
            self.held = None;
        }

        match lease::acquire(
            store,
            clock,
            &layout::lease_maintenance(),
            &cfg.agent_id,
            Some("roster maintenance"),
            MAINTENANCE_LEASE_TTL,
        )
        .await?
        {
            AcquireOutcome::Acquired(handle) => {
                self.held = Some(handle);
            }
            AcquireOutcome::NotAcquired { .. } => {
                // Normal contention (AGENTS.md invariant 4) — someone else holds
                // it and hasn't lapsed. Nothing to do this cycle.
            }
        }
        Ok(())
    }

    /// Release the maintenance lease if held. See the module doc for why this is a
    /// courtesy, not a correctness requirement — the lease is advisory and expires
    /// on its own regardless.
    pub async fn shutdown(&mut self, store: &dyn ObjectStore) -> Result<(), StoreError> {
        if let Some(handle) = self.held.take() {
            lease::release(store, handle).await?;
        }
        Ok(())
    }
}

/// Run the presence loop until `shutdown` fires, releasing the maintenance lease
/// (if held) on the way out.
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

    if let Err(e) = presence.shutdown(store.as_ref()).await {
        tracing::warn!(error = %e, "failed to release maintenance lease on shutdown");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_store::clock::SystemClock;
    use object_store::memory::InMemory;
    use object_store::ObjectStoreExt;

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

    #[tokio::test]
    async fn publish_intent_writes_a_readable_intent() {
        let store = InMemory::new();
        publish_intent(&store, &cfg()).await.unwrap();
        let back = intent::read(&store, "cc-01").await.unwrap().unwrap();
        assert_eq!(back.agent_id, "cc-01");
        assert_eq!(back.branch.as_deref(), Some("wave2/sync"));
    }

    #[tokio::test]
    async fn a_heartbeat_never_clears_a_richer_intent_something_else_already_wrote() {
        // Regression: `intent_snapshot` used to hardcode `session_id: None, task:
        // None, paths: Vec::new()` on every call. Because `intent::write` is a
        // whole-object `PutMode::Overwrite`, omitting a field IS clearing it (the
        // `skip_serializing_if` attributes drop it from the JSON either way) — so
        // this daemon's own 60s "I am alive" republish erased a richer intent
        // (session_id, task, paths) that any session-aware writer had just set,
        // exactly contradicting this module's own doc comment.
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

        let back = intent::read(&store, "cc-01").await.unwrap().unwrap();
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
    async fn tick_without_provisioning_the_lease_still_publishes_intent_and_does_not_error() {
        let store = InMemory::new();
        let clock = SystemClock;
        let mut presence = Presence::new();
        // No `lease::provision` call — mirrors a fleet where `ctxlake init` hasn't
        // run yet against this bucket.
        presence.tick(&store, &clock, &cfg()).await.unwrap();

        assert!(intent::read(&store, "cc-01").await.unwrap().is_some());
        assert!(!presence.holds_maintenance_lease());
    }

    #[tokio::test]
    async fn tick_acquires_the_lease_and_builds_the_roster_when_provisioned() {
        let store = InMemory::new();
        let clock = SystemClock;
        lease::provision(&store, &layout::lease_maintenance())
            .await
            .unwrap();
        let mut presence = Presence::new();

        presence.tick(&store, &clock, &cfg()).await.unwrap();

        assert!(presence.holds_maintenance_lease());
        let roster_obj = store.get(&layout::roster()).await;
        assert!(roster_obj.is_ok(), "roster.json should have been built");
    }

    #[tokio::test]
    async fn a_second_agent_does_not_steal_a_freshly_held_lease() {
        let store = InMemory::new();
        let clock = SystemClock;
        lease::provision(&store, &layout::lease_maintenance())
            .await
            .unwrap();

        let mut a = Presence::new();
        a.tick(&store, &clock, &cfg()).await.unwrap();
        assert!(a.holds_maintenance_lease());

        let mut b = Presence::new();
        let cfg_b = PresenceConfig {
            agent_id: "cc-02".into(),
            ..cfg()
        };
        b.tick(&store, &clock, &cfg_b).await.unwrap();
        assert!(
            !b.holds_maintenance_lease(),
            "a live holder must not be stolen from"
        );
    }

    #[tokio::test]
    async fn shutdown_releases_a_held_lease_so_another_agent_can_take_over_immediately() {
        let store = InMemory::new();
        let clock = SystemClock;
        lease::provision(&store, &layout::lease_maintenance())
            .await
            .unwrap();

        let mut a = Presence::new();
        a.tick(&store, &clock, &cfg()).await.unwrap();
        assert!(a.holds_maintenance_lease());
        a.shutdown(&store).await.unwrap();

        let state = lease::read(&store, &layout::lease_maintenance())
            .await
            .unwrap();
        assert_eq!(
            state.holder, None,
            "a graceful shutdown must free the lease, not merely let it lapse"
        );

        let mut b = Presence::new();
        let cfg_b = PresenceConfig {
            agent_id: "cc-02".into(),
            ..cfg()
        };
        b.tick(&store, &clock, &cfg_b).await.unwrap();
        assert!(
            b.holds_maintenance_lease(),
            "a freed lease must be immediately acquirable"
        );
    }

    #[tokio::test]
    async fn shutdown_without_ever_holding_the_lease_is_a_no_op() {
        let store = InMemory::new();
        let mut presence = Presence::new();
        presence.shutdown(&store).await.unwrap();
    }
}
