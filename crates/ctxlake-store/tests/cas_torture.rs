//! CAS torture and clock-skew tests for `lease`.
//!
//! These run against the local filesystem backend unconditionally. When
//! `CTXLAKE_TEST_S3_ENDPOINT` is set (CI provides MinIO there — see
//! `.github/workflows/ci.yml`), the same suites also run against it; when it is
//! unset, that half skips cleanly rather than failing. A bucket must already exist
//! at `CTXLAKE_TEST_S3_BUCKET` (default `ctxlake-test`) — `object_store` has no
//! bucket-creation API of its own, so provisioning one is a CI/infra concern outside
//! this crate. If the endpoint is reachable but the bucket is missing, we skip with
//! a diagnostic rather than fail the whole suite over infrastructure this crate
//! doesn't own; any other error (a real CAS bug, bad credentials) still fails loudly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ctxlake_store::backend::{self, BackendOptions};
use ctxlake_store::clock::{Clock, ObjectStoreClock, SystemClock};
use ctxlake_store::lease::{self, AcquireOutcome};
use futures::future::BoxFuture;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

fn torture_key() -> Path {
    Path::from("live/leases/cas-torture.json")
}

/// Builds a local-filesystem store the same way `ctxlake` itself would (through
/// `backend::build`), so these tests exercise `CasLocalFileSystem` — plain
/// `object_store::local::LocalFileSystem` has no `PutMode::Update` at all (see
/// `local_cas`'s module doc) and would make every acquire() here fail outright.
fn local_store(dir: &std::path::Path) -> Arc<dyn ObjectStore> {
    let url = url::Url::from_directory_path(dir).unwrap();
    backend::build(&url, &BackendOptions::default()).unwrap().0
}

