//! ctxlake-maint — the mechanical half of turning captured sessions into something
//! queryable and (eventually) memorable. See AGENTS.md and `docs/architecture.md`'s
//! maintenance chain.
//!
//! **Everything in this crate's wave is arithmetic. There is no LLM anywhere in this
//! stream** — compaction dedups and repacks Parquet, the digest counts exit codes
//! and elapsed time, and the snapshot folds an event log into SQLite. All three are
//! derived so directly from captured events that they cannot be wrong the way a
//! model's summary can be — see `docs/summarization.md`'s Tier 0 for the argument in
//! full. Batch extraction (Tier 2, the one tier that *does* call an LLM) and the
//! promotion gate are a different wave's responsibility and do not live here.
//!
//! Module map:
//! - [`compact`] — rewrite a day's sealed session segments into fewer, larger
//!   Parquet files. Idempotent, deduped on meaningful `content_hash`.
//! - [`digest`] — the Tier 0 structural digest per sealed session, including
//!   friction signals (repeated failures, hot files, abandonment).
//! - [`snapshot`] — fold `claims/events/` into a content-addressed SQLite artifact
//!   with an FTS5 index, published via write-then-CAS-swap.
//! - [`run`] — the maintenance chain (compact -> digest -> snapshot), serialized
//!   fleet-wide by `live/leases/_maintenance`.
//! - [`partition`] — parse a bronze session's identity back out of its object key;
//!   the read-side inverse of `ctxlake_store::layout`'s write-side construction.

mod error;

pub mod compact;
pub mod digest;
pub mod partition;
pub mod run;
pub mod snapshot;

pub use error::MaintError;

/// The current instant, RFC 3339-formatted, falling back to the Unix epoch on a
/// formatting failure that should never actually happen — mirrors
/// `ctxlake-sync::upload`'s identical `today_rfc3339` fallback for the same reason:
/// this is metadata inside a maintenance marker, not something worth panicking a
/// background job over.
pub(crate) fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
