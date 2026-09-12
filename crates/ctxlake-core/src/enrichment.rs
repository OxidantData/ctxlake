//! What a session's transcript knows that its capture hooks could not see.
//!
//! A data shape only — the reader that produces one lives in `ctxlake-cli`
//! (`import::claude_code`), because parsing a transcript needs the redactor and the
//! adapter helpers. It lives here because two crates that cannot see each other both
//! need the type: the daemon writes it at seal time, and `ctxlake-maint` reads it back
//! when computing a digest, possibly on a different machine entirely.
//!
//! Written to `layout::session_enrichment` as a sibling of the session's segments
//! rather than merged into them: `sessions/` is append-only, and enrichment that
//! arrives after the segments must not mean rewriting immutable bronze.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::envelope::Usage;

/// The current shape. Bumped when a reader change would misread older objects.
pub const SCHEMA_VERSION: u32 = 1;

/// What the transcript knows about one tool call.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolOutcome {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// `Some(1)` when the runtime marked the call failed, `Some(0)` when it did not,
    /// `None` when it said nothing.
    ///
    /// Not a real exit status — the transcript records a boolean, and inventing a
    /// number the runtime never sent would repeat the mistake this whole path exists to
    /// correct. Consumers only ever ask whether it is non-zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Files this call edited, **including the ones edited through a shell command**,
    /// which no hook payload can attribute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// Everything one transcript contributes to its session's digest.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Enrichment {
    pub schema_version: u32,
    /// Keyed by the runtime's tool-use id, which is the envelope's `message_id`. That
    /// join key is already captured; nothing new had to be recorded to connect the two.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `clean` / `redacted` / `quarantined`, recorded so the lake states that this
    /// object went through the scrubber instead of leaving it to be assumed.
    pub redaction_status: String,
}

impl Enrichment {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.usage.is_none() && self.branch.is_none()
    }
}