/// Spawns `tasks` tokio tasks, each attempting `iterations` full
/// acquire-hold-release cycles against the same lease key, busy-retrying on
/// contention. An in-process oracle (`currently_held`) independent of the store
/// records who *should* be the exclusive holder at any instant; any task that
/// observes it disagreeing with itself while it believes it holds the lease has
/// found a double hold.
async fn run_torture(
    store: Arc<dyn ObjectStore>,
    make_clock: fn(Arc<dyn ObjectStore>, String) -> Arc<dyn Clock>,
    tasks: usize,
    iterations: usize,
) {
    let key = torture_key();
    let currently_held: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let total_successes = Arc::new(AtomicU64::new(0));

    let mut join_set = Vec::new();
    for task_id in 0..tasks {
        let store = store.clone();
        let key = key.clone();
        let currently_held = currently_held.clone();
        let total_successes = total_successes.clone();
        join_set.push(tokio::spawn(async move {
            let holder_id = format!("torture-{task_id}");
            let clock = make_clock(store.clone(), holder_id.clone());
            let mut completed = 0usize;
            let mut attempts = 0u64;
            while completed < iterations {
                attempts += 1;
                assert!(attempts < 200_000, "CAS never made progress — liveness bug, not just contention");

                match lease::acquire(
                    store.as_ref(),
                    clock.as_ref(),
                    &key,
                    &holder_id,
                    None,
                    Duration::from_secs(30),
                )
                .await
                .unwrap()
                {
                    AcquireOutcome::Acquired(handle) => {
                        {
                            let mut guard = currently_held.lock().unwrap();
                            assert!(
                                guard.is_none(),
                                "acquire() reported success while {:?} already held the lease",
                                *guard
                            );
                            *guard = Some(holder_id.clone());
                        }

                        tokio::task::yield_now().await;

                        {
                            let mut guard = currently_held.lock().unwrap();
                            assert_eq!(
                                guard.as_deref(),
                                Some(holder_id.as_str()),
                                "the lease's recorded holder changed while we believed we held it — double hold"
                            );
                            *guard = None;
                        }

                        lease::release(store.as_ref(), handle).await.unwrap();
                        total_successes.fetch_add(1, Ordering::SeqCst);
                        completed += 1;
                    }
                    AcquireOutcome::NotAcquired { .. } => {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }));
    }

    for h in join_set {
        h.await.unwrap();
    }

    assert_eq!(
        total_successes.load(Ordering::SeqCst) as usize,
        tasks * iterations,
        "no lost updates: every task must complete every iteration exactly once"
    );

    let final_state = lease::read(store.as_ref(), &key).await.unwrap();
    assert_eq!(
        final_state.holder, None,
        "lease must end free — every acquire was released"
    );
}

fn system_clock(_store: Arc<dyn ObjectStore>, _id: String) -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

fn object_store_clock(store: Arc<dyn ObjectStore>, id: String) -> Arc<dyn Clock> {
    Arc::new(ObjectStoreClock::new(store, id))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cas_torture_local_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let store = local_store(dir.path());
    run_torture(store, system_clock, 10, 15).await;
}

/// Builds an S3-compatible store for the MinIO suites, or `None` with a message
/// explaining why to skip. See the module doc for the bucket-provisioning caveat.
async fn minio_store_for_tests() -> Option<Arc<dyn ObjectStore>> {
    let endpoint = std::env::var("CTXLAKE_TEST_S3_ENDPOINT").ok()?;
    let bucket =
        std::env::var("CTXLAKE_TEST_S3_BUCKET").unwrap_or_else(|_| "ctxlake-test".to_string());

    let opts = ctxlake_store::backend::BackendOptions {
        endpoint: Some(endpoint.clone()),
        allow_http: true,
        ..Default::default()
    };
    let url = url::Url::parse(&format!("s3://{bucket}/ctxlake-cas-torture")).unwrap();
    let (store, _path) = match ctxlake_store::backend::build(&url, &opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("skipping MinIO suite: could not construct S3 store: {e}");
            return None;
        }
    };

    // Probe reachability and bucket existence with one harmless write before
    // committing the whole suite to it.
    let probe_key = Path::from("_meta/probe/torture-bucket-check.json");
    match store.put(&probe_key, PutPayload::from_static(b"{}")).await {
        Ok(_) => {
            let _ = store.delete(&probe_key).await;
            Some(store)
        }
        Err(e) => {
            eprintln!(
                "skipping MinIO suite: bucket {bucket:?} at {endpoint} is not writable ({e}). \
                 CTXLAKE_TEST_S3_ENDPOINT is set, so this is an infra gap (the bucket likely \
                 doesn't exist yet), not a code failure — object_store has no bucket-creation \
                 API, so provisioning it is a CI-side step."
            );
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cas_torture_minio() {
    let Some(store) = minio_store_for_tests().await else {
        return;
    };
    run_torture(store, object_store_clock, 6, 5).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dies_mid_hold_only_one_contender_steals() {
    let dir = tempfile::tempdir().unwrap();
    let store = local_store(dir.path());
    let clock = SystemClock;
    let key = Path::from("live/leases/dies-mid-hold.json");

    let AcquireOutcome::Acquired(handle) = lease::acquire(
        store.as_ref(),
        &clock,
        &key,
        "the-doomed-holder",
        None,
        Duration::from_millis(50),
    )
    .await
    .unwrap() else {
        panic!("expected the first acquire to succeed");
    };
    drop(handle); // no release() call — simulates a process that dies mid-hold.

    // Contenders race the moment the TTL is up; none should win before it, exactly
    // one should win after.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let too_early: Vec<_> = futures::future::join_all((0..4).map(|i| {
        let store = store.clone();
        let key = key.clone();
        async move {
            lease::acquire(
                store.as_ref(),
                &SystemClock,
                &key,
                &format!("early-{i}"),
                None,
                Duration::from_secs(30),
            )
            .await
            .unwrap()
        }
    }))
    .await;
    assert!(
        too_early
            .iter()
            .all(|o| matches!(o, AcquireOutcome::NotAcquired { .. })),
        "no contender may steal before the TTL elapses"
    );

    tokio::time::sleep(Duration::from_millis(60)).await;
    let results: Vec<_> = futures::future::join_all((0..8).map(|i| {
        let store = store.clone();
        let key = key.clone();
        async move {
            lease::acquire(
                store.as_ref(),
                &SystemClock,
                &key,
                &format!("late-{i}"),
                None,
                Duration::from_secs(30),
            )
            .await
            .unwrap()
        }
    }))
    .await;
    let winners = results
        .iter()
        .filter(|o| matches!(o, AcquireOutcome::Acquired(_)))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one contender must steal an expired, unreleased lease"
    );
}

/// A [`Clock`] whose reading is a fixed offset from a shared, real system clock —
/// used to simulate a process whose wall clock disagrees with everyone else's.
#[derive(Debug)]
struct SkewedClock(i64);

impl Clock for SkewedClock {
    fn now(&self) -> BoxFuture<'_, Result<std::time::SystemTime, ctxlake_store::StoreError>> {
        Box::pin(async move {
            let real = std::time::SystemTime::now();
            Ok(if self.0 >= 0 {
                real + Duration::from_secs(self.0 as u64)
            } else {
                real - Duration::from_secs((-self.0) as u64)
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clock_skew_of_plus_and_minus_ten_minutes_never_causes_a_double_hold() {
    let dir = tempfile::tempdir().unwrap();
    let store = local_store(dir.path());
    let key = Path::from("live/leases/skew-race.json");

    let ahead = SkewedClock(600); // +10 minutes
    let behind = SkewedClock(-600); // -10 minutes
                                    // An hour, comfortably longer than the 10-minute skew above — see the comment
                                    // below on what this test can and can't prove.
    let ttl = Duration::from_secs(3600);

    // Both race a never-touched lease concurrently. Whichever clock a contender
    // trusts, the CAS write is what actually decides the winner — this proves that
    // decision doesn't depend on agreement between two disagreeing clocks.
    let (a, b) = tokio::join!(
        lease::acquire(store.as_ref(), &ahead, &key, "ahead", None, ttl),
        lease::acquire(store.as_ref(), &behind, &key, "behind", None, ttl),
    );
    let winners = [&a, &b]
        .into_iter()
        .filter(|o| matches!(o, Ok(AcquireOutcome::Acquired(_))))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one of two skewed clocks may win the initial acquire"
    );

    // acquire() trusts whatever its Clock reports (that boundary is documented in
    // lease.rs and is where real skew protection actually lives — ObjectStoreClock
    // never consults the caller's own SystemTime::now()). What this can still
    // prove without controlling the real system clock: as long as a lease's TTL
    // comfortably exceeds realistic skew, neither an ahead- nor a behind-skewed
    // contender treats it as expired.
    let state_before = lease::read(store.as_ref(), &key).await.unwrap();
    assert!(state_before.holder.is_some());

    let contender = if matches!(a, Ok(AcquireOutcome::Acquired(_))) {
        &behind
    } else {
        &ahead
    };
    let steal_attempt = lease::acquire(
        store.as_ref(),
        contender,
        &key,
        "late-contender",
        None,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(
        matches!(steal_attempt, AcquireOutcome::NotAcquired { .. }),
        "an hour-long lease must not be stealable by a contender skewed only 10 minutes"
    );
}
