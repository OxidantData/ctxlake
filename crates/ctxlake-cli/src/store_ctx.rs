//! Wires a [`Config`] to a live [`ctxlake_store`] backend and clock.
//!
//! Every subcommand that talks to the object store (`init`, `doctor`, `status`,
//! `claim`, `release`) goes through here so "how do I turn `ctxlake.toml`'s `store`
//! line into a connection" has exactly one answer.

use std::sync::Arc;

use anyhow::{Context, Result};
use ctxlake_store::backend::{self, BackendOptions};
use ctxlake_store::clock::{Clock, ObjectStoreClock, SystemClock};
use object_store::path::Path as StorePath;
use object_store::ObjectStore;
use url::Url;

use crate::config::Config;

pub struct StoreCtx {
    pub store: Arc<dyn ObjectStore>,
    /// The path segment `store`'s URL pointed at within the backend (a bucket
    /// prefix) — every `ctxlake_store::layout` key must be joined onto this, not
    /// used as if the store were rooted at the bucket itself.
    pub prefix: StorePath,
    pub clock: Arc<dyn Clock>,
}

/// Build a store connection for `cfg`. `caller_id` scopes any scratch objects this
/// session's operations touch (AGENTS.md invariant 3) — pass something unique per
/// invocation, an agent id is the natural choice.
pub fn connect(cfg: &Config, caller_id: &str) -> Result<StoreCtx> {
    let url = Url::parse(&cfg.store)
        .with_context(|| format!("{:?} is not a valid store URL", cfg.store))?;
    let (store, prefix) = backend::build(&url, &BackendOptions::default())
        .with_context(|| format!("connecting to {}", cfg.store))?;
    // The local filesystem backend has no remote clock to distrust (see
    // `ctxlake_store::clock`'s module doc) — everywhere else, trusting our own
    // wall clock for lease expiry is exactly what AGENTS.md invariant 6 forbids.
    let clock: Arc<dyn Clock> = if url.scheme() == "file" {
        Arc::new(SystemClock)
    } else {
        Arc::new(ObjectStoreClock::new(store.clone(), caller_id.to_string()))
    };
    Ok(StoreCtx {
        store,
        prefix,
        clock,
    })
}

/// Join a `ctxlake_store::layout` key onto this store's configured prefix. Every
/// call site in this crate should use this rather than passing a bare `layout::*()`
/// path straight to the store — `store` URLs like `s3://bucket/ctxlake` carry a
/// prefix (`ctxlake`) that every key must sit under.
pub fn full_path(ctx: &StoreCtx, key: &StorePath) -> StorePath {
    ctx.prefix.parts().chain(key.parts()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connects_to_a_file_store_and_prefixes_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::new(
            format!("file://{}", dir.path().display()),
            "myteam",
            "cc-01",
        );
        let ctx = connect(&cfg, "cc-01").unwrap();
        let key = ctxlake_store::layout::fleet_meta();
        let full = full_path(&ctx, &key);
        assert_eq!(full, key, "file:// stores have an empty prefix");
    }

    #[test]
    fn full_path_joins_a_bucket_prefix() {
        let prefix = StorePath::from("ctxlake");
        let key = ctxlake_store::layout::fleet_meta();
        let ctx = StoreCtx {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: prefix.clone(),
            clock: Arc::new(SystemClock),
        };
        let full = full_path(&ctx, &key);
        assert_eq!(full.as_ref(), "ctxlake/_meta/fleet.json");
    }

    #[test]
    fn rejects_an_unparseable_store_url() {
        // Not `.unwrap_err()`: that requires `StoreCtx: Debug` (to print the Ok
        // value on failure), and a trait-object field makes that more machinery
        // than a one-line match is worth.
        let cfg = Config::new("not a url at all", "myteam", "cc-01");
        match connect(&cfg, "cc-01") {
            Err(e) => assert!(e.to_string().contains("not a valid store URL")),
            Ok(_) => panic!("expected an unparseable store URL to fail"),
        }
    }
}
