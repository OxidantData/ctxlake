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
}

impl StoreError {
    /// True for `PutMode::Create` losing to a concurrent writer — the capability
    /// probe's `put-if-absent` check exercises this deliberately; a genuine
    /// create-if-absent idempotency marker (`ctxlake_maint::extract`'s
    /// `claims/extracted/<id>`) relies on it for real, to tell "I raced and lost"
    /// apart from a real backend failure.
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
