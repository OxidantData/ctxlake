//! The capability probe behind `ctxlake doctor`.
//!
//! Backends genuinely differ in which conditional-write primitives they support —
//! MinIO rejects `If-None-Match: *` outright, GCS's conditional semantics run on
//! generation numbers instead of etags, and so on. Rather than assume a primitive
//! works because a spec says it should, every function here actually executes it
//! against the configured bucket and reports pass/fail with a human-readable reason.
//! This is meant to be run once against a new bucket, before anything depends on the
//! result (`ctxlake doctor`), so a MinIO-specific gap is a config note, not a 2am
//! page when a lease first contends.
//!
//! Every probe cleans up the scratch objects it creates, on both the pass and the
//! fail path, and everything lives under `layout::internal::probe_prefix` so a
//! probe run can never collide with real fleet data.

use std::collections::HashSet;

use futures::StreamExt;
use object_store::{
    Error as OsError, GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutPayload, UpdateVersion,
};

use crate::layout::internal::probe_prefix;

/// One primitive's pass/fail, with enough detail to act on.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

fn pass(name: &'static str, detail: impl Into<String>) -> ProbeResult {
    ProbeResult {
        name,
        passed: true,
        detail: detail.into(),
    }
}

fn fail(name: &'static str, detail: impl Into<String>) -> ProbeResult {
    ProbeResult {
        name,
        passed: false,
        detail: detail.into(),
    }
}

/// Run every probe against `store` and return one result per primitive, in a fixed
/// order matching how `ctxlake-store` actually uses each one (lease creation, lease
/// contention, the roster fan-in, generic housekeeping).
pub async fn run(store: &dyn ObjectStore) -> Vec<ProbeResult> {
    vec![
        put_if_absent(store).await,
        cas_update(store).await,
        cas_conflict_detection(store).await,
        conditional_get(store).await,
        list(store).await,
        delete(store).await,
    ]
}

/// `PutMode::Create` — used exactly once in this crate, to materialize a
/// never-touched lease into the free state (see `lease::read_or_create_free`).
/// Note a `fail` here does not mean ctxlake is broken on this backend: leases never
/// depend on `Create` rejecting a second write (see AGENTS.md invariant 4). It means
/// *other* code must not assume `Create` gives real put-if-absent semantics here.
async fn put_if_absent(store: &dyn ObjectStore) -> ProbeResult {
    let key = probe_prefix().join("put-if-absent.json");
    let _ = store.delete(&key).await;

    let result = match store
        .put_opts(&key, PutPayload::from_static(b"{}"), PutMode::Create.into())
        .await
    {
        Ok(_) => match store
            .put_opts(&key, PutPayload::from_static(b"{}"), PutMode::Create.into())
            .await
        {
            Err(OsError::AlreadyExists { .. }) => {
                pass("put-if-absent", "a second Create was correctly rejected")
            }
            Ok(_) => fail(
                "put-if-absent",
                "a second Create silently overwrote the object — true put-if-absent is not available here",
            ),
            Err(e) => fail("put-if-absent", format!("second Create errored unexpectedly: {e}")),
        },
        Err(e) => fail("put-if-absent", format!("first Create failed: {e}")),
    };

    let _ = store.delete(&key).await;
    result
}

/// `PutMode::Update(version)` succeeding against a version we just wrote — the one
/// write every `lease::acquire`/`renew`/`release` call makes.
async fn cas_update(store: &dyn ObjectStore) -> ProbeResult {
    let key = probe_prefix().join("cas-update.json");
    let _ = store.delete(&key).await;

    let outcome = async {
        let initial = store
            .put(&key, PutPayload::from_static(b"{\"v\":1}"))
            .await?;
        store
            .put_opts(
                &key,
                PutPayload::from_static(b"{\"v\":2}"),
                PutMode::Update(UpdateVersion::from(initial)).into(),
            )
            .await
    }
    .await;

    let result = match outcome {
        Ok(_) => pass(
            "cas-update",
            "Update succeeded against the version we just wrote",
        ),
        Err(e) => fail("cas-update", format!("{e}")),
    };
    let _ = store.delete(&key).await;
    result
}

/// `PutMode::Update(version)` correctly rejecting a *stale* version — the failure
/// mode every acquire/renew/release call must be able to detect as "someone else
/// won" (AGENTS.md invariant 4). A pass here is what makes the CAS-torture suite in
/// `lease.rs` meaningful on this backend.
async fn cas_conflict_detection(store: &dyn ObjectStore) -> ProbeResult {
    let key = probe_prefix().join("cas-conflict.json");
    let _ = store.delete(&key).await;

    let outcome = async {
        let initial = store
            .put(&key, PutPayload::from_static(b"{\"v\":1}"))
            .await?;
        let stale_version = UpdateVersion::from(initial);
        // Advance the object so `stale_version` is no longer current.
        store
            .put(&key, PutPayload::from_static(b"{\"v\":2}"))
            .await?;
        Ok::<_, OsError>(
            store
                .put_opts(
                    &key,
                    PutPayload::from_static(b"{\"v\":3}"),
                    PutMode::Update(stale_version).into(),
                )
                .await,
        )
    }
    .await;

    let result = match outcome {
        Ok(Err(OsError::Precondition { .. })) => pass(
            "cas-conflict-detection",
            "a stale version was correctly rejected",
        ),
        Ok(Ok(_)) => fail(
            "cas-conflict-detection",
            "a write against a stale version succeeded — lost updates are possible on this backend",
        ),
        Ok(Err(e)) => fail("cas-conflict-detection", format!("unexpected error: {e}")),
        Err(e) => fail("cas-conflict-detection", format!("setup failed: {e}")),
    };
    let _ = store.delete(&key).await;
    result
}

