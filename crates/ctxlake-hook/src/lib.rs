//! ctxlake-hook as a library, so its adapters can be exercised by tests without
//! spawning a subprocess per fixture. `main.rs` is a thin wrapper over this crate.
//!
//! Everything here fires on every tool call and is budgeted at 5ms p99 (AGENTS.md
//! invariant 2). Dependencies are pinned to `ctxlake-core`, `serde`, `serde_json` —
//! CI's `hook-deps` job fails the build the moment this tree grows an async runtime,
//! an HTTP stack, or an object store.

pub mod adapters;
pub mod briefing;
pub mod clock;
pub mod hostinfo;
pub mod spool;
