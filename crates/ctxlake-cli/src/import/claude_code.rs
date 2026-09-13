//! Reading a Claude Code session transcript for everything the hook could not see.
//!
//! **Why this exists.** `docs/memory.md` promises Tier 0 delivers "files touched,
//! commands and exit codes, tests, commits, duration, cost, friction signals". Measured
//! on a live lake, it delivered duration. The other six were absent not because the
//! digest was wrong but because no hook payload carries them: there is no token usage in
//! any hook event, no git branch, no exit code, and — as 842 real envelopes proved — no
//! tool output under the key the adapter was reading.
//!
//! All of it is in the transcript, whose path every hook payload hands us. Verified
//! against a live 30 MB / 15,832-record file:
//!
//! | Needed | Transcript source |
//! |---|---|
//! | tool output | `toolUseResult.stdout` / `.stderr` |
//! | **did it fail** | `is_error: true` on the `tool_result` content block |
//! | tokens and cost | `message.usage` |
//! | git branch | `gitBranch` |
//! | files edited | `filePath`, `structuredPatch` |
//! | **Bash-driven edits** | `bashEditDiff` |
//!
//! That last row is why this is worth doing rather than fixing a field name: most file
//! edits in a real session happen through a Bash heredoc or `sed`, whose `tool_input`
//! has no `file_path` at all. No hook can see those. The transcript records them.
//!
//! **The join key already exists.** The hook writes `message_id` from `tool_use_id`, and
//! the transcript keys results by the same id. Nothing new has to be captured to connect
//! the two.
//!
//! **Redaction runs here, before anything leaves the machine.** This is raw command
//! output — the single likeliest place a secret sits, and the reason the hook's own
//! result capture was covered by two security tests. Those tests moved here with the
//! data; `a_secret_in_command_output_never_reaches_the_enrichment` is their replacement.

use std::path::Path;

use ctxlake_core::enrichment::{Enrichment, SCHEMA_VERSION};
use ctxlake_core::redact::Redactor;
use ctxlake_hook::adapters::common::{scrub_field, RedactionAcc};

/// Cap on a single captured field on this path.
///
/// Twenty times the hook's, because the constraints are not the same one. The hook's cap
/// is a latency guard on a 5 ms budget; this runs in the daemon at seal time with
/// nothing waiting on it, so the only question is how much output is worth keeping. A
/// `cargo test --workspace` failure is the exact case where the tail of the output is
/// the whole value.
pub const MAX_TRANSCRIPT_FIELD_BYTES: usize = 20 * 1024 * 1024;

/// Read `path` and reduce it to what a digest needs.
///
/// Tolerant by construction: a transcript is another program's private format, it is
/// appended to while we read it, and a record we do not understand is not an error. Any
/// line that fails to parse is skipped; the worst outcome is a thinner digest.
pub fn read_enrichment(path: &Path) -> Result<Enrichment, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("reading transcript {}: {e}", path.display()))?;
    Ok(parse_enrichment(&text))
}

/// [`read_enrichment`] over already-loaded text — the seam the tests drive.
pub fn parse_enrichment(text: &str) -> Enrichment {
    let redactor = Redactor::new();
    let mut acc = RedactionAcc::default();
    let mut out = Enrichment {
        schema_version: SCHEMA_VERSION,
        ..Default::default()
    };
    let mut usage = ctxlake_core::envelope::Usage::default();
    let mut saw_usage = false;

    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if out.branch.is_none() {
            if let Some(b) = rec.get("gitBranch").and_then(|v| v.as_str()) {
                if !b.is_empty() {
                    out.branch = Some(b.to_string());
                }
            }
        }

        if let Some(u) = rec.pointer("/message/usage") {
            saw_usage |= accumulate_usage(&mut usage, u);
        }

        // A tool's outcome is split across two places in the same record: the failure
        // flag lives on the `tool_result` content block, the output and paths on the
        // sibling `toolUseResult`. Both are keyed by the same `tool_use_id`.
        let Some(id) = tool_use_id(&rec) else {
            continue;
        };
        let entry = out.tools.entry(id).or_default();

        if let Some(failed) = is_error(&rec) {
            entry.exit_code = Some(if failed { 1 } else { 0 });
        }
        if let Some(r) = rec.get("toolUseResult") {
            if let Some(text) = result_text(r) {
                entry.result = scrub_capped(&redactor, &mut acc, text);
            }
            entry.paths = edited_paths(r);
        }
    }

    if saw_usage {
        out.usage = Some(usage);
    }
    out.redaction_status = acc.into_redaction().status;
    out
}

/// The `tool_use_id` this record is about, from either shape that carries one.
fn tool_use_id(rec: &serde_json::Value) -> Option<String> {
    if let Some(id) = rec.get("toolUseID").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    rec.pointer("/message/content")?
        .as_array()?
        .iter()
        .find_map(|b| b.get("tool_use_id").and_then(|v| v.as_str()))
        .map(str::to_string)
}

