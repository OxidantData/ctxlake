//! Core types for ctxlake: the envelope, config, redaction, and hashing.
//!
//! This crate is a dependency of `ctxlake-hook`, which fires on every tool call and
//! must stay under 5ms. Nothing here may pull in an async runtime, an HTTP stack, or
//! an object store — see AGENTS.md invariant 2.

pub mod enrichment;
pub mod envelope;
pub mod hash;
pub mod paths;
pub mod redact;

pub use envelope::{Envelope, EventType, Runtime, SCHEMA_VERSION};
pub use hash::content_hash;
pub use paths::{cache_root, fleet_cache_dir, spool_root};
pub use redact::{RedactionOutcome, Redactor};
