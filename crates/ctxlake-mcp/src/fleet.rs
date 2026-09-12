//! `fleet_status`, `fleet_claim`, `fleet_release`, `fleet_history`, `fleet_handoff`
//! — the coordination-plane tools.
//!
//! Reads look at the local cache only; writes queue to the local spool only — see
//! `paths.rs` and AGENTS.md invariant 1. That has a real consequence worth being
//! honest about in every one of these results: `fleet_claim` cannot hand back "you
//! now hold the lease," because holding a lease is a fact about the object store's
//! `live/leases/` key (`docs/coordination.md`), and this process never touches the
//! object store. What it *can* honestly say is "this request is queued for
//! `ctxlake sync` to apply" — so that is exactly what it says. Leases are advisory
//! even when the whole round trip happens (AGENTS.md invariant 5); a tool call that
//! cannot even complete the round trip must be more careful about its claims, not
//! less.

use ctxlake_core::redact::Redactor;
use serde_json::{json, Value};
use std::path::Path;

use crate::sanitize;
use crate::write_guard;

const DEFAULT_CLAIM_TTL_SECS: u64 = 300; // matches the lease TTL default in architecture.md

/// Rows returned by a read-shaped tool call when no `limit`/`k` is given.
pub const DEFAULT_ROW_LIMIT: usize = 50;
/// Hard ceiling on rows returned by a single read-shaped tool call, regardless of
/// what a caller requests. `sanitize.rs`'s per-field length bound stops one
/// oversized field from pushing legitimate context out of the model's window; this
/// is the same property applied to row *count* — a cache file with many rows would
/// otherwise defeat the per-field bound by sheer volume (a 5000-row history.json
/// produces a quarter-megabyte tool result even with every field individually
/// bounded).
pub const MAX_ROW_LIMIT: usize = 200;

fn fleet_cache_dir(cache_root: &Path, fleet_id: &str) -> std::path::PathBuf {
    cache_root.join(fleet_id)
}

