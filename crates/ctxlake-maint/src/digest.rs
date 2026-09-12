//! Tier 0 — the structural session digest. See `docs/memory.md`.
//!
//! Everything in this module is arithmetic over envelopes that were already
//! captured, redacted, and sealed. There is no LLM call anywhere in this file, on
//! purpose: counting exit codes, counting how many times a path was touched, and
//! measuring the gap between the first and last `emitted_at` cannot hallucinate,
//! costs nothing to run, and — the whole reason Tier 0 is "always on" rather than
//! optional — is never wrong. The most valuable thing this module produces is the
//! friction signal: "abandoned after 4 failed `cargo test -p oxidant-connect`
//! runs" is not a guess, it is four exit codes and a missing `SessionEnd`.
//!
//! One thing worth being explicit about: exit codes arrive on
//! `Envelope.tool.exit_code` regardless of which runtime produced the envelope —
//! Claude Code, Cursor (`tool_output.exitCode`, per AGENTS.md's hard-won facts) and
//! Hermes all normalize into the same field in `ctxlake-hook`'s adapters. This
//! module reads that one field and nothing runtime-specific, so it cannot silently
//! go blind for one runtime the way a check keyed on a per-runtime tool name could
//! — see `friction_detection_works_identically_for_a_cursor_shaped_session` below.

use std::collections::BTreeMap;

use ctxlake_core::envelope::{Envelope, EventType, ToolCall};
use ctxlake_core::Runtime;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use serde::{Deserialize, Serialize};

use crate::error::MaintError;
use crate::partition::{parse_session_partition, read_session_segments};

/// Bumped when a field is added to [`SessionDigest`]. Same discipline as
/// `ctxlake_core::envelope::SCHEMA_VERSION`: never reinterpret an existing field,
/// because a digest already written is exactly as immutable as the sealed session it
/// was folded from.
pub const DIGEST_SCHEMA_VERSION: u32 = 1;

/// One tool call whose envelope carried an exit code — the operational definition of
/// "a command" this module uses, deliberately independent of any specific tool name
/// (`Bash`, `Terminal`, ...) so it works the same across every runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandRun {
    pub tool: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub is_test: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Clean,
    Error,
    Abandoned,
}

/// A friction signal — pure arithmetic over exit codes and path touches, per the
/// module doc. `kind` is tagged explicitly (rather than left to serde's untagged
/// guessing) so a reader of raw digest JSON can filter by kind without first parsing
/// every variant's shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Friction {
    /// The same exact command text failed at least `count` times somewhere in the
    /// session — not necessarily consecutively.
    RepeatedFailure { command: String, count: u32 },
    /// A path appeared in `tool.paths` more than `edit_count` times. Named `HotFile`
    /// to match `docs/memory.md`'s wording, but honestly: this counts every
    /// tool call that *named* the path, not only calls known to be a write —
    /// `ToolCall` carries no is-write flag, and guessing per-runtime tool names here
    /// would reintroduce exactly the runtime-specific coupling this module's
    /// exit-code check avoids. It overcounts plain reads of a hot file; it never
    /// undercounts real edit churn, which is the direction that actually matters for
    /// flagging thrash.
    HotFile { path: String, edit_count: u32 },
    /// The session's last `count` commands were all the same failing command, and no
    /// `SessionEnd` ever arrived — the signal `docs/memory.md` singles out by
    /// name: "abandoned after 4 failed `cargo test -p oxidant-connect` runs".
    AbandonedAfterFailures { command: String, count: u32 },
}

impl Friction {
    /// A human-readable line for a briefing — the exact phrasing
    /// `docs/memory.md` uses for its own example, so that example is also
    /// this function's regression test.
    pub fn headline(&self) -> String {
        match self {
            Friction::RepeatedFailure { command, count } => {
                format!("`{command}` failed {count} times")
            }
            Friction::HotFile { path, edit_count } => {
                format!("`{path}` touched {edit_count} times")
            }
            Friction::AbandonedAfterFailures { command, count } => {
                format!("abandoned after {count} failed `{command}` runs")
            }
        }
    }
}

/// Token and cost accounting, summed across every envelope in the session that
/// carried a [`ctxlake_core::envelope::Usage`] — "read straight from the runtime's
/// own accounting" per `docs/memory.md`, never estimated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
}

