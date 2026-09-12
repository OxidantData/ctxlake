//! Local filesystem roots — the only thing this crate is allowed to touch.
//!
//! `docs/architecture.md`'s component table is explicit: `ctxlake-mcp` reads the
//! local **cache** and writes the local **spool**, and never the object store,
//! directly or indirectly — the same discipline as `ctxlake-hook`, for the same
//! reason (AGENTS.md invariant 1): this process runs on a path the calling agent is
//! synchronously waiting on. There is no `object_store` or `tokio` dependency in
//! this crate's `Cargo.toml` at all, so that boundary cannot be crossed by accident.
//!
//! `ctxlake-hook` already established `~/.ctxlake/spool` (overridable with
//! `CTXLAKE_SPOOL_DIR`) as the real, implemented spool root — see
//! `crates/ctxlake-hook/src/spool.rs`. This module points at that same root rather
//! than a second, separately-resolved root: a daemon that eventually drains both the hook's
//! and this crate's spool output needs one tree, not two, and the hook's tree is
//! the one that already exists on disk. `docs/reference.md` says so plainly, since
//! papering over a gap between docs and the actual on-disk layout is exactly the
//! kind of overclaim `docs/checks/regression_checks.py` exists to catch elsewhere
//! in this repo.
//!
//! `CTXLAKE_CACHE_DIR` (default `~/.ctxlake/cache`) has no writer yet anywhere in
//! this codebase — the daemon that would populate it (`ctxlake sync`) is later
//! work. Every read through this root is written to degrade honestly when the
//! directory or file it wants is simply missing, not to treat that as an error.
//!
//! As with `ctxlake-hook`'s `spool.rs`, every function that touches disk takes an
//! explicit root instead of reading the environment itself, so tests never race
//! each other over a shared process-global env var; the `_at`-suffixed functions
//! are the ones under test, and the env-reading wrappers below them are the only
//! things `lib.rs` calls in the real binary.

use std::path::PathBuf;

/// Where write-shaped tool calls (`fleet_handoff`, `memory_propose`) queue their
/// output for `ctxlake sync` to apply. Same root and same env var `ctxlake-hook`
/// uses, so one daemon drains one tree.
pub fn spool_root() -> PathBuf {
    ctxlake_core::paths::spool_root()
}

/// Where read-shaped tool calls (`fleet_status`, `fleet_history`, `memory_search`,
/// `memory_timeline`) look for whatever `ctxlake sync`'s store-to-cache leg has
/// most recently refreshed. Nothing writes here yet in this codebase — every
/// reader must treat a missing file as "not synced yet," not as a bug.
pub fn cache_root() -> PathBuf {
    ctxlake_core::paths::cache_root()
}

/// Logical fleet identity, stable across restarts. `ctxlake-core`'s envelope
/// carries the same field for the same reason: it is how session data and this
/// process's local reads/writes are scoped to one coordination group.
pub fn fleet_id() -> String {
    std::env::var("CTXLAKE_FLEET_ID").unwrap_or_else(|_| "default".to_string())
}

/// This agent's own logical id — used to attribute what this process writes, never
/// to decide what it may read (reads are fleet-scoped, not agent-scoped).
pub fn agent_id() -> String {
    std::env::var("CTXLAKE_AGENT_ID").unwrap_or_else(|_| "unknown".to_string())
}

/// Everything a tool call needs to know about where "here" is: the local roots to
/// read and write, and this fleet/agent's identity. Built once per process from
/// the environment (see [`Ctx::from_env`]) and threaded through every dispatch
/// call rather than read from the environment again on each request — partly
/// hygiene, mostly so tests can construct an isolated [`Ctx`] pointed at a tempdir
/// instead of racing each other over shared process-global env vars the way a
/// design that re-read `std::env` per call would force them to.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub spool_root: PathBuf,
    pub cache_root: PathBuf,
    pub fleet_id: String,
    pub agent_id: String,
}

impl Ctx {
    /// The real context the binary runs with: every field read from process
    /// environment exactly once, at startup.
    pub fn from_env() -> Self {
        Self {
            spool_root: spool_root(),
            cache_root: cache_root(),
            fleet_id: fleet_id(),
            agent_id: agent_id(),
        }
    }
}

/// Reject a value that is not safe to join as a single path segment.
///
/// `fleet_id` and `agent_id` come from process environment, which is a step more
/// trusted than hook stdin but still not a literal this crate wrote itself — an
/// operator's misconfigured `.env`, or a value forwarded from somewhere else in the
/// install chain, could carry a `/` or a `..`. `ctxlake-hook`'s `spool.rs` rejects
/// the identical shape of input for the identical reason; see
/// `layout_segments_cannot_escape_their_directory` there for the attack this
/// blocks.
pub fn validate_segment(kind: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{kind} must not be empty"));
    }
    if value == "." || value == ".." {
        return Err(format!("{kind} must not be {value:?}"));
    }
    if value.contains(['/', '\\', '\0']) {
        return Err(format!(
            "{kind} must be a single path segment, got {value:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_segments_pass() {
        assert!(validate_segment("fleet_id", "oxidant").is_ok());
        assert!(validate_segment("agent_id", "cc-01.2").is_ok());
    }

    #[test]
    fn traversal_segments_are_rejected() {
        assert!(validate_segment("fleet_id", "../../etc").is_err());
        assert!(validate_segment("fleet_id", "..").is_err());
        assert!(validate_segment("fleet_id", "a/b").is_err());
        assert!(validate_segment("fleet_id", "").is_err());
    }
}