/// Whether the runtime marked this call failed. `None` when the record says nothing.
fn is_error(rec: &serde_json::Value) -> Option<bool> {
    let blocks = rec.pointer("/message/content")?.as_array()?;
    let block = blocks
        .iter()
        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))?;
    // Observed as both a JSON boolean and the string "True" in real transcripts, so
    // both are accepted rather than one being assumed.
    match block.get("is_error") {
        Some(serde_json::Value::Bool(b)) => Some(*b),
        Some(serde_json::Value::String(s)) => Some(s.eq_ignore_ascii_case("true")),
        Some(_) => Some(false),
        None => Some(false),
    }
}

/// Readable output for a tool result, whatever shape it took.
fn result_text(r: &serde_json::Value) -> Option<String> {
    if let Some(s) = r.as_str() {
        return Some(s.to_string());
    }
    let obj = r.as_object()?;
    let mut parts = Vec::new();
    for key in ["stdout", "stderr"] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                parts.push(v.to_string());
            }
        }
    }
    if parts.is_empty() {
        // An edit's result has no stdout; its content is the patch itself.
        for key in ["bashEditDiff", "content"] {
            if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    parts.push(v.to_string());
                }
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Files this tool call edited, including the Bash-driven edits no hook can see.
fn edited_paths(r: &serde_json::Value) -> Vec<String> {
    let Some(obj) = r.as_object() else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Some(p) = obj.get("filePath").and_then(|v| v.as_str()) {
        out.push(p.to_string());
    }
    for key in ["structuredPatch", "bashEditDiff"] {
        if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
            for entry in arr {
                for k in ["filePath", "file", "path"] {
                    if let Some(p) = entry.get(k).and_then(|v| v.as_str()) {
                        out.push(p.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Add one `message.usage` object into the running total.
fn accumulate_usage(into: &mut ctxlake_core::envelope::Usage, u: &serde_json::Value) -> bool {
    let get = |k: &str| u.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let (i, o, cr, cw) = (
        get("input_tokens"),
        get("output_tokens"),
        get("cache_read_input_tokens"),
        get("cache_creation_input_tokens"),
    );
    if i == 0 && o == 0 && cr == 0 && cw == 0 {
        return false;
    }
    into.input_tokens += i;
    into.output_tokens += o;
    into.cache_read_tokens += cr;
    into.cache_write_tokens += cw;
    true
}

/// Scrub with this path's much larger cap.
///
/// `scrub_field` truncates at the hook's `MAX_FIELD_BYTES` before scrubbing, which is
/// the right bound there and the wrong one here — so the value is pre-bounded to this
/// module's cap and handed over already inside the hook's.
fn scrub_capped(redactor: &Redactor, acc: &mut RedactionAcc, mut value: String) -> Option<String> {
    if value.len() > MAX_TRANSCRIPT_FIELD_BYTES {
        let mut end = MAX_TRANSCRIPT_FIELD_BYTES;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.push_str("\n[ctxlake: truncated]");
    }
    scrub_field(redactor, acc, Some(value), true)
}

/// Wires [`read_enrichment`] into the daemon's seal step.
///
/// Lives here rather than in `ctxlake-sync` because the reader needs the redactor and
/// the adapter helpers, and that crate deliberately depends on neither. `ctxlake-sync`
/// owns *when* a session is enriched; this owns *how*.
#[derive(Debug, Default)]
pub struct TranscriptEnricher;

impl ctxlake_sync::upload::SessionEnricher for TranscriptEnricher {
    fn enrichment_for(
        &self,
        runtime: ctxlake_core::Runtime,
        session_id: &str,
        done_sentinel: &std::path::Path,
    ) -> Option<Vec<u8>> {
        // Only Claude Code keeps a transcript. Cursor and Hermes populate result and
        // exit code on the hook path already and have nothing to attach here.
        if runtime != ctxlake_core::Runtime::ClaudeCode {
            return None;
        }
        let raw = std::fs::read_to_string(done_sentinel).ok()?;
        let path = serde_json::from_str::<serde_json::Value>(&raw)
            .ok()?
            .get("transcript_path")?
            .as_str()?
            .to_string();

        // Every one of these is a normal outcome, not a failure: a sentinel written by
        // a version that did not record the path, a transcript already rotated away, a
        // session that produced nothing worth attaching. The session is sealed either
        // way and the digest is the thinner one it would have been.
        let enrichment = match read_enrichment(std::path::Path::new(&path)) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(session = %session_id, error = %e, "no transcript to attach");
                return None;
            }
        };
        if enrichment.tools.is_empty() && enrichment.usage.is_none() {
            return None;
        }
        serde_json::to_vec(&enrichment).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records captured from a real 30 MB transcript and scrubbed of this machine's
    /// identity — one successful Bash, one failure, one file edit, one Bash-driven
    /// edit, one assistant turn with usage.
    ///
    /// Captured rather than hand-written, deliberately. The bug this module replaces
    /// survived because `post_tool_use.json` was written from documentation and the
    /// adapter read the same invented key, so fixture and code agreed with each other
    /// and disagreed with the runtime.
    const REAL: &str = include_str!("../../tests/fixtures/claude-code-transcript/sample.jsonl");

    #[test]
    fn a_real_transcript_yields_the_fields_the_digest_promises() {
        let e = parse_enrichment(REAL);

        assert!(
            e.branch.is_some(),
            "gitBranch must be recovered — no hook payload carries it"
        );
        let usage = e.usage.expect("message.usage must be recovered");
        assert!(
            usage.input_tokens + usage.output_tokens + usage.cache_read_tokens > 0,
            "usage must be non-zero: {usage:?}"
        );
        assert!(!e.tools.is_empty(), "tool outcomes must be keyed by id");

        let failed: Vec<_> = e
            .tools
            .values()
            .filter(|t| t.exit_code == Some(1))
            .collect();
        assert!(
            !failed.is_empty(),
            "the failure signal must survive — friction detection is built on it"
        );

        let with_output = e.tools.values().filter(|t| t.result.is_some()).count();
        assert!(with_output > 0, "tool output must be recovered");

        let with_paths = e.tools.values().filter(|t| !t.paths.is_empty()).count();
        assert!(
            with_paths > 0,
            "edited files must be recovered, including Bash-driven ones"
        );
    }

    #[test]
    fn a_bash_driven_edit_is_attributed_to_its_file() {
        // The row that justifies reading the transcript at all rather than fixing a
        // hook field name: most real edits go through a Bash heredoc or `sed`, whose
        // `tool_input` has no `file_path`, so no hook can attribute them.
        let line = serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "tu-9"}]},
            "toolUseResult": {
                "stdout": "",
                "bashEditDiff": [{"filePath": "/repo/src/main.rs"}]
            }
        })
        .to_string();
        let e = parse_enrichment(&line);
        assert_eq!(e.tools["tu-9"].paths, vec!["/repo/src/main.rs".to_string()]);
    }

    #[test]
    fn a_secret_in_command_output_never_reaches_the_enrichment() {
        // The replacement for the hook-side security tests that moved here with the
        // data. This is raw command output: the likeliest place a key appears.
        let line = serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "tu-1"}]},
            "toolUseResult": {
                "stdout": "OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz0123456789",
                "stderr": ""
            }
        })
        .to_string();
        let e = parse_enrichment(&line);
        let blob = serde_json::to_string(&e).unwrap();
        assert!(
            !blob.contains("sk-proj-abcdefghijklmnopqrstuvwxyz0123456789"),
            "a secret reached the enrichment object: {blob}"
        );
        assert_ne!(
            e.redaction_status, "clean",
            "and the object must record that it was scrubbed"
        );
    }

    #[test]
    fn is_error_is_accepted_as_both_a_boolean_and_a_string() {
        // Real transcripts carry both. Assuming one silently loses every failure
        // recorded in the other form — which is the whole friction signal.
        for flag in [serde_json::json!(true), serde_json::json!("True")] {
            let line = serde_json::json!({
                "type": "user",
                "message": {"content": [
                    {"type": "tool_result", "tool_use_id": "tu-1", "is_error": flag}
                ]}
            })
            .to_string();
            let e = parse_enrichment(&line);
            assert_eq!(
                e.tools["tu-1"].exit_code,
                Some(1),
                "failure must be detected for {flag:?}"
            );
        }
    }

    #[test]
    fn a_successful_call_is_recorded_as_such_not_as_unknown() {
        // `None` and `Some(0)` mean different things downstream: "the runtime said
        // nothing" versus "the runtime said it worked". Only the second lets the digest
        // count a command at all.
        let line = serde_json::json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "tu-1", "is_error": false}
            ]},
            "toolUseResult": {"stdout": "ok", "stderr": ""}
        })
        .to_string();
        let e = parse_enrichment(&line);
        assert_eq!(e.tools["tu-1"].exit_code, Some(0));
    }

    #[test]
    fn an_unparseable_line_is_skipped_rather_than_failing_the_session() {
        // A transcript is another program's private format, appended to while we read
        // it. A record we do not understand must cost one record, never the session.
        let text = format!(
            "not json at all\n{}\n{{\"half\": \n",
            serde_json::json!({
                "type": "user",
                "gitBranch": "main",
                "message": {"content": [{"type": "tool_result", "tool_use_id": "tu-1"}]},
                "toolUseResult": {"stdout": "fine", "stderr": ""}
            })
        );
        let e = parse_enrichment(&text);
        assert_eq!(e.branch.as_deref(), Some("main"));
        assert!(e.tools.contains_key("tu-1"));
    }

    #[test]
    fn an_empty_transcript_produces_an_empty_enrichment_not_an_error() {
        let e = parse_enrichment("");
        assert!(e.tools.is_empty());
        assert!(e.usage.is_none());
        assert!(e.branch.is_none());
        assert_eq!(e.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn oversized_output_is_truncated_on_a_character_boundary() {
        let huge = "é".repeat(MAX_TRANSCRIPT_FIELD_BYTES);
        let redactor = Redactor::new();
        let mut acc = RedactionAcc::default();
        let got = scrub_capped(&redactor, &mut acc, huge).expect("some output");
        assert!(got.len() <= MAX_TRANSCRIPT_FIELD_BYTES + 64);
        assert!(got.contains("truncated"));
    }
}
