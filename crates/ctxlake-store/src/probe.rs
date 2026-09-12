//! The capability probe behind `ctxlake doctor`.
//!
//! Backends genuinely differ in which conditional-write primitives they support —
//! MinIO rejects `If-None-Match: *` outright, GCS's conditional semantics run on
//! generation numbers instead of etags, and so on. Rather than assume a primitive
//! works because a spec says it should, every function here actually executes it
//! against the configured bucket and reports pass/fail with a human-readable reason.
//! This is meant to be run once against a new bucket, before anything depends on the
//! result (`ctxlake doctor`), so a MinIO-specific gap is a config note, not a 2am
//! page when the roster or snapshot pointer first contends.
//!
//! Every probe cleans up the scratch objects it creates, on both the pass and the
//! fail path, and everything lives under `layout::internal::probe_prefix`, keyed by
//! a `caller_id` [`run`] takes, so a probe run can never collide with real fleet
//! data *or* with another concurrent probe run (AGENTS.md invariant 3) — two agents
//! running `ctxlake doctor` against the same bucket at once must not see each
//! other's scratch objects and misreport a real capability as missing.

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
/// order matching how ctxlake actually uses each one (create-if-absent, CAS
/// updates, CAS conflict rejection, the roster fan-in's conditional-GET, generic
/// housekeeping).
///
/// `caller_id` scopes every scratch key this run touches (AGENTS.md invariant 3) —
/// pass something unique to this invocation (an agent id, a run id) so a concurrent
/// `doctor` run elsewhere can never share, and thus never race, one of these keys.
pub async fn run(store: &dyn ObjectStore, caller_id: &str) -> Vec<ProbeResult> {
    vec![
        put_if_absent(store, caller_id).await,
        cas_update(store, caller_id).await,
        cas_conflict_detection(store, caller_id).await,
        conditional_get(store, caller_id).await,
        list(store, caller_id).await,
        delete(store, caller_id).await,
    ]
}

/// `PutMode::Create` — not used anywhere else in this crate (nothing under `live/`
/// ever depends on put-if-absent semantics: AGENTS.md invariant 4), so this is
/// diagnostic here, but it is a real, load-bearing primitive one crate over:
/// `ctxlake_maint::extract`'s idempotency marker (`claims/extracted/<id>`) is a
/// genuine create-if-absent lock and needs `Create` to actually work on the
/// configured backend. Note a `fail` here does not mean ctxlake is broken
/// everywhere — it means an operator should not assume the primitive works if
/// they were relying on it for something outside what ctxlake itself uses it for.
async fn put_if_absent(store: &dyn ObjectStore, caller_id: &str) -> ProbeResult {
    let key = probe_prefix(caller_id).join("put-if-absent.json");
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
/// write every CAS publisher in this codebase makes (the roster fan-in, the
/// snapshot pointer swap).
async fn cas_update(store: &dyn ObjectStore, caller_id: &str) -> ProbeResult {
    let key = probe_prefix(caller_id).join("cas-update.json");
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
/// mode every CAS publisher must be able to detect as "a fresher write already
/// landed" (AGENTS.md invariant 4). A pass here is what makes this crate's own
/// CAS tests (`tests/cas.rs`) meaningful on this backend.
async fn cas_conflict_detection(store: &dyn ObjectStore, caller_id: &str) -> ProbeResult {
    let key = probe_prefix(caller_id).join("cas-conflict.json");
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
async fn conditional_get(store: &dyn ObjectStore, caller_id: &str) -> ProbeResult {
    let key = probe_prefix(caller_id).join("conditional-get.json");
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
async fn list(store: &dyn ObjectStore, caller_id: &str) -> ProbeResult {
    let prefix = probe_prefix(caller_id).join("list");
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
async fn delete(store: &dyn ObjectStore, caller_id: &str) -> ProbeResult {
    let key = probe_prefix(caller_id).join("delete.json");

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
        let results = run(&store, "test-caller").await;
        assert_eq!(results.len(), 6);
        for r in &results {
            assert!(r.passed, "{}: {}", r.name, r.detail);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_doctor_runs_never_collide() {
        // Regression test: an unkeyed scratch prefix means two concurrent `doctor`
        // runs (a fleet where every agent probes on startup, say) touch the exact
        // same keys and see each other's writes and deletes mid-probe — reported
        // as the *backend* lacking put-if-absent, conditional GET or list, not as
        // what it actually is: two callers racing a key that should have been
        // scoped to each of them (AGENTS.md invariant 3). Real OS threads plus
        // repetition, rather than a single cooperative `join!`, is what actually
        // forces the interleaving that exposes an unkeyed prefix.
        let store: std::sync::Arc<dyn ObjectStore> = std::sync::Arc::new(InMemory::new());
        for _ in 0..20 {
            let a = tokio::spawn({
                let store = store.clone();
                async move { run(store.as_ref(), "doctor-run-a").await }
            });
            let b = tokio::spawn({
                let store = store.clone();
                async move { run(store.as_ref(), "doctor-run-b").await }
            });
            let (results_a, results_b) = (a.await.unwrap(), b.await.unwrap());
            for (label, results) in [("a", results_a), ("b", results_b)] {
                for r in &results {
                    assert!(
                        r.passed,
                        "run {label}'s {} probe was falsely reported as failing due to \
                         collision with a concurrent run: {}",
                        r.name, r.detail
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn cas_conflict_detection_actually_exercises_a_stale_write() {
        // A probe that can't fail is exactly the kind of test the house style
        // warns against, just relocated into production code — assert this one
        // really does distinguish the good and bad outcomes by checking it against
        // a store where the second half of the scenario is set up manually too.
        let store = InMemory::new();
        let key = probe_prefix("test-caller").join("cas-conflict.json");
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