/// The Tier 0 digest of one sealed session. See the module doc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionDigest {
    pub schema_version: u32,
    pub session_id: String,
    pub fleet_id: String,
    pub agent_id: String,
    pub runtime: Runtime,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_ms: Option<i64>,
    /// Count of `EventType::Prompt` events — each user prompt starts one turn. Not
    /// "assistant replies," because a runtime that never emits a distinct
    /// turn-boundary event for its own replies (see `envelope.rs`'s note on Claude
    /// Code's `Stop`) would otherwise undercount by a variable, runtime-dependent
    /// amount; prompts are captured identically everywhere.
    pub turn_count: u32,
    pub files_touched: Vec<String>,
    pub commands: Vec<CommandRun>,
    /// The subset of `commands` this module's best-effort heuristic recognizes as a
    /// test run (`cargo test`, `pytest`, ...) — see `looks_like_a_test`. Unlike exit
    /// codes, this classification *can* be wrong (an unrecognized test runner, a
    /// command that merely mentions "test"), so treat it as a hint, not a fact.
    pub tests_run: Vec<CommandRun>,
    /// Bash commands recognized as `git commit ...` by their command text.
    pub commits: Vec<String>,
    pub git_sha_before: Option<String>,
    pub git_sha_after: Option<String>,
    pub usage: TokenUsage,
    pub outcome: Outcome,
    pub friction: Vec<Friction>,
}

/// Tunable friction thresholds. Kept as an explicit, overridable struct rather than
/// hardcoded constants so a golden test can exercise a threshold boundary without
/// needing dozens of synthetic events to reach a production-sized default.
#[derive(Debug, Clone, Copy)]
pub struct FrictionThresholds {
    /// A command must fail at least this many times (anywhere in the session, not
    /// necessarily consecutively) to be flagged as [`Friction::RepeatedFailure`].
    pub repeated_failure_min: u32,
    /// A path must be touched *more than* this many times to be flagged as
    /// [`Friction::HotFile`].
    pub hot_file_edit_min: u32,
    /// The trailing same-command failure streak must be at least this long, with no
    /// `SessionEnd` in the session, to be flagged as
    /// [`Friction::AbandonedAfterFailures`].
    pub abandon_trailing_failures_min: u32,
}

impl Default for FrictionThresholds {
    fn default() -> Self {
        Self {
            repeated_failure_min: 2,
            hot_file_edit_min: 5,
            abandon_trailing_failures_min: 3,
        }
    }
}

const TEST_MARKERS: &[&str] = &[
    "cargo test",
    "pytest",
    "py.test",
    "npm test",
    "npm run test",
    "yarn test",
    "go test",
    "jest",
    "mvn test",
    "make test",
    "gradle test",
    "rspec",
    "phpunit",
];

/// Best-effort: recognizes common invocations, nothing more. See
/// [`SessionDigest::tests_run`]'s doc for why this is a hint, not a guarantee.
fn looks_like_a_test(command: &str) -> bool {
    let lower = command.to_lowercase();
    TEST_MARKERS.iter().any(|m| lower.contains(m))
}

/// The human-meaningful text of a tool call: the `command` field of a JSON-shaped
/// input (what `Bash`-like tools send), falling back to the raw input string, falling
/// back to just the tool's name. Never fails — a tool call with no input at all is
/// still "a command," identified by name alone.
fn command_text(tool: &ToolCall) -> String {
    if let Some(input) = &tool.input {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(input) {
            if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                return cmd.to_string();
            }
        }
        return input.clone();
    }
    tool.name.clone()
}

