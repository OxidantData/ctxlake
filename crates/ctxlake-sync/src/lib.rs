//! ctxlake-sync — the daemon. See AGENTS.md invariant 1 and
//! `docs/architecture.md`'s component inventory: this is the *only* thing that
//! talks to the object store on a schedule, precisely so `ctxlake-hook` and
//! `ctxlake-mcp` never have to. Referred to in the docs as "`ctxlake sync`" (the
//! `sync` subcommand of the `ctxlake` binary) — this crate is the library behind
//! that subcommand, the same relationship `ctxlake-maint` has to `ctxlake maint`.
//!
//! Two loops, plus a third for this agent's own presence:
//!
//! - [`upload`] — hook spool -> `sessions/` bronze. The durability boundary: a
//!   spool line is never dropped until its `PUT` is confirmed (see that module's
//!   doc for exactly how `kill -9` resumability follows from that one rule).
//! - [`cache`] — object store -> local cache (`roster.json`, a generic
//!   content-addressed `snapshot.bin`) that `ctxlake-hook` and `ctxlake-mcp` read
//!   with no network of their own.
//! - [`presence`] — this agent's own `live/agents/<id>.json` heartbeat, and
//!   opportunistic `live/roster.json` maintenance when nobody else currently holds
//!   that role.
//!
//! [`daemon::Daemon`] runs all three under one shutdown signal.

pub mod atomic_file;
pub mod backoff;
pub mod cache;
pub mod codec;
pub mod daemon;
pub mod presence;
pub mod upload;
pub mod watermark;

pub use daemon::{Daemon, DaemonConfig};
