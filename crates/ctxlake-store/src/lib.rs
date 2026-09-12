//! ctxlake-store — the coordination layer: compare-and-swap, leases, roster, and the
//! backend capability probe. See AGENTS.md and `docs/architecture.md`.
//!
//! This crate is never linked into `ctxlake-hook` (AGENTS.md invariant 2) — it pulls
//! in `object_store` and `tokio`, both explicitly banned from the hook's dependency
//! tree. Everything here runs in the daemon or in `ctxlake` CLI subcommands, never
//! inline with a tool call.
//!
//! Module map:
//! - [`layout`] — every bucket key, constructed in one place.
//! - [`backend`] — build an [`object_store::ObjectStore`] from a URL and options.
//! - [`clock`] — "now," as the store would answer it, never the caller's wall clock.
//! - [`lease`] — the central abstraction: advisory, CAS-only leases.
//! - [`intent`] — one agent's live intent, single-writer, no CAS.
//! - [`roster`] — the O(N) fan-in of every intent, with an O(N) fallback.
//! - [`probe`] — `ctxlake doctor`'s capability matrix.

pub mod backend;
pub mod clock;
mod error;
pub mod intent;
pub mod layout;
pub mod lease;
mod local_cas;
pub mod probe;
pub mod roster;

pub use error::StoreError;
