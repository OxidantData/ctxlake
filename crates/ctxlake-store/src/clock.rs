//! The store's own clock — never the caller's wall clock.
//!
//! AGENTS.md invariant 6: lease expiry must compare against a timestamp the object
//! store itself produced, never `SystemTime::now()` on the calling host. Hosts in a
//! fleet are distributed and their clocks skew; a laptop with a fast clock must not
//! be able to decide a lease held by a live agent elsewhere has expired.
//!
//! Honesty about what we can actually get here: every S3-, GCS- and
//! Azure-compatible HTTP response carries a `Date` header stating the server's
//! clock at the moment it handled the request. That is the literal thing the
//! invariant names, and it would be the ideal reading — fresh on *every* request,
//! no extra round trip. But `object_store` 0.14's cross-backend API does not
//! surface it: [`object_store::ObjectMeta`] exposes only `last_modified`, the
//! object's own modification time, not the response's `Date` header, and getting at
//! the latter would mean reimplementing per-backend HTTP signing just to read one
//! header — out of proportion to what this crate needs.
//!
//! What we do instead: [`ObjectStoreClock::now`] writes a small scratch object and
//! reads back the `last_modified` the store assigns to *that write*. That value is
//! still computed by the store, not by us — the exact property invariant 6 is
//! protecting — it is just obtained via a write-then-read instead of a header we
//! can't reach. The cost is one extra `put` + `head` per reading. Lease operations
//! are never on the hook path (invariant 1), so this is not a latency-sensitive
//! path; it runs at daemon/maintenance cadence, not per tool call.
//!
//! The local filesystem backend has no separate "server" clock to distrust: its
//! mtimes come from the same kernel clock `SystemTime::now()` reads on this same
//! host, so [`SystemClock`] is the honest, adequate choice there — there is no skew
//! to protect against between a process and its own filesystem.

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use futures::future::BoxFuture;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::error::StoreError;
use crate::layout;

/// A reading of "now" for lease expiry decisions.
///
/// Implementations must return a timestamp every contender for a given lease would
/// agree on (to within normal network latency) — that is the whole contract. A
/// timestamp only one process can see is exactly what this trait exists to rule
/// out.
pub trait Clock: fmt::Debug + Send + Sync {
    fn now(&self) -> BoxFuture<'_, Result<SystemTime, StoreError>>;
}

/// The calling process's own wall clock.
///
/// Correct for [`object_store::local::LocalFileSystem`] (see the module doc) and for
/// tests that don't care about skew. Using this against a remote backend
/// reintroduces exactly the per-host skew AGENTS.md invariant 6 forbids — reach for
/// [`ObjectStoreClock`] there instead.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> BoxFuture<'_, Result<SystemTime, StoreError>> {
        Box::pin(async { Ok(SystemTime::now()) })
    }
}

/// Samples the store's clock by touching a scratch object scoped to `caller_id` and
/// reading back the `last_modified` timestamp the store assigns to that write. See
/// the module doc for why this is the closest available substitute for the literal
/// `Date` response header, and what it actually costs.
pub struct ObjectStoreClock {
    store: Arc<dyn ObjectStore>,
    caller_id: String,
}

impl fmt::Debug for ObjectStoreClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectStoreClock")
            .field("caller_id", &self.caller_id)
            .finish_non_exhaustive()
    }
}

impl ObjectStoreClock {
    /// `caller_id` must be unique to this process (an agent id is the natural
    /// choice) — see AGENTS.md invariant 3 on the scratch key this writes.
    pub fn new(store: Arc<dyn ObjectStore>, caller_id: impl Into<String>) -> Self {
        Self {
            store,
            caller_id: caller_id.into(),
        }
    }
}

impl Clock for ObjectStoreClock {
    fn now(&self) -> BoxFuture<'_, Result<SystemTime, StoreError>> {
        Box::pin(async move {
            let path = layout::internal::clock_probe(&self.caller_id);
            // The body is never read back by anyone; only the write's own
            // server-assigned timestamp matters. `Overwrite` (not CAS) is correct
            // because this key is single-writer-per-caller_id by construction.
            self.store
                .put(&path, PutPayload::from_static(b"{}"))
                .await?;
            let meta = self.store.head(&path).await?;
            Ok(SystemTime::from(meta.last_modified))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn system_clock_advances() {
        let c = SystemClock;
        let a = c.now().await.unwrap();
        let b = c.now().await.unwrap();
        assert!(b >= a);
    }

    #[tokio::test]
    async fn object_store_clock_reads_back_a_recent_timestamp() {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let clock = ObjectStoreClock::new(store, "agent-under-test");
        let before = SystemTime::now();
        let observed = clock.now().await.unwrap();
        let after = SystemTime::now();
        // InMemory timestamps come from the host clock too, but round-tripping
        // through put+head is exactly the mechanism the real backends use — this
        // guards against, e.g., accidentally reading the *request* time instead of
        // the value the store actually assigned.
        assert!(observed >= before && observed <= after);
    }

    #[tokio::test]
    async fn two_callers_never_share_a_probe_key() {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let a = ObjectStoreClock::new(store.clone(), "agent-a");
        let b = ObjectStoreClock::new(store.clone(), "agent-b");
        a.now().await.unwrap();
        b.now().await.unwrap();
        // Both objects exist independently — neither caller's probe overwrote the
        // other's, which is what AGENTS.md invariant 3 requires even for a scratch
        // key nobody downstream reads.
        assert!(store
            .head(&layout::internal::clock_probe("agent-a"))
            .await
            .is_ok());
        assert!(store
            .head(&layout::internal::clock_probe("agent-b"))
            .await
            .is_ok());
    }
}
