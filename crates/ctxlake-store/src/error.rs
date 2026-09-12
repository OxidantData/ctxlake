//! The error type every `ctxlake-store` operation returns.

use thiserror::Error;

/// Errors from `ctxlake-store`.
///
/// This wraps `object_store::Error` rather than re-deriving a parallel set of
/// per-backend failure modes: the backend already tells us precisely which
/// primitive failed (`AlreadyExists`, `Precondition`, `NotModified`, `NotFound`, ...),
/// and callers that care about a specific outcome should match on the wrapped
/// variant (or use the `is_*` helpers below) rather than string-matching a message.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Store(#[from] object_store::Error),

    #[error("malformed json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid store url: {0}")]
    Url(#[from] url::ParseError),

    #[error(
        "unsupported backend scheme {0:?} (expected file, memory, s3, s3a, gs, az, abfs, or abfss)"
    )]
    UnsupportedScheme(String),

    #[error("{0}")]
    Config(String),

    /// `lease::acquire` was called against a key that has never been materialized.
    /// AGENTS.md invariant 4 forbids using `PutMode::Create` to do that lazily
    /// (MinIO rejects `If-None-Match: *` outright, not only on conflict — see
    /// minio/minio#20346), so materializing a lease is a one-time, pre-contention
    /// step: call `lease::provision` once (`ctxlake init` does this) before any
    /// contender may call `acquire`. See the `lease` module doc.
    #[error(
        "lease {0} was never provisioned — call lease::provision() once, before any \
         contender calls acquire(), see AGENTS.md invariant 4"
    )]
    LeaseNotProvisioned(String),
}

impl StoreError {
    /// True for `PutMode::Create` losing to a concurrent writer — the capability
    /// probe's `put-if-absent` check is the only caller left that exercises
    /// `Create` at all; `lease` no longer does (see its module doc for why).
    pub fn is_already_exists(&self) -> bool {
        matches!(
            self,
            StoreError::Store(object_store::Error::AlreadyExists { .. })
        )
    }

    /// True for a `PutMode::Update(version)` that lost a CAS race. AGENTS.md
    /// invariant 4: this is the normal, expected shape of contention on `live/`,
    /// not an error condition — "someone else won" is a result, not a failure.
    pub fn is_precondition_failed(&self) -> bool {
        matches!(
            self,
            StoreError::Store(object_store::Error::Precondition { .. })
        )
    }

    /// True for a conditional `GET` (`If-None-Match`) whose target is unchanged —
    /// the 304 outcome the roster fan-in depends on to stay O(N) instead of O(N^2).
    pub fn is_not_modified(&self) -> bool {
        matches!(
            self,
            StoreError::Store(object_store::Error::NotModified { .. })
        )
    }

    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            StoreError::Store(object_store::Error::NotFound { .. })
        )
    }
}
