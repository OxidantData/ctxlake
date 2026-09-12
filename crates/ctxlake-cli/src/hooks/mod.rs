//! Installers — AGENTS.md invariant 8, and "the most important correctness property
//! in this crate" per the wave-2 brief.
//!
//! Three runtimes, three file formats, but one shared shape: find (or create) an
//! array of hook-command entries for a given event, remove any entry this crate
//! previously added, then append a freshly built one. That "remove ours, then
//! re-add" step — rather than "add only if entirely absent" — is what makes install
//! *idempotent under a changing config*: if `fleet_id` or `agent_id` changes between
//! two `ctxlake install` runs, the second run converges the command line to the new
//! values instead of leaving a stale entry sitting next to a fresh one.
//!
//! Every entry ctxlake adds embeds the literal substring `ctxlake-hook` in its
//! command string — see [`MARKER`] — which is both how a re-run recognizes its own
//! prior entry and how `uninstall` removes *exactly* what `install` added and
//! nothing else, matching the stable-marker approach `docs/runtimes/claude-code.md`
//! already documents.

pub mod claude_code;
pub mod cursor;
pub mod hermes;
mod json_util;

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Every command ctxlake writes into a hook config contains this literal string.
/// Detection is a plain substring search, not a parsed prefix — installers must not
/// assume anything about what comes before or after it in someone's hand-edited
/// command line.
pub const MARKER: &str = "ctxlake-hook";

/// Which agent runtime an install/uninstall targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    ClaudeCode,
    Cursor,
    Hermes,
}

impl Runtime {
    pub const ALL: [Runtime; 3] = [Runtime::ClaudeCode, Runtime::Cursor, Runtime::Hermes];

    pub fn name(self) -> &'static str {
        match self {
            Runtime::ClaudeCode => "claude-code",
            Runtime::Cursor => "cursor",
            Runtime::Hermes => "hermes",
        }
    }

    /// The id `ctxlake-hook` expects as its own second argument (see
    /// `crates/ctxlake-hook/src/adapters/mod.rs::parse_runtime`) — distinct from
    /// [`name`](Self::name), which is this CLI's own `--runtime` spelling.
    pub fn hook_runtime_arg(self) -> &'static str {
        match self {
            Runtime::ClaudeCode => "claude_code",
            Runtime::Cursor => "cursor",
            Runtime::Hermes => "hermes",
        }
    }

    pub fn default_config_path(self) -> PathBuf {
        match self {
            Runtime::ClaudeCode => crate::paths::claude_code_settings_path(),
            Runtime::Cursor => crate::paths::cursor_hooks_path(),
            Runtime::Hermes => crate::paths::hermes_config_path(),
        }
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for Runtime {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "claude-code" | "claude_code" => Ok(Runtime::ClaudeCode),
            "cursor" => Ok(Runtime::Cursor),
            "hermes" => Ok(Runtime::Hermes),
            other => Err(format!(
                "unknown runtime {other:?} (expected claude-code, cursor, or hermes)"
            )),
        }
    }
}

/// The shell command line ctxlake registers for one event.
///
/// `CTXLAKE_FLEET_ID`/`CTXLAKE_AGENT_ID` are inlined into the command string itself,
/// never a separate config field: none of the three hook schemas (Claude Code
/// settings.json, Cursor hooks.json, Hermes config.yaml — see AGENTS.md) has an `env`
/// key, so a shell-style `VAR=value` prefix is the only way to reach
/// `ctxlake-hook::hostinfo`'s environment lookup, which otherwise falls back to
/// `unconfigured-fleet`/`unconfigured-agent` placeholders.
///
/// `event` is passed through verbatim as argv[1] — every call site uses the event
/// name in *that runtime's own vocabulary* (`PostToolUse`, `postToolUse`,
/// `post_tool_call`), matching `ctxlake-hook`'s documented argv contract
/// (`crates/ctxlake-hook/src/main.rs`: argv[1] event, argv[2] runtime id).
pub fn hook_command(fleet_id: &str, agent_id: &str, runtime: Runtime, event: &str) -> String {
    format!(
        "env CTXLAKE_FLEET_ID={} CTXLAKE_AGENT_ID={} ctxlake-hook {event} {}",
        shell_quote(fleet_id),
        shell_quote(agent_id),
        runtime.hook_runtime_arg(),
    )
}

/// POSIX single-quote a string for safe use in a `sh`-interpreted command line.
/// `fleet_id`/`agent_id` are operator-chosen, not attacker input, but a space or `$`
/// in either would otherwise silently split argv or expand — quoting costs nothing
/// and removes the question entirely.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// True if `command` is one ctxlake would have written (see [`MARKER`]).
pub fn is_ours(command: &str) -> bool {
    command.contains(MARKER)
}

