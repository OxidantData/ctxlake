//! Advisory, CAS-only leases — the central abstraction of this crate.
//!
//! Read AGENTS.md invariant 4 twice before touching this file.
//!
//! A lease object always exists once first touched; its contents say free or held.
//! We never use `PutMode::Create`/`If-None-Match: *` to decide who holds a lease,
//! because MinIO rejects that precondition outright (minio/minio#20346, closed
//! "working as intended") — a design that relied on it would work on AWS S3 in
//! development and then break in front of the one backend this project exists to
//! support. Every state transition here (acquire from free, steal from an expired
//! holder, renew, release) is the same primitive: read the current JSON body and its
//! [`UpdateVersion`], then `PutMode::Update(version)`. `Create` appears exactly once,
//! in `read_or_create_free`, purely to materialize a never-touched key into the
//! free state — see that function's doc for why the race there is harmless.
//!
//! Leases are advisory, and that should stay visible everywhere. Nothing on any
//! backend in scope can make a write fail because it came from a "stale" holder —
//! there is no server-side fencing primitive to hook into. A process that stalls
//! past its own TTL and then wakes up can still successfully `renew()` if nobody
//! has stolen the lease yet (its `version` is still current), and can still act on
//! stale state it read before stalling. `epoch` (below) documents "how many times
//! has this handed off," not a usable defense against that. For code, git is the
//! real arbiter; for anything irreversible, the target of the action needs its own
//! idempotency key. See `docs/coordination.md`.
//!
//! `epoch` is not a fencing token. It increments by one on every acquire that
//! succeeds — including a steal, and including the same holder re-acquiring after
//! losing its in-memory handle. Log it, show it in `ctxlake status`, use it to
//! notice a lease is thrashing. Do not build a "reject writes from an old epoch"
//! mechanism on top of it: nothing here enforces that on the writer's behalf, so
//! such a mechanism would only be as strong as every caller choosing to check it —
//! which is not a mechanism, it's a convention that one missed call site silently
//! breaks.
//!
//! Expiry never trusts the calling process's clock. `acquire`/`renew` take a
//! [`Clock`] rather than reading `SystemTime::now()` directly —
//! see `clock.rs` for what "now" actually means here and why (AGENTS.md invariant
//! 6).

use std::time::Duration;

use object_store::path::Path;
use object_store::{
    Error as OsError, ObjectStore, ObjectStoreExt, PutMode, PutPayload, UpdateVersion,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::clock::Clock;
use crate::error::StoreError;

/// The full contents of a lease object. Every field is present even when the lease
/// is free (`holder: None`) — there is no "the key doesn't exist" state once a lease
/// has been touched once, by design (see the module doc).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseState {
    pub holder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub acquired_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// See the module doc: a handoff counter, not a fencing token.
    #[serde(default)]
    pub epoch: u64,
}

impl LeaseState {
    fn free() -> Self {
        Self {
            holder: None,
            reason: None,
            acquired_at: None,
            expires_at: None,
            epoch: 0,
        }
    }

    /// True if a contender should be allowed to attempt acquiring this lease: it is
    /// free, or its TTL has elapsed as of `now`.
    ///
    /// This is a *plausibility* check that decides whether to attempt the CAS write
    /// at all — it grants nothing by itself. Two contenders can both see `true` here
    /// and race for real in the write that follows; the CAS is what actually decides
    /// (AGENTS.md invariant 4).
    fn stealable(&self, now: OffsetDateTime) -> bool {
        match (&self.holder, self.expires_at) {
            (None, _) => true,
            (Some(_), Some(expires_at)) => now >= expires_at,
            // A holder with no expiry is a malformed record (every writer here
            // always sets one) — refuse to guess rather than either wedging the
            // lease forever or treating corrupt data as instantly stealable.
            (Some(_), None) => false,
        }
    }
}

/// A held lease. Produced by a successful [`acquire`]; consumed by [`release`] or
/// kept around and passed to [`renew`].
#[derive(Debug, Clone)]
pub struct LeaseHandle {
    pub key: Path,
    pub holder: String,
    pub reason: Option<String>,
    pub epoch: u64,
    pub acquired_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    version: UpdateVersion,
}