fn parse_rfc3339(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

/// Find the longest run, ending at the very last command, where every command in
/// that run shares the same command text and failed. `None` if there are no
/// commands or the last one didn't fail — a session cannot be "abandoned after N
/// failures" if the thing it most recently did succeeded.
fn trailing_same_command_failure_streak(commands: &[CommandRun]) -> Option<(&str, u32)> {
    let last = commands.last()?;
    if !matches!(last.exit_code, Some(code) if code != 0) {
        return None;
    }
    let cmd = last.command.as_str();
    let mut count = 0u32;
    for c in commands.iter().rev() {
        if c.command == cmd && matches!(c.exit_code, Some(code) if code != 0) {
            count += 1;
        } else {
            break;
        }
    }
    Some((cmd, count))
}

/// Compute the Tier 0 digest of one sealed session.
///
/// `session_id`/`fleet_id`/`agent_id`/`runtime` are supplied by the caller (who
/// already knows them from the `_SEALED` marker's own path) rather than derived from
/// `envelopes`, because a legitimately empty-but-sealed session (an agent that
/// exited before any tool call landed — `ctxlake-sync`'s `upload.rs` documents this
/// as a real case it seals) has no envelope to derive them from at all.
///
/// `envelopes` need not be pre-sorted; this function sorts a local copy by
/// `event_id`, which AGENTS.md and `envelope.rs` guarantee is strictly monotonic
/// within one session's single writer — the ordering every calculation here
/// (duration, turn count, the trailing failure streak) depends on.
pub fn compute(
    session_id: &str,
    fleet_id: &str,
    agent_id: &str,
    runtime: Runtime,
    envelopes: &[Envelope],
    thresholds: &FrictionThresholds,
) -> SessionDigest {
    let mut sorted: Vec<&Envelope> = envelopes.iter().collect();
    sorted.sort_by(|a, b| a.event_id.cmp(&b.event_id));

    let started_at = sorted.first().map(|e| e.emitted_at.clone());
    let ended_at = sorted.last().map(|e| e.emitted_at.clone());
    let duration_ms = match (&started_at, &ended_at) {
        (Some(a), Some(b)) => match (parse_rfc3339(a), parse_rfc3339(b)) {
            (Some(a), Some(b)) => Some((b - a).whole_milliseconds() as i64),
            _ => None,
        },
        _ => None,
    };

    let turn_count = sorted
        .iter()
        .filter(|e| e.event_type == EventType::Prompt)
        .count() as u32;

    let has_session_end = sorted.iter().any(|e| e.event_type == EventType::SessionEnd);

    let mut files_touched: std::collections::BTreeSet<String> = Default::default();
    let mut commands: Vec<CommandRun> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut git_sha_before: Option<String> = None;
    let mut git_sha_after: Option<String> = None;

    for e in &sorted {
        if let Some(sha) = &e.git_sha {
            if git_sha_before.is_none() {
                git_sha_before = Some(sha.clone());
            }
            git_sha_after = Some(sha.clone());
        }
        if let Some(u) = &e.usage {
            usage.input_tokens += u.input_tokens;
            usage.output_tokens += u.output_tokens;
            usage.cache_read_tokens += u.cache_read_tokens;
            usage.cache_write_tokens += u.cache_write_tokens;
            usage.cost_usd += u.cost_usd.unwrap_or(0.0);
        }
        if let Some(tool) = &e.tool {
            for p in &tool.paths {
                files_touched.insert(p.clone());
            }
            // "Commands" are tool calls the runtime reported an exit code for — see
            // the module doc for why that's the runtime-agnostic definition, rather
            // than matching a per-runtime tool name.
            if let Some(exit_code) = tool.exit_code {
                let text = command_text(tool);
                commands.push(CommandRun {
                    tool: tool.name.clone(),
                    is_test: looks_like_a_test(&text),
                    command: text,
                    exit_code: Some(exit_code),
                });
            }
        }
    }

    let commits: Vec<String> = commands
        .iter()
        .filter(|c| {
            c.command
                .trim_start()
                .to_lowercase()
                .starts_with("git commit")
        })
        .map(|c| c.command.clone())
        .collect();
    let tests_run: Vec<CommandRun> = commands.iter().filter(|c| c.is_test).cloned().collect();

    let mut friction = Vec::new();

    // Abandonment: the single highest-value signal (docs/memory.md names it
    // explicitly). Computed before repeated-failure so the latter can avoid
    // reporting the same fact twice — see below.
    let abandonment = trailing_same_command_failure_streak(&commands).and_then(|(cmd, count)| {
        (!has_session_end && count >= thresholds.abandon_trailing_failures_min)
            .then(|| (cmd.to_string(), count))
    });
    if let Some((command, count)) = &abandonment {
        friction.push(Friction::AbandonedAfterFailures {
            command: command.clone(),
            count: *count,
        });
    }

    let mut fail_counts: BTreeMap<String, u32> = BTreeMap::new();
    for c in &commands {
        if matches!(c.exit_code, Some(code) if code != 0) {
            *fail_counts.entry(c.command.clone()).or_insert(0) += 1;
        }
    }
    let abandoned_command = abandonment.as_ref().map(|(cmd, _)| cmd.clone());
    let mut repeated: Vec<(String, u32)> = fail_counts
        .into_iter()
        .filter(|(command, count)| {
            *count >= thresholds.repeated_failure_min && Some(command.clone()) != abandoned_command
        })
        .collect();
    // Deterministic order: most frequent first, ties broken lexicographically so
    // re-running this over the same sealed data always emits bytes in the same
    // order (the digest key is a plain overwrite, not a CAS — see layout.rs).
    repeated.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    friction.extend(
        repeated
            .into_iter()
            .map(|(command, count)| Friction::RepeatedFailure { command, count }),
    );

    let mut path_counts: BTreeMap<String, u32> = BTreeMap::new();
    for e in &sorted {
        if let Some(tool) = &e.tool {
            for p in &tool.paths {
                *path_counts.entry(p.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut hot: Vec<(String, u32)> = path_counts
        .into_iter()
        .filter(|(_, count)| *count > thresholds.hot_file_edit_min)
        .collect();
    hot.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    friction.extend(
        hot.into_iter()
            .map(|(path, edit_count)| Friction::HotFile { path, edit_count }),
    );

    let outcome = if abandonment.is_some() {
        Outcome::Abandoned
    } else if matches!(commands.last().and_then(|c| c.exit_code), Some(code) if code != 0) {
        Outcome::Error
    } else {
        Outcome::Clean
    };

    SessionDigest {
        schema_version: DIGEST_SCHEMA_VERSION,
        session_id: session_id.to_string(),
        fleet_id: fleet_id.to_string(),
        agent_id: agent_id.to_string(),
        runtime,
        started_at,
        ended_at,
        duration_ms,
        turn_count,
        files_touched: files_touched.into_iter().collect(),
        commands,
        tests_run,
        commits,
        git_sha_before,
        git_sha_after,
        usage,
        outcome,
        friction,
    }
}

/// What one [`run_for_session`] call actually did.
#[derive(Debug, Clone, PartialEq)]
pub enum DigestOutcome {
    Written(Box<SessionDigest>),
    /// A digest already existed at this session's key. Recomputing it would produce
    /// byte-identical output (the sealed segments it's folded from are immutable),
    /// so this is a cheap, correct skip, not a missed update.
    Skipped,
}

/// List every fleet-scoped `_SEALED` marker under `sessions/` — every sealed session
/// that might still need a digest. Scans the whole tree on every call rather than
/// tracking a watermark of "sessions already digested": simpler, and correct (a
/// digest write is itself the record of "already done," checked in
/// [`run_for_session`]), at the cost of a LIST over the full history every
/// maintenance run — an honest tradeoff to revisit if `docs/storage.md`'s
/// small-object-explosion arithmetic ever makes that LIST itself the bottleneck.
pub async fn discover_sealed_sessions(
    store: &dyn ObjectStore,
    fleet_id: &str,
) -> Result<Vec<Path>, MaintError> {
    use futures::StreamExt;
    let mut stream = store.list(Some(&Path::from("sessions")));
    let mut markers = Vec::new();
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        let loc = meta.location;
        if !loc.as_ref().ends_with("/_SEALED") {
            continue;
        }
        if let Some(p) = parse_session_partition(&loc) {
            if p.fleet_id == fleet_id {
                markers.push(loc);
            }
        }
    }
    markers.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
    Ok(markers)
}

/// Compute and write the digest for the one session named by `sealed_marker`, or
/// report that one already exists. See the module doc for why the identity fields
/// come from the marker's own path rather than from the envelopes it might decode
/// to zero of.
pub async fn run_for_session(
    store: &dyn ObjectStore,
    sealed_marker: &Path,
    thresholds: &FrictionThresholds,
) -> Result<DigestOutcome, MaintError> {
    let partition = parse_session_partition(sealed_marker)
        .ok_or_else(|| MaintError::Other(format!("unparseable sealed marker: {sealed_marker}")))?;
    let digest_key = ctxlake_store::layout::session_digest(
        &partition.date,
        &partition.fleet_id,
        partition.runtime,
        &partition.agent_id,
        &partition.session_id,
    );
    if store.head(&digest_key).await.is_ok() {
        return Ok(DigestOutcome::Skipped);
    }

    let envelopes = read_session_segments(store, sealed_marker).await?;
    let digest = compute(
        &partition.session_id,
        &partition.fleet_id,
        &partition.agent_id,
        partition.runtime,
        &envelopes,
        thresholds,
    );
    // Plain overwrite, not CAS: a digest is a pure function of immutable sealed
    // data, so a concurrent recompute (two maintenance hosts racing over the same
    // session, or a retry after a crash) always produces the same bytes — "two
    // writers" racing to write identical content is not the hazard AGENTS.md
    // invariant 3 is about.
    store
        .put(
            &digest_key,
            PutPayload::from(serde_json::to_vec_pretty(&digest)?),
        )
        .await?;
    Ok(DigestOutcome::Written(Box::new(digest)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::envelope::{Redaction, Usage};

    fn base(session: &str, n: u32, event_type: EventType, runtime: Runtime) -> Envelope {
        let mut e = Envelope::new(
            "oxidant",
            "cc-01",
            runtime,
            session,
            event_type,
            format!("2026-09-11T18:{:02}:00.000Z", n % 60),
        );
        // Force strictly increasing, sortable event_ids matching call order — real
        // envelopes get this from `next_event_id()`; tests need it deterministic
        // and tied to the intended sequence instead.
        e.event_id = format!("{n:026}");
        e
    }

    fn failing_tool_call(
        session: &str,
        n: u32,
        runtime: Runtime,
        command: &str,
        exit_code: i32,
        path: Option<&str>,
    ) -> Envelope {
        let mut e = base(session, n, EventType::ToolCall, runtime);
        e.tool = Some(ToolCall {
            name: "Bash".to_string(),
            input: Some(serde_json::json!({"command": command}).to_string()),
            input_hash: ctxlake_core::hash::content_hash(command),
            result: None,
            exit_code: Some(exit_code),
            duration_ms: None,
            paths: path.map(|p| vec![p.to_string()]).unwrap_or_default(),
        });
        e.redaction = Redaction {
            status: "clean".into(),
            rules_fired: vec![],
        };
        e
    }

    #[test]
    fn four_failing_runs_of_the_same_command_with_no_session_end_is_abandoned() {
        // The exact example from docs/memory.md: "abandoned after 4 failed
        // `cargo test -p oxidant-connect` runs" is pure arithmetic over exit codes.
        let envelopes: Vec<Envelope> = (0..4)
            .map(|n| {
                failing_tool_call(
                    "sess-1",
                    n,
                    Runtime::ClaudeCode,
                    "cargo test -p oxidant-connect",
                    101,
                    None,
                )
            })
            .collect();
        // No SessionEnd: the agent never came back.
        let digest = compute(
            "sess-1",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &envelopes,
            &FrictionThresholds::default(),
        );
        assert_eq!(digest.outcome, Outcome::Abandoned);
        let headline = digest
            .friction
            .iter()
            .find(|f| matches!(f, Friction::AbandonedAfterFailures { .. }))
            .expect("expected an AbandonedAfterFailures friction signal")
            .headline();
        assert_eq!(
            headline,
            "abandoned after 4 failed `cargo test -p oxidant-connect` runs"
        );
    }

    #[test]
    fn four_succeeding_runs_of_the_same_command_produce_no_abandonment_line() {
        // The negative half of the golden test the task brief asks for: identical
        // shape, exit code 0 instead of 101, must produce neither the abandonment
        // line nor a repeated-failure line.
        let envelopes: Vec<Envelope> = (0..4)
            .map(|n| {
                failing_tool_call(
                    "sess-2",
                    n,
                    Runtime::ClaudeCode,
                    "cargo test -p oxidant-connect",
                    0,
                    None,
                )
            })
            .collect();
        let digest = compute(
            "sess-2",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &envelopes,
            &FrictionThresholds::default(),
        );
        assert_eq!(digest.outcome, Outcome::Clean);
        assert!(
            digest.friction.is_empty(),
            "a session with only succeeding runs must have no friction signals: {:?}",
            digest.friction
        );
    }

    #[test]
    fn friction_detection_works_identically_for_a_cursor_shaped_session() {
        // Regression guard named directly in the task brief: Cursor's exit codes
        // arrive via tool_output.exitCode (AGENTS.md's hard-won facts), and if this
        // module ever grew a runtime-specific check it would silently see no exit
        // codes for Cursor. It doesn't — it reads `tool.exit_code`, which the
        // adapter already normalized — so the identical scenario must produce the
        // identical digest regardless of which `Runtime` produced the envelopes.
        let envelopes: Vec<Envelope> = (0..4)
            .map(|n| {
                failing_tool_call(
                    "sess-3",
                    n,
                    Runtime::Cursor,
                    "cargo test -p oxidant-connect",
                    101,
                    None,
                )
            })
            .collect();
        let digest = compute(
            "sess-3",
            "oxidant",
            "cur-01",
            Runtime::Cursor,
            &envelopes,
            &FrictionThresholds::default(),
        );
        assert_eq!(digest.outcome, Outcome::Abandoned);
        assert!(digest
            .friction
            .iter()
            .any(|f| matches!(f, Friction::AbandonedAfterFailures { count: 4, .. })));
    }

    #[test]
    fn a_file_edited_past_the_threshold_is_flagged_hot() {
        let mut envelopes: Vec<Envelope> = Vec::new();
        for n in 0..6 {
            let mut e = base("sess-4", n, EventType::ToolCall, Runtime::ClaudeCode);
            e.tool = Some(ToolCall {
                name: "Edit".to_string(),
                input: Some("{}".to_string()),
                input_hash: ctxlake_core::hash::content_hash("{}"),
                result: None,
                exit_code: None,
                duration_ms: None,
                paths: vec!["crates/foo/src/lib.rs".to_string()],
            });
            envelopes.push(e);
        }
        let digest = compute(
            "sess-4",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &envelopes,
            &FrictionThresholds::default(), // hot_file_edit_min: 5, we touched it 6 times
        );
        assert!(digest.friction.iter().any(
            |f| matches!(f, Friction::HotFile { path, edit_count: 6 } if path == "crates/foo/src/lib.rs")
        ));
    }

    #[test]
    fn a_clean_session_end_after_success_is_never_flagged_abandoned() {
        let mut envelopes = vec![failing_tool_call(
            "sess-5",
            0,
            Runtime::ClaudeCode,
            "cargo build",
            0,
            None,
        )];
        envelopes.push(base(
            "sess-5",
            1,
            EventType::SessionEnd,
            Runtime::ClaudeCode,
        ));
        let digest = compute(
            "sess-5",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &envelopes,
            &FrictionThresholds::default(),
        );
        assert_eq!(digest.outcome, Outcome::Clean);
    }

    #[test]
    fn a_session_end_after_a_failure_is_error_not_abandoned() {
        // Ended cleanly (SessionEnd exists), but the last thing that happened
        // failed — that's a broken close, not a vanished agent.
        let mut envelopes = vec![failing_tool_call(
            "sess-6",
            0,
            Runtime::ClaudeCode,
            "cargo build",
            1,
            None,
        )];
        envelopes.push(base(
            "sess-6",
            1,
            EventType::SessionEnd,
            Runtime::ClaudeCode,
        ));
        let digest = compute(
            "sess-6",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &envelopes,
            &FrictionThresholds::default(),
        );
        assert_eq!(digest.outcome, Outcome::Error);
    }

    #[test]
    fn a_session_end_after_a_long_failure_streak_is_error_not_abandoned() {
        // The other boundary the previous test doesn't reach: a streak long
        // enough to cross the abandonment threshold on its own (4 >= the default
        // 3), but the session *did* come back and close properly. `SessionEnd`
        // must still veto `Abandoned` regardless of how long the trailing streak
        // is — abandonment means "never came back," not "failed a lot."
        let mut envelopes: Vec<Envelope> = (0..4)
            .map(|n| {
                failing_tool_call(
                    "sess-11",
                    n,
                    Runtime::ClaudeCode,
                    "cargo test -p oxidant-connect",
                    101,
                    None,
                )
            })
            .collect();
        envelopes.push(base(
            "sess-11",
            4,
            EventType::SessionEnd,
            Runtime::ClaudeCode,
        ));
        let digest = compute(
            "sess-11",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &envelopes,
            &FrictionThresholds::default(),
        );
        assert_eq!(digest.outcome, Outcome::Error);
        assert!(
            !digest
                .friction
                .iter()
                .any(|f| matches!(f, Friction::AbandonedAfterFailures { .. })),
            "a session that closed cleanly must never be reported as abandoned, \
             no matter how long the trailing failure streak was: {:?}",
            digest.friction
        );
    }

    #[test]
    fn an_empty_session_produces_a_valid_clean_digest() {
        // The legitimate zero-event sealed session ctxlake-sync's upload.rs
        // documents (an agent that exited before any tool call landed).
        let digest = compute(
            "sess-empty",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &[],
            &FrictionThresholds::default(),
        );
        assert_eq!(digest.outcome, Outcome::Clean);
        assert!(digest.commands.is_empty());
        assert!(digest.friction.is_empty());
        assert_eq!(digest.turn_count, 0);
    }

    #[test]
    fn commits_are_recognized_from_git_commit_commands() {
        let envelopes = vec![failing_tool_call(
            "sess-7",
            0,
            Runtime::ClaudeCode,
            "git commit -m 'fix: thing'",
            0,
            None,
        )];
        let digest = compute(
            "sess-7",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &envelopes,
            &FrictionThresholds::default(),
        );
        assert_eq!(
            digest.commits,
            vec!["git commit -m 'fix: thing'".to_string()]
        );
    }

    #[test]
    fn token_usage_sums_across_every_envelope_that_carries_it() {
        let mut e1 = base("sess-8", 0, EventType::Assistant, Runtime::ClaudeCode);
        e1.usage = Some(Usage {
            model: Some("claude-sonnet".into()),
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 0,
            cost_usd: Some(0.01),
        });
        let mut e2 = base("sess-8", 1, EventType::Assistant, Runtime::ClaudeCode);
        e2.usage = Some(Usage {
            model: Some("claude-sonnet".into()),
            input_tokens: 200,
            output_tokens: 75,
            cache_read_tokens: 0,
            cache_write_tokens: 5,
            cost_usd: Some(0.02),
        });
        let digest = compute(
            "sess-8",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &[e1, e2],
            &FrictionThresholds::default(),
        );
        assert_eq!(digest.usage.input_tokens, 300);
        assert_eq!(digest.usage.output_tokens, 125);
        assert_eq!(digest.usage.cache_read_tokens, 10);
        assert_eq!(digest.usage.cache_write_tokens, 5);
        assert!((digest.usage.cost_usd - 0.03).abs() < 1e-9);
    }

    #[test]
    fn abandonment_suppresses_the_redundant_repeated_failure_line_for_the_same_command() {
        // The same fact ("cargo test failed 4 times") should not appear twice in one
        // digest's friction list once the more informative abandonment line already
        // says it.
        let envelopes: Vec<Envelope> = (0..4)
            .map(|n| failing_tool_call("sess-9", n, Runtime::ClaudeCode, "cargo test", 101, None))
            .collect();
        let digest = compute(
            "sess-9",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &envelopes,
            &FrictionThresholds::default(),
        );
        let repeated_for_cargo_test = digest
            .friction
            .iter()
            .filter(|f| matches!(f, Friction::RepeatedFailure { command, .. } if command == "cargo test"))
            .count();
        assert_eq!(
            repeated_for_cargo_test, 0,
            "abandonment already reports this command's failures; a duplicate RepeatedFailure line is noise: {:?}",
            digest.friction
        );
    }

    #[test]
    fn out_of_order_input_is_sorted_by_event_id_before_anything_is_computed() {
        // Envelopes can arrive in any order this function is called with (a caller
        // decoding several Parquet segments and concatenating them, say) — only
        // event_id order is trustworthy (AGENTS.md: monotonic within one session's
        // single writer). Feed them in reverse and assert the trailing-failure
        // streak is still computed against the *logical* last command, not
        // whichever happens to be last in the slice.
        let mut envelopes: Vec<Envelope> = (0..4)
            .map(|n| failing_tool_call("sess-10", n, Runtime::ClaudeCode, "flaky", 1, None))
            .collect();
        envelopes.reverse();
        let digest = compute(
            "sess-10",
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            &envelopes,
            &FrictionThresholds::default(),
        );
        assert_eq!(digest.outcome, Outcome::Abandoned);
    }

    mod store_integration {
        use super::*;
        use object_store::memory::InMemory;

        async fn seal(
            store: &dyn ObjectStore,
            date: &str,
            fleet: &str,
            runtime: Runtime,
            agent: &str,
            session: &str,
            envelopes: &[Envelope],
        ) {
            if !envelopes.is_empty() {
                let bytes = ctxlake_sync::codec::encode(envelopes).unwrap();
                let seg =
                    ctxlake_store::layout::session_segment(date, fleet, runtime, agent, session, 0);
                store.put(&seg, PutPayload::from(bytes)).await.unwrap();
            }
            let sealed =
                ctxlake_store::layout::session_sealed(date, fleet, runtime, agent, session);
            store
                .put(&sealed, PutPayload::from_static(b"{}"))
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn run_for_session_writes_a_digest_reflecting_its_sealed_data() {
            let store = InMemory::new();
            let envelopes: Vec<Envelope> = (0..4)
                .map(|n| {
                    failing_tool_call("sess-1", n, Runtime::ClaudeCode, "cargo test", 101, None)
                })
                .collect();
            seal(
                &store,
                "2026-09-11",
                "oxidant",
                Runtime::ClaudeCode,
                "cc-01",
                "sess-1",
                &envelopes,
            )
            .await;

            let markers = discover_sealed_sessions(&store, "oxidant").await.unwrap();
            assert_eq!(markers.len(), 1);
            let outcome = run_for_session(&store, &markers[0], &FrictionThresholds::default())
                .await
                .unwrap();
            let DigestOutcome::Written(digest) = outcome else {
                panic!("expected a freshly written digest");
            };
            assert_eq!(digest.outcome, Outcome::Abandoned);

            let digest_key = ctxlake_store::layout::session_digest(
                "2026-09-11",
                "oxidant",
                Runtime::ClaudeCode,
                "cc-01",
                "sess-1",
            );
            let stored = store.get(&digest_key).await.unwrap().bytes().await.unwrap();
            let stored_digest: SessionDigest = serde_json::from_slice(&stored).unwrap();
            assert_eq!(stored_digest, *digest);
        }

        #[tokio::test]
        async fn a_second_run_over_the_same_session_is_skipped() {
            let store = InMemory::new();
            seal(
                &store,
                "2026-09-11",
                "oxidant",
                Runtime::ClaudeCode,
                "cc-01",
                "sess-1",
                &[],
            )
            .await;
            let markers = discover_sealed_sessions(&store, "oxidant").await.unwrap();
            let first = run_for_session(&store, &markers[0], &FrictionThresholds::default())
                .await
                .unwrap();
            assert!(matches!(first, DigestOutcome::Written(_)));
            let second = run_for_session(&store, &markers[0], &FrictionThresholds::default())
                .await
                .unwrap();
            assert!(matches!(second, DigestOutcome::Skipped));
        }

        #[tokio::test]
        async fn discover_sealed_sessions_only_returns_the_requested_fleet() {
            let store = InMemory::new();
            seal(
                &store,
                "2026-09-11",
                "oxidant",
                Runtime::ClaudeCode,
                "cc-01",
                "sess-1",
                &[],
            )
            .await;
            seal(
                &store,
                "2026-09-11",
                "other-fleet",
                Runtime::ClaudeCode,
                "cc-02",
                "sess-2",
                &[],
            )
            .await;
            let markers = discover_sealed_sessions(&store, "oxidant").await.unwrap();
            assert_eq!(markers.len(), 1);
        }

        #[tokio::test]
        async fn an_empty_but_sealed_session_still_gets_a_digest() {
            // ctxlake-sync's upload.rs documents this as a real case: an agent that
            // exits before any tool call landed still seals. There is no envelope to
            // derive identity from — it must come from the marker's own path.
            let store = InMemory::new();
            seal(
                &store,
                "2026-09-11",
                "oxidant",
                Runtime::Hermes,
                "hm-01",
                "sess-empty",
                &[],
            )
            .await;
            let markers = discover_sealed_sessions(&store, "oxidant").await.unwrap();
            let outcome = run_for_session(&store, &markers[0], &FrictionThresholds::default())
                .await
                .unwrap();
            let DigestOutcome::Written(digest) = outcome else {
                panic!("expected a written digest even for zero envelopes");
            };
            assert_eq!(digest.session_id, "sess-empty");
            assert_eq!(digest.agent_id, "hm-01");
            assert_eq!(digest.runtime, Runtime::Hermes);
            assert_eq!(digest.outcome, Outcome::Clean);
        }
    }
}