/// What one install/uninstall run did, for `--dry-run` and for human-readable
/// summaries. `before`/`after` are the full file contents either side of the change
/// (or identical, for a no-op run) — that is what a text diff needs, and it is also
/// exactly what proves an idempotent re-run really changed nothing byte-for-byte.
#[derive(Debug, Clone)]
pub struct ChangeSet {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: String,
}

impl ChangeSet {
    pub fn changed(&self) -> bool {
        self.before.as_deref() != Some(self.after.as_str())
    }

    /// A minimal line-level diff for `--dry-run`. Not meant to compete with `diff -u`
    /// — the two runtime formats get fully re-serialized (see `hooks/*`'s module
    /// docs on why "byte-identical" applies to entries, not to whitespace), so a
    /// real diff would be mostly noise. This shows insertions and deletions by line
    /// identity, which is enough to see *which* hook entry changed.
    pub fn diff(&self) -> String {
        let before_lines: Vec<&str> = self.before.as_deref().unwrap_or("").lines().collect();
        let after_lines: Vec<&str> = self.after.lines().collect();
        line_diff(&before_lines, &after_lines)
    }
}

/// Classic O(n*m) LCS diff. Hook config files are small (tens to low hundreds of
/// lines), so the quadratic cost is irrelevant here; a real diff crate would be a
/// dependency bought for a `--dry-run` nicety, not for anything correctness-bearing.
fn line_diff(a: &[&str], b: &[&str]) -> String {
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push_str("  ");
            out.push_str(a[i]);
            out.push('\n');
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push_str("- ");
            out.push_str(a[i]);
            out.push('\n');
            i += 1;
        } else {
            out.push_str("+ ");
            out.push_str(b[j]);
            out.push('\n');
            j += 1;
        }
    }
    for line in &a[i..] {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    for line in &b[j..] {
        out.push_str("+ ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Read `path` if it exists; `Ok(None)` for "never created," which every installer
/// treats as "start from an empty document" rather than an error.
pub fn read_if_exists(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Apply a computed [`ChangeSet`]: write a `.bak` of whatever is currently on disk
/// (AGENTS.md invariant 8), then write the new content. A no-op change (`before ==
/// after`) still succeeds without touching the filesystem at all — re-running
/// install twice must not even refresh the `.bak`'s mtime, since nothing changed.
pub fn apply(change: &ChangeSet) -> Result<()> {
    if !change.changed() {
        return Ok(());
    }
    if let Some(parent) = change.path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if let Some(before) = &change.before {
        let bak = backup_path(&change.path);
        std::fs::write(&bak, before)
            .with_context(|| format!("writing backup {}", bak.display()))?;
    }
    std::fs::write(&change.path, &change.after)
        .with_context(|| format!("writing {}", change.path.display()))
}

pub fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Read `path`, merge in ctxlake's hooks for `runtime`, and return the resulting
/// [`ChangeSet`] — computed, not yet applied. Callers decide whether to [`apply`] it
/// or just print [`ChangeSet::diff`] for `--dry-run`.
pub fn plan_install(
    runtime: Runtime,
    path: &Path,
    fleet_id: &str,
    agent_id: &str,
) -> Result<ChangeSet> {
    let before = read_if_exists(path)?;
    let after = match runtime {
        Runtime::ClaudeCode => claude_code::install(before.as_deref(), fleet_id, agent_id)?,
        Runtime::Cursor => cursor::install(before.as_deref(), fleet_id, agent_id)?,
        Runtime::Hermes => hermes::install(before.as_deref(), fleet_id, agent_id)?,
    };
    Ok(ChangeSet {
        path: path.to_path_buf(),
        before,
        after,
    })
}

/// The uninstall counterpart of [`plan_install`] — removes exactly what an install
/// would have added.
pub fn plan_uninstall(runtime: Runtime, path: &Path) -> Result<ChangeSet> {
    let before = read_if_exists(path)?;
    let after = match runtime {
        Runtime::ClaudeCode => claude_code::uninstall(before.as_deref())?,
        Runtime::Cursor => cursor::uninstall(before.as_deref())?,
        Runtime::Hermes => hermes::uninstall(before.as_deref())?,
    };
    Ok(ChangeSet {
        path: path.to_path_buf(),
        before,
        after,
    })
}

/// A quick read of whether ctxlake has already wired into a runtime's config, for
/// `ctxlake doctor`'s runtime-detection section. Counts existing entries so
/// `doctor` can report coexistence ("N entries from other tools") rather than a
/// bare found/not-found.
pub struct RuntimeStatus {
    pub config_exists: bool,
    pub ctxlake_wired: bool,
    /// Hook entries belonging to some other tool, not ctxlake — a nonzero count is
    /// reported as normal coexistence, never as a problem (AGENTS.md invariant 8).
    pub foreign_entries: usize,
}

pub fn detect(runtime: Runtime, path: &Path) -> Result<RuntimeStatus> {
    let Some(text) = read_if_exists(path)? else {
        return Ok(RuntimeStatus {
            config_exists: false,
            ctxlake_wired: false,
            foreign_entries: 0,
        });
    };
    // A file that exists but fails to parse is reported as "found, not wired" rather
    // than propagating a parse error — `doctor` must finish and report on every
    // other check even if one runtime's config is currently broken.
    let ctxlake_wired = text.contains(MARKER);
    let foreign_entries = count_foreign_entries(runtime, &text).unwrap_or(0);
    Ok(RuntimeStatus {
        config_exists: true,
        ctxlake_wired,
        foreign_entries,
    })
}

/// Entries left behind after stripping ctxlake's own = entries some other tool put
/// there. Reuses each runtime's own (already-tested) `uninstall` as the "what would
/// be left" computation, rather than a second, parallel bit of shape-parsing logic
/// that could silently drift from it.
fn count_foreign_entries(runtime: Runtime, text: &str) -> Option<usize> {
    let stripped = match runtime {
        Runtime::ClaudeCode => claude_code::uninstall(Some(text)).ok()?,
        Runtime::Cursor => cursor::uninstall(Some(text)).ok()?,
        Runtime::Hermes => hermes::uninstall(Some(text)).ok()?,
    };
    Some(match runtime {
        Runtime::ClaudeCode => claude_code::count_entries(&stripped),
        Runtime::Cursor => cursor::count_entries(&stripped),
        Runtime::Hermes => hermes::count_entries(&stripped),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_safe_identifiers_alone() {
        assert_eq!(shell_quote("myteam"), "myteam");
        assert_eq!(shell_quote("cc-01"), "cc-01");
    }

    #[test]
    fn shell_quote_escapes_a_single_quote() {
        let q = shell_quote("o'brien");
        assert_eq!(q, "'o'\\''brien'");
    }

    #[test]
    fn shell_quote_wraps_whitespace() {
        assert_eq!(shell_quote("my team"), "'my team'");
    }

    #[test]
    fn hook_command_embeds_fleet_and_agent_and_the_marker() {
        let cmd = hook_command("myteam", "cc-01", Runtime::ClaudeCode, "PostToolUse");
        assert!(cmd.contains("CTXLAKE_FLEET_ID=myteam"));
        assert!(cmd.contains("CTXLAKE_AGENT_ID=cc-01"));
        assert!(cmd.contains("ctxlake-hook PostToolUse claude_code"));
        assert!(is_ours(&cmd));
    }

    #[test]
    fn a_foreign_command_is_never_marked_ours() {
        assert!(!is_ours("some-other-tool --flag --verbose"));
    }

    #[test]
    fn changeset_reports_unchanged_when_before_equals_after() {
        let c = ChangeSet {
            path: PathBuf::from("/tmp/x"),
            before: Some("same".into()),
            after: "same".into(),
        };
        assert!(!c.changed());
    }

    #[test]
    fn apply_writes_a_backup_of_the_prior_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "original").unwrap();
        let change = ChangeSet {
            path: path.clone(),
            before: Some("original".into()),
            after: "modified".into(),
        };
        apply(&change).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "modified");
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            "original"
        );
    }

    #[test]
    fn apply_is_a_true_no_op_when_nothing_changed() {
        // Regression guard: an early draft always rewrote the .bak, which meant a
        // repeated `install` kept refreshing the backup's mtime even though the
        // config itself never moved — noisy, and a false signal that something
        // happened.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "same").unwrap();
        let change = ChangeSet {
            path: path.clone(),
            before: Some("same".into()),
            after: "same".into(),
        };
        apply(&change).unwrap();
        assert!(
            !backup_path(&path).exists(),
            "no-op apply must not write a .bak"
        );
    }

    #[test]
    fn line_diff_marks_additions_and_removals() {
        let d = line_diff(&["a", "b", "c"], &["a", "x", "c"]);
        assert!(d.contains("- b"));
        assert!(d.contains("+ x"));
        assert!(d.contains("  a"));
        assert!(d.contains("  c"));
    }

    #[test]
    fn runtime_round_trips_through_str() {
        for r in Runtime::ALL {
            assert_eq!(r.name().parse::<Runtime>().unwrap(), r);
        }
        assert!("windsurf".parse::<Runtime>().is_err());
    }
}
