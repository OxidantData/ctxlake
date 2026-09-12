//! Ties the three loops together: [`crate::upload`], [`crate::cache`], and
//! [`crate::presence`], each on its own `tokio` task, sharing one shutdown signal.
//!
//! There is deliberately no cross-loop coordination beyond that signal — the three
//! loops touch disjoint keys (`sessions/` vs. `live/` vs. reading `snapshot/` and
//! `live/roster.json`) by the one-write-pattern-per-plane rule (AGENTS.md invariant
//! 3), so nothing about running them concurrently needs a lock between them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ctxlake_core::Runtime;
use object_store::ObjectStore;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use ctxlake_store::clock::Clock;

use crate::cache::CacheConfig;
use crate::presence::PresenceConfig;
use crate::upload::UploadConfig;

/// Everything the daemon needs to start; see `docs/architecture.md`'s "Every knob"
/// table for where the interval defaults below come from.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub fleet_id: String,
    pub agent_id: String,
    pub runtime: Runtime,
    pub spool_root: PathBuf,
    pub cache_root: PathBuf,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub cwd: Option<String>,
    /// Spool flush/batch trigger — `docs/architecture.md`'s knob table gives 2s as
    /// an example default.
    pub upload_poll_interval: Duration,
    /// Cache refresh interval — the docs' 5-15s staleness floor.
    pub cache_poll_interval: Duration,
    /// Intent republish / lease renewal cadence — 60s, 1/5 of
    /// [`crate::presence::MAINTENANCE_LEASE_TTL`].
    pub presence_poll_interval: Duration,
}

impl DaemonConfig {
    /// The documented defaults, for anything that only needs to override identity
    /// and paths.
    pub fn with_defaults(
        fleet_id: impl Into<String>,
        agent_id: impl Into<String>,
        runtime: Runtime,
        spool_root: PathBuf,
        cache_root: PathBuf,
    ) -> Self {
        Self {
            fleet_id: fleet_id.into(),
            agent_id: agent_id.into(),
            runtime,
            spool_root,
            cache_root,
            repo: None,
            branch: None,
            cwd: None,
            upload_poll_interval: Duration::from_secs(2),
            cache_poll_interval: Duration::from_secs(10),
            presence_poll_interval: Duration::from_secs(60),
        }
    }
}

/// A running daemon: three background tasks and the shutdown signal that stops
/// them.
pub struct Daemon {
    shutdown_tx: watch::Sender<bool>,
    handles: Vec<JoinHandle<()>>,
}

impl Daemon {
    /// Start all three loops. Must be called from inside a `tokio` runtime.
    pub fn spawn(store: Arc<dyn ObjectStore>, clock: Arc<dyn Clock>, cfg: DaemonConfig) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let upload_cfg = UploadConfig {
            fleet_id: cfg.fleet_id.clone(),
            agent_id: cfg.agent_id.clone(),
            spool_root: cfg.spool_root.clone(),
        };
        let cache_cfg = CacheConfig {
            fleet_id: cfg.fleet_id.clone(),
            cache_root: cfg.cache_root.clone(),
        };
        let presence_cfg = PresenceConfig {
            fleet_id: cfg.fleet_id.clone(),
            agent_id: cfg.agent_id.clone(),
            runtime: cfg.runtime,
            repo: cfg.repo.clone(),
            branch: cfg.branch.clone(),
            cwd: cfg.cwd.clone(),
        };

        let upload_store = Arc::clone(&store);
        let upload_rx = shutdown_rx.clone();
        let upload_interval = cfg.upload_poll_interval;
        let upload_handle = tokio::spawn(async move {
            crate::upload::run(upload_store, upload_cfg, upload_interval, upload_rx).await;
        });

        let cache_store = Arc::clone(&store);
        let cache_clock = Arc::clone(&clock);
        let cache_rx = shutdown_rx.clone();
        let cache_interval = cfg.cache_poll_interval;
        let cache_handle = tokio::spawn(async move {
            crate::cache::run(
                cache_store,
                cache_clock,
                cache_cfg,
                cache_interval,
                cache_rx,
            )
            .await;
        });

