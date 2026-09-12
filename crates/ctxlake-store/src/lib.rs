//! ctxlake-store — the coordination layer: compare-and-swap, roster, and the
//! backend capability probe. See AGENTS.md and `docs/architecture.md`.
//!
//! This crate is never linked into `ctxlake-hook` (AGENTS.md invariant 2) — it pulls
//! in `object_store` and `tokio`, both explicitly banned from the hook's dependency
//! tree. Everything here runs in the daemon or in `ctxlake` CLI subcommands, never
//! inline with a tool call.
//!
//! **There is no lease/lock abstraction here, on purpose.** Every unit of work this
//! crate coordinates is already idempotent by content or by construction: the
//! roster and the snapshot pointer are published with a CAS write (last write
//! among concurrent, equally-current publishers wins, and a stale write is
//! rejected — never a torn read), and `ctxlake-maint`'s compaction generations and
//! extraction markers (outside this crate) use the same "no holder, no TTL, no
//! stealing" idempotency instead of exclusivity. An earlier version of this crate
//! had a `lease` module — holder, TTL, expiry, steal, contention — layered on top
//! of work that never needed it. Do not add one back; make the new work
//! content-addressed or CAS-published instead, the same way everything else here
//! already is.
//!
//! Module map:
//! - [`layout`] — every bucket key, constructed in one place.
//! - [`backend`] — build an [`object_store::ObjectStore`] from a URL and options.
//! - [`clock`] — "now," as the store would answer it, never the caller's wall clock.
//! - [`intent`] — one agent's live intent, single-writer, no CAS.
//! - [`roster`] — the O(N) fan-in of every intent, CAS-published so any number of
//!   builders may race to write it.
//! - [`probe`] — `ctxlake doctor`'s capability matrix.

pub mod backend;
pub mod clock;
mod error;
pub mod intent;
pub mod layout;
mod local_cas;
pub mod probe;
pub mod roster;

pub use error::StoreError;
