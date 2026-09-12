//! The error type every `ctxlake-maint` operation returns.

use thiserror::Error;

/// Errors from `ctxlake-maint`.
///
/// Mirrors `ctxlake-store`'s `StoreError` in spirit (wrap, don't re-derive a parallel
/// per-backend failure taxonomy) — see that crate's `error.rs`.
#[derive(Debug, Error)]
pub enum MaintError {
    #[error(transparent)]
    Store(#[from] ctxlake_store::StoreError),

    #[error(transparent)]
    ObjectStore(#[from] object_store::Error),

    #[error("malformed json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("parquet/arrow codec: {0}")]
    Codec(String),

    #[error("{0}")]
    Other(String),
}
