//! ctxlake-maint — see AGENTS.md and docs/architecture.md.
//!
//! Compaction, digests, and snapshot building are still a scaffold, owned by a
//! different wave. This crate currently ships the belief layer (wave 3):
//!
//! - [`claims`] — the append-only claim event log and its fold into current state.
//! - [`extract`] — Tier 2 batch extraction over sealed sessions (docs/summarization.md).
//! - [`gate`] — the four promotion gates (docs/memory.md), including shadow mode's
//!   agent-read cutoff.

pub mod claims;
pub mod extract;
pub mod gate;
