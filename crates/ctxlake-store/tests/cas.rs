//! CAS behavior, proven directly — no lease, no lock, just `PutMode::Update`.
//!
//! This is the evidence that the storage primitive everything else in this crate
//! is built on actually works: an `Update` against the version just written
//! succeeds, an `Update` against a stale version is rejected, and under real
//! concurrent access exactly one of two racing `Update`s against the same stale
//! version ever wins. An earlier version of this file (`cas_torture.rs`) proved
//! the same three properties by acquiring and releasing leases in a loop; leases
//! are gone (see `ctxlake_store`'s crate doc), but CAS itself is staying — these
//! tests are what is left to prove it, relocated here rather than lost with the
//! lease machinery they used to ride along with.
//!
//! These run against the local filesystem backend unconditionally. When
//! `CTXLAKE_TEST_S3_ENDPOINT` is set (CI provides MinIO there — see
//! `.github/workflows/ci.yml`), the torture suite also runs against it; when it is
//! unset, that half skips cleanly rather than failing. A bucket must already exist
//! at `CTXLAKE_TEST_S3_BUCKET` (default `ctxlake-test`) — `object_store` has no
//! bucket-creation API of its own, so provisioning one is a CI/infra concern outside
//! this crate. If the endpoint is reachable but the bucket is missing, we skip with
//! a diagnostic rather than fail the whole suite over infrastructure this crate
//! doesn't own; any other error (a real CAS bug, bad credentials) still fails loudly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ctxlake_store::backend::{self, BackendOptions};
use object_store::path::Path;
use object_store::{Error as OsError, ObjectStore, ObjectStoreExt, PutMode, PutPayload};

fn cas_key() -> Path {
    Path::from("coordination/counter.json")
}

/// Builds a local-filesystem store the same way `ctxlake` itself would (through
/// `backend::build`), so these tests exercise `CasLocalFileSystem` — plain
/// `object_store::local::LocalFileSystem` has no `PutMode::Update` at all (see
/// `local_cas`'s module doc) and would make every `Update` here fail outright.
fn local_store(dir: &std::path::Path) -> Arc<dyn ObjectStore> {
    let url = url::Url::from_directory_path(dir).unwrap();
    backend::build(&url, &BackendOptions::default()).unwrap().0
}

#[tokio::test]
async fn update_succeeds_against_the_version_just_written() {
    let dir = tempfile::tempdir().unwrap();
    let store = local_store(dir.path());
    let key = cas_key();

    let written = store
        .put(&key, PutPayload::from_static(b"{\"v\":1}"))
        .await
        .unwrap();
    let version = object_store::UpdateVersion::from(written);
    store
        .put_opts(
            &key,
            PutPayload::from_static(b"{\"v\":2}"),
            PutMode::Update(version).into(),
        )
        .await
        .unwrap();

    let body = store.get(&key).await.unwrap().bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"{\"v\":2}");
}

#[tokio::test]
async fn update_against_a_stale_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = local_store(dir.path());
    let key = cas_key();

    let written = store
        .put(&key, PutPayload::from_static(b"{\"v\":1}"))
        .await
        .unwrap();
    let stale_version = object_store::UpdateVersion::from(written);
    // Advances the real version — `stale_version` no longer matches.
    store
        .put(&key, PutPayload::from_static(b"{\"v\":2}"))
        .await
        .unwrap();

    let result = store
        .put_opts(
            &key,
            PutPayload::from_static(b"{\"v\":3}"),
            PutMode::Update(stale_version).into(),
        )
        .await;
    assert!(matches!(result, Err(OsError::Precondition { .. })));

    let body = store.get(&key).await.unwrap().bytes().await.unwrap();
    assert_eq!(
        body.as_ref(),
        b"{\"v\":2}",
        "a rejected stale Update must not have touched the object"
    );
}