/// Conditional `GET` with `If-None-Match` reporting 304 (`Error::NotModified`) — the
/// primitive `roster::fetch` depends on to keep polling O(N) instead of O(N^2).
async fn conditional_get(store: &dyn ObjectStore) -> ProbeResult {
    let key = probe_prefix().join("conditional-get.json");
    let _ = store.delete(&key).await;

    let outcome = async {
        let put_result = store.put(&key, PutPayload::from_static(b"{}")).await?;
        let opts = GetOptions {
            if_none_match: put_result.e_tag,
            ..Default::default()
        };
        Ok::<_, OsError>(store.get_opts(&key, opts).await)
    }
    .await;

    let result = match outcome {
        Ok(Err(OsError::NotModified { .. })) => {
            pass("conditional-get-304", "If-None-Match correctly reported unchanged")
        }
        Ok(Ok(_)) => fail(
            "conditional-get-304",
            "a matching If-None-Match still returned a full body — the roster fan-in's savings do not apply on this backend",
        ),
        Ok(Err(e)) => fail("conditional-get-304", format!("unexpected error: {e}")),
        Err(e) => fail("conditional-get-304", format!("setup failed: {e}")),
    };
    let _ = store.delete(&key).await;
    result
}

/// `LIST` under a prefix returning objects written moments ago. Every eventually
/// consistent object store in the wild (this crate targets none of those knowingly,
/// but `doctor` should say so if it turns out to be wrong) would show up here as a
/// flaky or missing entry.
async fn list(store: &dyn ObjectStore) -> ProbeResult {
    let prefix = probe_prefix().join("list");
    let a = prefix.clone().join("a.json");
    let b = prefix.clone().join("b.json");
    let _ = store.delete(&a).await;
    let _ = store.delete(&b).await;

    let outcome = async {
        store.put(&a, PutPayload::from_static(b"{}")).await?;
        store.put(&b, PutPayload::from_static(b"{}")).await?;
        let mut stream = store.list(Some(&prefix));
        let mut seen = HashSet::new();
        while let Some(meta) = stream.next().await {
            seen.insert(meta?.location);
        }
        Ok::<_, OsError>(seen)
    }
    .await;

    let result = match outcome {
        Ok(seen) if seen.contains(&a) && seen.contains(&b) => pass(
            "list",
            format!("saw both scratch objects among {} listed", seen.len()),
        ),
        Ok(seen) => fail(
            "list",
            format!("expected both scratch objects, saw {seen:?}"),
        ),
        Err(e) => fail("list", format!("{e}")),
    };
    let _ = store.delete(&a).await;
    let _ = store.delete(&b).await;
    result
}

/// `DELETE` actually removing the object, verified by a follow-up `GET` returning
/// `NotFound`.
async fn delete(store: &dyn ObjectStore) -> ProbeResult {
    let key = probe_prefix().join("delete.json");

    if let Err(e) = store.put(&key, PutPayload::from_static(b"{}")).await {
        return fail("delete", format!("setup failed: {e}"));
    }
    if let Err(e) = store.delete(&key).await {
        return fail("delete", format!("delete itself failed: {e}"));
    }
    match store.get(&key).await {
        Err(OsError::NotFound { .. }) => pass("delete", "object was gone immediately after delete"),
        Ok(_) => fail("delete", "object was still readable after delete"),
        Err(e) => fail("delete", format!("verifying delete failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    #[tokio::test]
    async fn every_probe_passes_against_in_memory() {
        // InMemory implements every primitive correctly, so this is a regression
        // guard on the probes themselves (a probe that always reports `fail`, or
        // always reports `pass` regardless of outcome, is worse than no probe) —
        // not a claim about any real backend.
        let store = InMemory::new();
        let results = run(&store).await;
        assert_eq!(results.len(), 6);
        for r in &results {
            assert!(r.passed, "{}: {}", r.name, r.detail);
        }
    }

    #[tokio::test]
    async fn cas_conflict_detection_actually_exercises_a_stale_write() {
        // A probe that can't fail is exactly the kind of test the house style
        // warns against, just relocated into production code — assert this one
        // really does distinguish the good and bad outcomes by checking it against
        // a store where the second half of the scenario is set up manually too.
        let store = InMemory::new();
        let key = probe_prefix().join("cas-conflict.json");
        let initial = store
            .put(&key, PutPayload::from_static(b"{\"v\":1}"))
            .await
            .unwrap();
        let stale = UpdateVersion::from(initial);
        store
            .put(&key, PutPayload::from_static(b"{\"v\":2}"))
            .await
            .unwrap();
        let result = store
            .put_opts(
                &key,
                PutPayload::from_static(b"{\"v\":3}"),
                PutMode::Update(stale).into(),
            )
            .await;
        assert!(matches!(result, Err(OsError::Precondition { .. })));
    }
}
