//! ctxlake-maint — the mechanical half of turning captured sessions into something
//! queryable and (eventually) memorable. See AGENTS.md and `docs/architecture.md`'s
//! maintenance chain.
//!
//! Everything ctxlake does after a session is sealed.
//!
//! Two halves with very different risk profiles, deliberately kept distinguishable:
//!
//! **The arithmetic half cannot be wrong.** Compaction repacks Parquet, the digest
//! counts exit codes and elapsed time, and the snapshot folds an event log into
//! SQLite. All three are derived so directly from captured events that there is
//! nothing for them to be mistaken about — see `docs/summarization.md`'s Tier 0.
//!
//! **The belief half can be.** Tier 2 extraction asks a model what a session meant,
//! and the promotion gate decides what the fleet gets to believe. That is why the
//! gates exist, why shadow mode is the default, and why `docs/memory.md` spends most
//! of its length on what the layer refuses to do.
//!
//! Module map — arithmetic:
//! - [`compact`] — rewrite a day's sealed session segments into fewer, larger
//!   Parquet files. Idempotent, deduped on meaningful `content_hash`.
//! - [`digest`] — the Tier 0 structural digest per sealed session, including
//!   friction signals (repeated failures, hot files, abandonment).
//! - [`snapshot`] — fold `claims/events/` into a content-addressed SQLite artifact
//!   with an FTS5 index, published via write-then-CAS-swap.
//! - [`partition`] — parse a bronze session's identity back out of its object key;
//!   the read-side inverse of `ctxlake_store::layout`'s write-side construction.
//! - [`run`] — the maintenance chain (compact -> digest -> snapshot), serialized
//!   fleet-wide by `live/leases/_maintenance`.
//!
//! Module map — belief:
//! - [`claims`] — the append-only claim event log and its fold into current state.
//! - [`extract`] — Tier 2 batch extraction over sealed sessions.
//! - [`gate`] — the four promotion gates, including shadow mode's agent-read cutoff.

mod error;

pub mod claims;
pub mod compact;
pub mod digest;
pub mod extract;
pub mod gate;
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