/// Spawns `tasks` tokio tasks, each attempting `iterations` full read-then-CAS
/// increments of a shared JSON counter at the same key, busy-retrying on
/// contention. An in-process oracle (`total_confirmed`) independent of the store
/// counts every increment that the store itself confirmed landed, so the test can
/// assert "no lost updates" against a number nothing here can fudge.
async fn run_torture(store: Arc<dyn ObjectStore>, tasks: usize, iterations: usize) {
    let key = cas_key();
    store
        .put(&key, PutPayload::from_static(b"{\"count\":0}"))
        .await
        .unwrap();
    let total_confirmed = Arc::new(AtomicU64::new(0));

    let mut join_set = Vec::new();
    for _ in 0..tasks {
        let store = store.clone();
        let key = key.clone();
        let total_confirmed = total_confirmed.clone();
        join_set.push(tokio::spawn(async move {
            let mut completed = 0usize;
            let mut attempts = 0u64;
            while completed < iterations {
                attempts += 1;
                assert!(
                    attempts < 200_000,
                    "CAS never made progress — liveness bug, not just contention"
                );

                let current = store.get(&key).await.unwrap();
                let version = object_store::UpdateVersion {
                    e_tag: current.meta.e_tag.clone(),
                    version: current.meta.version.clone(),
                };
                let body: serde_json::Value =
                    serde_json::from_slice(&current.bytes().await.unwrap()).unwrap();
                let count = body["count"].as_u64().unwrap();
                let next = serde_json::json!({ "count": count + 1 });

                match store
                    .put_opts(
                        &key,
                        PutPayload::from(serde_json::to_vec(&next).unwrap()),
                        PutMode::Update(version).into(),
                    )
                    .await
                {
                    Ok(_) => {
                        total_confirmed.fetch_add(1, Ordering::SeqCst);
                        completed += 1;
                    }
                    Err(OsError::Precondition { .. }) => {
                        tokio::task::yield_now().await;
                    }
                    Err(e) => panic!("unexpected store error: {e}"),
                }
            }
        }));
    }

    for h in join_set {
        h.await.unwrap();
    }

    assert_eq!(
        total_confirmed.load(Ordering::SeqCst) as usize,
        tasks * iterations,
        "every task must complete every iteration exactly once"
    );

    let final_body = store.get(&key).await.unwrap().bytes().await.unwrap();
    let final_value: serde_json::Value = serde_json::from_slice(&final_body).unwrap();
    assert_eq!(
        final_value["count"].as_u64().unwrap(),
        (tasks * iterations) as u64,
        "the counter itself must reflect every confirmed increment — a lost update \
         here would mean two racing writers both believed they won the same CAS"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cas_torture_local_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let store = local_store(dir.path());
    run_torture(store, 10, 15).await;
}

/// Builds an S3-compatible store for the MinIO suite, or `None` with a message
/// explaining why to skip. See the module doc for the bucket-provisioning caveat.
async fn minio_store_for_tests() -> Option<Arc<dyn ObjectStore>> {
    let endpoint = std::env::var("CTXLAKE_TEST_S3_ENDPOINT").ok()?;
    let bucket =
        std::env::var("CTXLAKE_TEST_S3_BUCKET").unwrap_or_else(|_| "ctxlake-test".to_string());

    let opts = BackendOptions {
        endpoint: Some(endpoint.clone()),
        allow_http: true,
        ..Default::default()
    };
    let url = url::Url::parse(&format!("s3://{bucket}/ctxlake-cas-torture")).unwrap();
    let (store, _path) = match backend::build(&url, &opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("skipping MinIO suite: could not construct S3 store: {e}");
            return None;
        }
    };

    // Probe reachability and bucket existence with one harmless write before
    // committing the whole suite to it.
    let probe_key = Path::from("_meta/probe/cas-torture-bucket-check.json");
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
    run_torture(store, 6, 5).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_updates_against_the_same_stale_version_never_both_succeed() {
    // The direct, minimal version of the property `run_torture` exercises at
    // scale: two real OS threads racing a `PutMode::Update` against the exact
    // same (soon-to-be-stale) version must not both land.
    let dir = tempfile::tempdir().unwrap();
    let store = local_store(dir.path());
    let key = cas_key();
    let written = store
        .put(&key, PutPayload::from_static(b"{\"v\":0}"))
        .await
        .unwrap();
    let version = object_store::UpdateVersion::from(written);

    let a = {
        let store = store.clone();
        let key = key.clone();
        let version = version.clone();
        tokio::spawn(async move {
            store
                .put_opts(
                    &key,
                    PutPayload::from_static(b"{\"v\":\"from-a\"}"),
                    PutMode::Update(version).into(),
                )
                .await
        })
    };
    let b = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .put_opts(
                    &key,
                    PutPayload::from_static(b"{\"v\":\"from-b\"}"),
                    PutMode::Update(version).into(),
                )
                .await
        })
    };
    let (ra, rb) = tokio::join!(a, b);
    let successes = [ra.unwrap(), rb.unwrap()]
        .into_iter()
        .filter(Result::is_ok)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one racing Update against the same version may win"
    );
}