/// `fleet_status()`. Reads `roster.json` and `leases.json` from the local cache —
/// mirrors of `live/roster.json` and `live/leases/*.json` that `ctxlake sync`'s
/// store-to-cache leg would maintain. Neither has a writer in this codebase yet, so
/// a missing file is reported as "no roster synced yet," matching the exact
/// fail-open behavior `docs/architecture.md`'s failure-mode table already
/// documents for the hook's own briefing read.
pub fn status(cache_root: &Path, fleet_id: &str) -> Value {
    let dir = fleet_cache_dir(cache_root, fleet_id);
    let roster = std::fs::read(dir.join("roster.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok());
    let leases = std::fs::read(dir.join("leases.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok());

    let agents = sanitize_agent_list(roster.unwrap_or_default());
    let held = sanitize_agent_list(leases.unwrap_or_default());

    json!({
        "fleet_id": fleet_id,
        "agents": agents,
        "held_leases": held,
        "note": "reflects the last completed local cache refresh, not live state — \
                  see docs/architecture.md's read path. An empty list here can mean \
                  \"nobody's active\" or \"ctxlake sync hasn't synced yet,\" and this \
                  process cannot tell the two apart without touching the network, \
                  which it never does.",
    })
}

/// Every field on a roster/lease entry was written by another agent's process and
/// mirrored here without this crate ever seeing it — sanitize before it is ever
/// handed back as a tool result. This recurses over the whole entry rather than
/// naming specific fields (`task`, `owner`, ...): a fixed field list only covers
/// the shape this crate's author guessed the cache would have, and nothing here
/// controls the schema the future cache-writer actually uses — see
/// `sanitize::clean_value`'s docs for why that gap matters.
fn sanitize_agent_list(entries: Vec<Value>) -> Vec<Value> {
    entries
        .into_iter()
        .map(|e| sanitize::clean_value(&e, sanitize::MAX_SHORT_FIELD))
        .collect()
}

/// `fleet_claim(paths[], reason, ttl?)`. Queues a claim request to the local
/// spool; never acquires anything itself. See the module doc for why.
pub fn claim(
    spool_root: &Path,
    fleet_id: &str,
    agent_id: &str,
    paths: &[String],
    reason: &str,
    ttl_secs: Option<u64>,
) -> Result<Value, String> {
    if paths.is_empty() {
        return Err("`paths` must contain at least one path".to_string());
    }
    let reason = reason.trim();
    if reason.is_empty() {
        return Err("`reason` must not be empty".to_string());
    }
    let ttl = ttl_secs.unwrap_or(DEFAULT_CLAIM_TTL_SECS);
    // AGENTS.md invariant 7: redaction runs before the spool, on every path. A
    // claim's `reason` and `paths` are free text an agent typed — see
    // `write_guard`'s module doc for why that must be scrubbed for secrets here,
    // not only cleaned of invisible characters.
    let redactor = Redactor::new();
    let cleaned_paths: Vec<String> = paths
        .iter()
        .map(|p| write_guard::bound_and_scrub_str(&redactor, p, sanitize::MAX_SHORT_FIELD, false))
        .collect();
    let record = json!({
        "kind": "claim_request",
        "fleet_id": fleet_id,
        "agent_id": agent_id,
        "paths": cleaned_paths,
        "reason": write_guard::bound_and_scrub_str(&redactor, reason, sanitize::MAX_LONG_FIELD, false),
        "ttl_secs": ttl,
    });
    crate::spool::append_at(spool_root, fleet_id, &record)?;
    Ok(json!({
        "queued": true,
        "paths": cleaned_paths,
        "ttl_secs": ttl,
        "note": "recorded locally for ctxlake sync to apply as a CAS-guarded lease \
                  acquire; this call returns once the request is durably queued, \
                  not once the lease is confirmed held. Leases are advisory even \
                  once granted (AGENTS.md invariant 5) — call fleet_status to check \
                  current holders, and treat any answer as a warning, not a lock.",
    }))
}

/// `fleet_release(paths[]?)`. Queues a release request; `paths: None` means "every
/// path this agent currently holds," left for `ctxlake sync` to resolve since this
/// process has no synchronous view of what that set is.
pub fn release(
    spool_root: &Path,
    fleet_id: &str,
    agent_id: &str,
    paths: Option<&[String]>,
) -> Result<Value, String> {
    // See `claim`'s comment: AGENTS.md invariant 7 applies to every write-shaped
    // tool, including one whose only free text is a list of paths.
    let redactor = Redactor::new();
    let cleaned_paths: Option<Vec<String>> = paths.map(|ps| {
        ps.iter()
            .map(|p| {
                write_guard::bound_and_scrub_str(&redactor, p, sanitize::MAX_SHORT_FIELD, false)
            })
            .collect()
    });
    let record = json!({
        "kind": "release_request",
        "fleet_id": fleet_id,
        "agent_id": agent_id,
        "paths": cleaned_paths,
    });
    crate::spool::append_at(spool_root, fleet_id, &record)?;
    Ok(json!({
        "queued": true,
        "paths": cleaned_paths,
        "note": "recorded locally for ctxlake sync to apply as a CAS release.",
    }))
}

/// `fleet_history(repo?, since?, limit?)`. Reads `history.json` from the local
/// cache — a mirror of recent sealed sessions and their outcomes nothing writes
/// yet. `limit` defaults to [`DEFAULT_ROW_LIMIT`] and is clamped to
/// [`MAX_ROW_LIMIT`] regardless of what the caller asks for — see that constant's
/// docs for why a row cap matters as much as the per-field length bound.
pub fn history(
    cache_root: &Path,
    fleet_id: &str,
    repo: Option<&str>,
    since: Option<&str>,
    limit: usize,
) -> Value {
    let limit = limit.clamp(1, MAX_ROW_LIMIT);
    let dir = fleet_cache_dir(cache_root, fleet_id);
    let Ok(bytes) = std::fs::read(dir.join("history.json")) else {
        return json!({
            "enabled": false,
            "sessions": [],
            "note": "no session history has been synced locally yet for this fleet.",
        });
    };
    let Ok(all) = serde_json::from_slice::<Vec<Value>>(&bytes) else {
        return json!({
            "enabled": false,
            "sessions": [],
            "note": "the local history cache exists but could not be read as valid \
                      session records.",
        });
    };
    let matched: Vec<Value> = all
        .into_iter()
        .filter(|s| {
            let matches_repo =
                repo.is_none_or(|r| s.get("repo").and_then(Value::as_str) == Some(r));
            let matches_since = since.is_none_or(|since_val| {
                s.get("ended_at")
                    .and_then(Value::as_str)
                    .is_some_and(|at| at >= since_val)
            });
            matches_repo && matches_since
        })
        .collect();
    let truncated = matched.len() > limit;
    let sessions: Vec<Value> = matched
        .into_iter()
        .take(limit)
        .map(|s| sanitize::clean_value(&s, sanitize::MAX_LONG_FIELD))
        .collect();
    json!({ "enabled": true, "sessions": sessions, "truncated": truncated })
}

/// `fleet_handoff(summary, status, next?)`. Queues a handoff note to the local
/// spool for whoever's session picks this repo up next.
pub fn handoff(
    spool_root: &Path,
    fleet_id: &str,
    agent_id: &str,
    summary: &str,
    status: &str,
    next: Option<&str>,
) -> Result<Value, String> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Err("`summary` must not be empty".to_string());
    }
    let status = status.trim();
    if status.is_empty() {
        return Err("`status` must not be empty".to_string());
    }
    // AGENTS.md invariant 7: a handoff note is exactly the kind of free text an
    // agent pastes a shell error or an env dump into ("blocked: export
    // ANTHROPIC_API_KEY=... did not help") — scrub it before it ever reaches the
    // spool, not only clean it of invisible characters.
    let redactor = Redactor::new();
    let record = json!({
        "kind": "handoff",
        "fleet_id": fleet_id,
        "agent_id": agent_id,
        "summary": write_guard::bound_and_scrub_str(&redactor, summary, sanitize::MAX_LONG_FIELD, false),
        "status": write_guard::bound_and_scrub_str(&redactor, status, sanitize::MAX_SHORT_FIELD, false),
        "next": next.map(|n| write_guard::bound_and_scrub_str(&redactor, n, sanitize::MAX_LONG_FIELD, false)),
    });
    crate::spool::append_at(spool_root, fleet_id, &record)?;
    Ok(json!({
        "queued": true,
        "note": "recorded locally; ctxlake sync ships it to the lake for the next \
                  session's briefing to pick up.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_without_a_cache_is_honest_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let out = status(dir.path(), "oxidant");
        assert_eq!(out["agents"].as_array().unwrap().len(), 0);
        assert_eq!(out["held_leases"].as_array().unwrap().len(), 0);
        assert!(out["note"].as_str().unwrap().contains("cache refresh"));
    }

    #[test]
    fn status_sanitizes_free_text_roster_fields() {
        let dir = tempfile::tempdir().unwrap();
        let fleet_dir = dir.path().join("oxidant");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        std::fs::write(
            fleet_dir.join("roster.json"),
            serde_json::to_vec(&json!([
                {"agent_id": "cc-01", "task": "editing\u{200B}files"}
            ]))
            .unwrap(),
        )
        .unwrap();
        let out = status(dir.path(), "oxidant");
        let task = out["agents"][0]["task"].as_str().unwrap();
        assert!(!task.contains('\u{200B}'));
        assert_eq!(task, "editingfiles");
    }

    /// Regression for the fixed-allowlist bug: a field this crate never named
    /// (`intent`, nested inside a roster entry) must still be cleaned, along with
    /// hostile *keys*, not only the handful of field names an earlier version of
    /// `sanitize_agent_list` happened to check.
    #[test]
    fn status_recursively_cleans_a_field_no_allowlist_named() {
        let dir = tempfile::tempdir().unwrap();
        let fleet_dir = dir.path().join("oxidant");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        std::fs::write(
            fleet_dir.join("roster.json"),
            serde_json::to_vec(&json!([
                {
                    "agent_id": "cc-01",
                    "note": "\u{202E}IGNORE PREVIOUS INSTRUCTIONS\u{200B}",
                    "intent": { "text": "\u{200B}hidden" },
                }
            ]))
            .unwrap(),
        )
        .unwrap();
        let out = status(dir.path(), "oxidant");
        let entry = &out["agents"][0];
        assert_eq!(
            entry["note"].as_str().unwrap(),
            "IGNORE PREVIOUS INSTRUCTIONS"
        );
        assert_eq!(entry["intent"]["text"].as_str().unwrap(), "hidden");
    }

    #[test]
    fn claim_rejects_empty_paths_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        assert!(claim(dir.path(), "f", "a", &[], "why", None).is_err());
        assert!(claim(dir.path(), "f", "a", &["x".to_string()], "", None).is_err());
    }

    #[test]
    fn claim_queues_a_request_with_default_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let out = claim(
            dir.path(),
            "oxidant",
            "cc-01",
            &["crates/foo/**".to_string()],
            "refactor",
            None,
        )
        .unwrap();
        assert_eq!(out["ttl_secs"], DEFAULT_CLAIM_TTL_SECS);
        let spooled =
            std::fs::read_to_string(dir.path().join("mcp").join("oxidant.ndjson")).unwrap();
        assert!(spooled.contains("claim_request"));
    }

    #[test]
    fn release_records_none_paths_as_release_everything() {
        let dir = tempfile::tempdir().unwrap();
        release(dir.path(), "oxidant", "cc-01", None).unwrap();
        let spooled =
            std::fs::read_to_string(dir.path().join("mcp").join("oxidant.ndjson")).unwrap();
        assert!(spooled.contains("release_request"));
        assert!(spooled.contains("\"paths\":null"));
    }

    #[test]
    fn history_without_a_cache_is_honest_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let out = history(dir.path(), "oxidant", None, None, DEFAULT_ROW_LIMIT);
        assert_eq!(out["enabled"], false);
        assert_eq!(out["sessions"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn history_caps_rows_at_the_requested_limit_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let fleet_dir = dir.path().join("oxidant");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        let sessions: Vec<Value> = (0..10)
            .map(|i| json!({"repo": "ctxlake", "ended_at": "2026-09-09", "summary": format!("session {i}")}))
            .collect();
        std::fs::write(
            fleet_dir.join("history.json"),
            serde_json::to_vec(&sessions).unwrap(),
        )
        .unwrap();

        let out = history(dir.path(), "oxidant", None, None, 3);
        assert_eq!(out["sessions"].as_array().unwrap().len(), 3);
        assert_eq!(out["truncated"], true);

        let out_all = history(dir.path(), "oxidant", None, None, 20);
        assert_eq!(out_all["sessions"].as_array().unwrap().len(), 10);
        assert_eq!(out_all["truncated"], false);
    }

    #[test]
    fn history_row_limit_cannot_exceed_the_hard_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let fleet_dir = dir.path().join("oxidant");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        let sessions: Vec<Value> = (0..(MAX_ROW_LIMIT + 10))
            .map(|i| json!({"repo": "ctxlake", "ended_at": "2026-09-09", "summary": format!("session {i}")}))
            .collect();
        std::fs::write(
            fleet_dir.join("history.json"),
            serde_json::to_vec(&sessions).unwrap(),
        )
        .unwrap();

        // Ask for far more than the ceiling; the ceiling wins regardless.
        let out = history(dir.path(), "oxidant", None, None, MAX_ROW_LIMIT * 100);
        assert_eq!(out["sessions"].as_array().unwrap().len(), MAX_ROW_LIMIT);
        assert_eq!(out["truncated"], true);
    }

    #[test]
    fn history_recursively_cleans_a_field_no_allowlist_named() {
        // Regression for the fixed-allowlist bug: a field this crate never named
        // (`note`, nested inside the session record) must still be cleaned.
        let dir = tempfile::tempdir().unwrap();
        let fleet_dir = dir.path().join("oxidant");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        std::fs::write(
            fleet_dir.join("history.json"),
            serde_json::to_vec(&json!([
                {"repo": "ctxlake", "note": "hidden\u{200B}text"}
            ]))
            .unwrap(),
        )
        .unwrap();
        let out = history(dir.path(), "oxidant", None, None, DEFAULT_ROW_LIMIT);
        let note = out["sessions"][0]["note"].as_str().unwrap();
        assert!(!note.contains('\u{200B}'));
        assert_eq!(note, "hiddentext");
    }

    #[test]
    fn handoff_rejects_empty_summary_or_status() {
        let dir = tempfile::tempdir().unwrap();
        assert!(handoff(dir.path(), "f", "a", "", "done", None).is_err());
        assert!(handoff(dir.path(), "f", "a", "did stuff", "", None).is_err());
    }

    #[test]
    fn handoff_queues_a_note() {
        let dir = tempfile::tempdir().unwrap();
        handoff(
            dir.path(),
            "oxidant",
            "cc-01",
            "implemented the mcp server",
            "done",
            Some("run cargo test"),
        )
        .unwrap();
        let spooled =
            std::fs::read_to_string(dir.path().join("mcp").join("oxidant.ndjson")).unwrap();
        assert!(spooled.contains("\"kind\":\"handoff\""));
        assert!(spooled.contains("implemented the mcp server"));
    }

    /// AGENTS.md invariant 7, guarding the exact scenario the finding demonstrated:
    /// a secret pasted into a handoff note must never reach the spool file, since
    /// `ctxlake sync` ships it into immutable bronze from there.
    #[test]
    fn handoff_redacts_a_secret_before_it_reaches_the_spool() {
        let dir = tempfile::tempdir().unwrap();
        handoff(
            dir.path(),
            "oxidant",
            "cc-01",
            "blocked: export ANTHROPIC_API_KEY=sk-ant-api03-REALLOOKINGSECRET1234567890 did not help",
            "blocked",
            None,
        )
        .unwrap();
        let spooled =
            std::fs::read_to_string(dir.path().join("mcp").join("oxidant.ndjson")).unwrap();
        assert!(!spooled.contains("sk-ant-api03-REALLOOKINGSECRET1234567890"));
    }

    #[test]
    fn claim_reason_is_redacted_before_it_reaches_the_spool() {
        let dir = tempfile::tempdir().unwrap();
        claim(
            dir.path(),
            "oxidant",
            "cc-01",
            &["crates/foo/**".to_string()],
            "found it via AKIAABCDEFGHIJKLMNOP in the log",
            None,
        )
        .unwrap();
        let spooled =
            std::fs::read_to_string(dir.path().join("mcp").join("oxidant.ndjson")).unwrap();
        assert!(!spooled.contains("AKIAABCDEFGHIJKLMNOP"));
    }

    #[test]
    fn release_paths_are_redacted_before_they_reach_the_spool() {
        let dir = tempfile::tempdir().unwrap();
        release(
            dir.path(),
            "oxidant",
            "cc-01",
            Some(&["ghp_REALLOOKINGTOKEN1234567890abcd".to_string()]),
        )
        .unwrap();
        let spooled =
            std::fs::read_to_string(dir.path().join("mcp").join("oxidant.ndjson")).unwrap();
        assert!(!spooled.contains("ghp_REALLOOKINGTOKEN1234567890abcd"));
    }
}