        let presence_store = Arc::clone(&store);
        let presence_clock = Arc::clone(&clock);
        let presence_rx = shutdown_rx.clone();
        let presence_interval = cfg.presence_poll_interval;
        let presence_handle = tokio::spawn(async move {
            crate::presence::run(
                presence_store,
                presence_clock,
                presence_cfg,
                presence_interval,
                presence_rx,
            )
            .await;
        });

        Self {
            shutdown_tx,
            handles: vec![upload_handle, cache_handle, presence_handle],
        }
    }

    /// Signal every loop to stop after its current iteration, release the
    /// maintenance lease if this daemon held it (inside `presence::run`'s own
    /// shutdown tail), and wait for all three tasks to actually finish. Flushing is
    /// implicit: each loop's own watermark/cache writes are already durable the
    /// instant they happen (module docs), so there is no separate buffered state
    /// here that a shutdown needs to flush on the way out.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        for handle in self.handles {
            if let Err(e) = handle.await {
                tracing::warn!(error = %e, "a sync loop task panicked during shutdown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_store::clock::SystemClock;
    use ctxlake_store::{intent, layout};
    use object_store::memory::InMemory;
    use object_store::ObjectStoreExt;
    use std::io::Write as _;

    #[tokio::test]
    async fn starts_runs_a_cycle_and_shuts_down_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let spool_root = dir.path().join("spool");
        let cache_root = dir.path().join("cache");
        std::fs::create_dir_all(spool_root.join("claude_code")).unwrap();
        {
            let mut f = std::fs::File::create(spool_root.join("claude_code").join("sess-1.ndjson"))
                .unwrap();
            let e = ctxlake_core::Envelope::new(
                "oxidant",
                "cc-01",
                Runtime::ClaudeCode,
                "sess-1",
                ctxlake_core::EventType::ToolCall,
                "2026-09-11T18:22:00.000Z",
            );
            writeln!(f, "{}", e.to_ndjson().unwrap()).unwrap();
        }

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let mut cfg = DaemonConfig::with_defaults(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            spool_root,
            cache_root.clone(),
        );
        cfg.upload_poll_interval = Duration::from_millis(20);
        cfg.cache_poll_interval = Duration::from_millis(20);
        cfg.presence_poll_interval = Duration::from_millis(20);

        let daemon = Daemon::spawn(Arc::clone(&store), clock, cfg);
        tokio::time::sleep(Duration::from_millis(200)).await;
        daemon.shutdown().await;

        // The upload loop should have shipped the one event as a segment.
        let seg = layout::session_segment(
            "2026-09-11",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "sess-1",
            0,
        );
        assert!(
            store.get(&seg).await.is_ok(),
            "expected the upload loop to have run at least once"
        );

        // The presence loop should have published this agent's own intent.
        assert!(intent::read(store.as_ref(), "cc-01")
            .await
            .unwrap()
            .is_some());

        // The cache loop should have written a roster cache mirroring that intent.
        let roster_cache = cache_root.join("oxidant").join("roster.json");
        assert!(
            roster_cache.exists(),
            "expected the cache loop to have run at least once"
        );
    }

    #[tokio::test]
    async fn shutdown_releases_the_maintenance_lease_if_this_daemon_held_it() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        ctxlake_store::lease::provision(store.as_ref(), &layout::lease_maintenance())
            .await
            .unwrap();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);

        let mut cfg = DaemonConfig::with_defaults(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            dir.path().join("spool"),
            dir.path().join("cache"),
        );
        cfg.presence_poll_interval = Duration::from_millis(10);
        cfg.upload_poll_interval = Duration::from_millis(50);
        cfg.cache_poll_interval = Duration::from_millis(50);

        let daemon = Daemon::spawn(Arc::clone(&store), clock, cfg);
        tokio::time::sleep(Duration::from_millis(100)).await;
        daemon.shutdown().await;

        let state = ctxlake_store::lease::read(store.as_ref(), &layout::lease_maintenance())
            .await
            .unwrap();
        assert_eq!(
            state.holder, None,
            "a held maintenance lease must be released on graceful shutdown"
        );
    }
}