/// The result of an [`acquire`] attempt. Losing a CAS race is not a `StoreError` —
/// see AGENTS.md invariant 4 — so both outcomes are `Ok`.
#[derive(Debug, Clone)]
pub enum AcquireOutcome {
    Acquired(LeaseHandle),
    /// Someone else holds it and its TTL hasn't elapsed, *or* a concurrent
    /// contender's write beat ours to the same expired/free lease. `holder` and
    /// `expires_at` reflect what we last observed, not necessarily the live state —
    /// call `acquire` again for a fresh read.
    NotAcquired {
        holder: Option<String>,
        expires_at: Option<OffsetDateTime>,
    },
}

/// Reads `key`, lazily creating it in the free state if it has never been touched.
///
/// Two processes racing to create the same never-touched key both attempt
/// `PutMode::Create` with the *identical* free-state payload — the free state has no
/// fields that vary per-writer. So whichever `Create` the backend accepts, the
/// object ends up holding the same bytes either way; the loser's `AlreadyExists`
/// just means "the value I was about to write is already there," and a plain
/// re-`GET` recovers it. The race is over *who gets to write*, never over *what gets
/// written*, which is exactly why it is bounded (resolves in one extra round trip)
/// and benign (no observer can tell who won).
async fn read_or_create_free(
    store: &dyn ObjectStore,
    key: &Path,
) -> Result<(LeaseState, UpdateVersion), StoreError> {
    match store.get(key).await {
        Ok(res) => {
            let version = UpdateVersion {
                e_tag: res.meta.e_tag.clone(),
                version: res.meta.version.clone(),
            };
            let bytes = res.bytes().await?;
            Ok((serde_json::from_slice(&bytes)?, version))
        }
        Err(OsError::NotFound { .. }) => {
            let free = LeaseState::free();
            let payload = PutPayload::from(serde_json::to_vec(&free)?);
            match store.put_opts(key, payload, PutMode::Create.into()).await {
                Ok(result) => Ok((free, UpdateVersion::from(result))),
                Err(OsError::AlreadyExists { .. }) => {
                    let res = store.get(key).await?;
                    let version = UpdateVersion {
                        e_tag: res.meta.e_tag.clone(),
                        version: res.meta.version.clone(),
                    };
                    let bytes = res.bytes().await?;
                    Ok((serde_json::from_slice(&bytes)?, version))
                }
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Read a lease without side effects. Reports the free state for a never-touched
/// key rather than lazily creating it — a read-only caller (the roster fan-in
/// checking whether anyone holds [`crate::layout::lease_maintenance`]) should never
/// cause a write.
pub async fn read(store: &dyn ObjectStore, key: &Path) -> Result<LeaseState, StoreError> {
    match store.get(key).await {
        Ok(res) => {
            let bytes = res.bytes().await?;
            Ok(serde_json::from_slice(&bytes)?)
        }
        Err(OsError::NotFound { .. }) => Ok(LeaseState::free()),
        Err(e) => Err(e.into()),
    }
}

/// Attempt to acquire `key` for `holder`, valid for `ttl` from the store's clock.
///
/// This one function covers acquiring a free lease, stealing an expired one (the
/// CAS is against the version we just read *from the expired holder's own record*,
/// so of every contender racing this same expired lease, only one write can land),
/// and a holder re-acquiring its own lease. All three are "the object currently
/// permits a new holder to take it," which is exactly `LeaseState::stealable` plus
/// the same-holder case, followed by the identical `PutMode::Update` write.
pub async fn acquire(
    store: &dyn ObjectStore,
    clock: &dyn Clock,
    key: &Path,
    holder: &str,
    reason: Option<&str>,
    ttl: Duration,
) -> Result<AcquireOutcome, StoreError> {
    let (state, version) = read_or_create_free(store, key).await?;
    let now = OffsetDateTime::from(clock.now().await?);

    let eligible = state.holder.as_deref() == Some(holder) || state.stealable(now);
    if !eligible {
        return Ok(AcquireOutcome::NotAcquired {
            holder: state.holder,
            expires_at: state.expires_at,
        });
    }

    let new_state = LeaseState {
        holder: Some(holder.to_string()),
        reason: reason.map(str::to_string),
        acquired_at: Some(now),
        expires_at: Some(now + ttl),
        epoch: state.epoch + 1,
    };
    let payload = PutPayload::from(serde_json::to_vec(&new_state)?);
    match store
        .put_opts(key, payload, PutMode::Update(version).into())
        .await
    {
        Ok(result) => Ok(AcquireOutcome::Acquired(LeaseHandle {
            key: key.clone(),
            holder: holder.to_string(),
            reason: new_state.reason,
            epoch: new_state.epoch,
            acquired_at: now,
            expires_at: new_state.expires_at.expect("just set"),
            version: UpdateVersion::from(result),
        })),
        // Normal contention (AGENTS.md invariant 4), not a bug: report what we saw
        // before attempting the write rather than paying for another round trip
        // the caller may not need — they can call acquire() again for a fresh read.
        Err(OsError::Precondition { .. }) => Ok(AcquireOutcome::NotAcquired {
            holder: state.holder,
            expires_at: state.expires_at,
        }),
        Err(e) => Err(e.into()),
    }
}

/// Extend `handle`'s TTL by `ttl` from the store's clock, in place.
///
/// Returns `Ok(false)` — not an error — if the lease's version no longer matches
/// `handle`'s: someone else's write landed first, which can only mean they won a
/// legitimate steal (our version is otherwise never touched by anyone but us). The
/// handle is left unchanged on that outcome; the caller no longer holds the lease
/// and should treat their critical section as over.
pub async fn renew(
    store: &dyn ObjectStore,
    clock: &dyn Clock,
    handle: &mut LeaseHandle,
    ttl: Duration,
) -> Result<bool, StoreError> {
    let now = OffsetDateTime::from(clock.now().await?);
    let expires_at = now + ttl;
    let new_state = LeaseState {
        holder: Some(handle.holder.clone()),
        reason: handle.reason.clone(),
        acquired_at: Some(handle.acquired_at),
        expires_at: Some(expires_at),
        epoch: handle.epoch,
    };
    let payload = PutPayload::from(serde_json::to_vec(&new_state)?);
    match store
        .put_opts(
            &handle.key,
            payload,
            PutMode::Update(handle.version.clone()).into(),
        )
        .await
    {
        Ok(result) => {
            handle.version = UpdateVersion::from(result);
            handle.expires_at = expires_at;
            Ok(true)
        }
        Err(OsError::Precondition { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Release `handle` back to the free state.
///
/// `epoch` is carried forward unchanged rather than reset to 0 — it counts handoffs
/// over the lease's whole lifetime, not "holds since the last release."
///
/// Returns `Ok(false)` — not an error — if the lease was already stolen out from
/// under `handle` (its version is stale): there is nothing left to release.
pub async fn release(store: &dyn ObjectStore, handle: LeaseHandle) -> Result<bool, StoreError> {
    let freed = LeaseState {
        holder: None,
        reason: None,
        acquired_at: None,
        expires_at: None,
        epoch: handle.epoch,
    };
    let payload = PutPayload::from(serde_json::to_vec(&freed)?);
    match store
        .put_opts(&handle.key, payload, PutMode::Update(handle.version).into())
        .await
    {
        Ok(_) => Ok(true),
        Err(OsError::Precondition { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use object_store::memory::InMemory;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn key() -> Path {
        Path::from("live/leases/test-resource.json")
    }

    #[tokio::test]
    async fn acquires_a_never_touched_lease() {
        let store = InMemory::new();
        let clock = SystemClock;
        let outcome = acquire(
            &store,
            &clock,
            &key(),
            "agent-a",
            Some("editing"),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        match outcome {
            AcquireOutcome::Acquired(h) => {
                assert_eq!(h.holder, "agent-a");
                assert_eq!(h.epoch, 1, "first-ever acquire must be epoch 1");
            }
            other => panic!("expected Acquired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn second_contender_is_refused_while_held() {
        let store = InMemory::new();
        let clock = SystemClock;
        let _first = acquire(
            &store,
            &clock,
            &key(),
            "agent-a",
            None,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let second = acquire(
            &store,
            &clock,
            &key(),
            "agent-b",
            None,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(
            matches!(second, AcquireOutcome::NotAcquired { holder: Some(h), .. } if h == "agent-a")
        );
    }

    #[tokio::test]
    async fn renew_extends_ttl_and_release_frees_it() {
        let store = InMemory::new();
        let clock = SystemClock;
        let AcquireOutcome::Acquired(mut handle) = acquire(
            &store,
            &clock,
            &key(),
            "agent-a",
            None,
            Duration::from_millis(50),
        )
        .await
        .unwrap() else {
            panic!("expected to acquire");
        };
        let old_expiry = handle.expires_at;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(renew(&store, &clock, &mut handle, Duration::from_secs(60))
            .await
            .unwrap());
        assert!(handle.expires_at > old_expiry);

        assert!(release(&store, handle).await.unwrap());
        let state = read(&store, &key()).await.unwrap();
        assert_eq!(state.holder, None, "released lease must read back free");
    }

    #[tokio::test]
    async fn expired_lease_can_be_stolen_by_exactly_one_contender() {
        let store = InMemory::new();
        let clock = SystemClock;
        let AcquireOutcome::Acquired(handle) = acquire(
            &store,
            &clock,
            &key(),
            "agent-a",
            None,
            Duration::from_millis(10),
        )
        .await
        .unwrap() else {
            panic!("expected to acquire");
        };
        drop(handle); // agent-a "dies" mid-hold: no release, TTL just lapses.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let steal_b = acquire(
            &store,
            &clock,
            &key(),
            "agent-b",
            None,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let steal_c = acquire(
            &store,
            &clock,
            &key(),
            "agent-c",
            None,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let winners: Vec<&LeaseHandle> = [&steal_b, &steal_c]
            .into_iter()
            .filter_map(|o| match o {
                AcquireOutcome::Acquired(h) => Some(h),
                AcquireOutcome::NotAcquired { .. } => None,
            })
            .collect();
        assert_eq!(winners.len(), 1, "exactly one contender must win the steal");
        assert_eq!(
            winners[0].epoch, 2,
            "a steal is a successful acquire: epoch must advance"
        );
    }

    #[tokio::test]
    async fn renew_fails_cleanly_once_someone_else_has_stolen_it() {
        let store = InMemory::new();
        let clock = SystemClock;
        let AcquireOutcome::Acquired(mut stale_handle) = acquire(
            &store,
            &clock,
            &key(),
            "agent-a",
            None,
            Duration::from_millis(10),
        )
        .await
        .unwrap() else {
            panic!("expected to acquire");
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let AcquireOutcome::Acquired(_thief) = acquire(
            &store,
            &clock,
            &key(),
            "agent-b",
            None,
            Duration::from_secs(60),
        )
        .await
        .unwrap() else {
            panic!("expected agent-b to steal it");
        };

        // agent-a wakes up late and tries to renew a lease it no longer holds.
        let renewed = renew(&store, &clock, &mut stale_handle, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!renewed, "renew must report false, not error, once stolen");
    }

    #[tokio::test]
    async fn ten_minutes_of_clock_skew_does_not_expire_an_hour_long_lease() {
        // acquire() trusts whatever its `Clock` reports — that trust boundary is
        // the whole point of the `Clock` trait (see clock.rs: `ObjectStoreClock`
        // never lets a caller's own `SystemTime::now()` decide "now", precisely so
        // a contender can't manufacture an early expiry by having a fast clock).
        // This test does not (and cannot, without controlling the real system
        // clock) prove a malicious clock can't lie; it proves the practical
        // guarantee that actually matters: a lease whose TTL comfortably exceeds
        // realistic NTP-class skew (AGENTS.md's own example is +/-10 minutes)
        // survives a contender whose clock carries exactly that much skew, ahead
        // or behind.
        struct SkewedClock(i64);
        impl std::fmt::Debug for SkewedClock {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "SkewedClock({}s)", self.0)
            }
        }
        impl Clock for SkewedClock {
            fn now(
                &self,
            ) -> futures::future::BoxFuture<'_, Result<std::time::SystemTime, StoreError>>
            {
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

        let store = InMemory::new();
        let real_clock = SystemClock;
        let AcquireOutcome::Acquired(_holder) = acquire(
            &store,
            &real_clock,
            &key(),
            "agent-a",
            None,
            Duration::from_secs(3600), // 1 hour — comfortably longer than the skew below.
        )
        .await
        .unwrap() else {
            panic!("expected to acquire");
        };

        for skew_seconds in [600, -600] {
            let skewed = SkewedClock(skew_seconds);
            let contender = acquire(
                &store,
                &skewed,
                &key(),
                "agent-b",
                None,
                Duration::from_secs(60),
            )
            .await
            .unwrap();
            assert!(
                matches!(contender, AcquireOutcome::NotAcquired { .. }),
                "a contender skewed {skew_seconds}s must not expire a still-fresh, hour-long lease"
            );
        }
    }

    #[tokio::test]
    async fn acquire_never_issues_put_if_absent_against_an_existing_lease() {
        // MinIO rejects If-None-Match: * (minio/minio#20346) — PutMode::Create is
        // only ever correct for materializing a never-touched key (see
        // read_or_create_free's doc). Once the object exists, every write acquire()
        // makes — including a steal — must be PutMode::Update.
        #[derive(Debug)]
        struct PutModeSpy {
            inner: Arc<dyn ObjectStore>,
            creates: AtomicUsize,
            updates: AtomicUsize,
        }
        impl std::fmt::Display for PutModeSpy {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "PutModeSpy({})", self.inner)
            }
        }
        #[async_trait::async_trait]
        impl ObjectStore for PutModeSpy {
            async fn put_opts(
                &self,
                location: &Path,
                payload: PutPayload,
                opts: object_store::PutOptions,
            ) -> object_store::Result<object_store::PutResult> {
                match &opts.mode {
                    PutMode::Create => self.creates.fetch_add(1, Ordering::SeqCst),
                    PutMode::Update(_) => self.updates.fetch_add(1, Ordering::SeqCst),
                    PutMode::Overwrite => 0,
                };
                self.inner.put_opts(location, payload, opts).await
            }
            async fn put_multipart_opts(
                &self,
                location: &Path,
                opts: object_store::PutMultipartOptions,
            ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
                self.inner.put_multipart_opts(location, opts).await
            }
            async fn get_opts(
                &self,
                location: &Path,
                options: object_store::GetOptions,
            ) -> object_store::Result<object_store::GetResult> {
                self.inner.get_opts(location, options).await
            }
            fn delete_stream(
                &self,
                locations: futures::stream::BoxStream<'static, object_store::Result<Path>>,
            ) -> futures::stream::BoxStream<'static, object_store::Result<Path>> {
                self.inner.delete_stream(locations)
            }
            fn list(
                &self,
                prefix: Option<&Path>,
            ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
            {
                self.inner.list(prefix)
            }
            async fn list_with_delimiter(
                &self,
                prefix: Option<&Path>,
            ) -> object_store::Result<object_store::ListResult> {
                self.inner.list_with_delimiter(prefix).await
            }
            async fn copy_opts(
                &self,
                from: &Path,
                to: &Path,
                options: object_store::CopyOptions,
            ) -> object_store::Result<()> {
                self.inner.copy_opts(from, to, options).await
            }
        }

        let spy = PutModeSpy {
            inner: Arc::new(InMemory::new()),
            creates: AtomicUsize::new(0),
            updates: AtomicUsize::new(0),
        };
        let clock = SystemClock;

        // First touch: the only Create this whole test should ever see.
        let AcquireOutcome::Acquired(handle) = acquire(
            &spy,
            &clock,
            &key(),
            "agent-a",
            None,
            Duration::from_millis(10),
        )
        .await
        .unwrap() else {
            panic!("expected to acquire");
        };
        assert_eq!(spy.creates.load(Ordering::SeqCst), 1);
        assert_eq!(spy.updates.load(Ordering::SeqCst), 1);

        release(&spy, handle).await.unwrap();
        assert_eq!(
            spy.updates.load(Ordering::SeqCst),
            2,
            "release is an Update, not a Create"
        );

        acquire(
            &spy,
            &clock,
            &key(),
            "agent-b",
            None,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        acquire(
            &spy,
            &clock,
            &key(),
            "agent-c",
            None,
            Duration::from_secs(60),
        )
        .await
        .unwrap(); // this is a steal of an expired lease

        assert_eq!(
            spy.creates.load(Ordering::SeqCst),
            1,
            "no acquire against an existing lease — free, held, or expired — may use PutMode::Create"
        );
    }
}
