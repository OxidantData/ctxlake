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

use serde_json::{json, Value};
use std::path::Path;

use crate::sanitize;

const DEFAULT_CLAIM_TTL_SECS: u64 = 300; // matches the lease TTL default in architecture.md

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

/// Every free-text field on a roster/lease entry (a task description, an owner
/// string) was written by another agent's process and mirrored here without this
/// crate ever seeing it — sanitize before it is ever handed back as a tool result.
fn sanitize_agent_list(entries: Vec<Value>) -> Vec<Value> {
    entries
        .into_iter()
        .map(|mut e| {
            for field in ["task", "owner", "repo", "branch", "agent_id"] {
                if let Some(s) = e.get(field).and_then(Value::as_str) {
                    let cleaned = sanitize::clean(s, sanitize::MAX_SHORT_FIELD);
                    e[field] = json!(cleaned);
                }
            }
            e
        })
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
    let cleaned_paths: Vec<String> = paths
        .iter()
        .map(|p| sanitize::clean(p, sanitize::MAX_SHORT_FIELD))
        .collect();
    let record = json!({
        "kind": "claim_request",
        "fleet_id": fleet_id,
        "agent_id": agent_id,
        "paths": cleaned_paths,
        "reason": sanitize::clean(reason, sanitize::MAX_LONG_FIELD),
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
    let cleaned_paths: Option<Vec<String>> = paths.map(|ps| {
        ps.iter()
            .map(|p| sanitize::clean(p, sanitize::MAX_SHORT_FIELD))
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

/// `fleet_history(repo?, since?)`. Reads `history.json` from the local cache — a
/// mirror of recent sealed sessions and their outcomes nothing writes yet.
pub fn history(
    cache_root: &Path,
    fleet_id: &str,
    repo: Option<&str>,
    since: Option<&str>,
) -> Value {
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
    let sessions: Vec<Value> = all
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
        .map(|mut s| {
            for field in ["summary", "outcome", "agent_id", "branch"] {
                if let Some(v) = s.get(field).and_then(Value::as_str) {
                    let cleaned = sanitize::clean(v, sanitize::MAX_LONG_FIELD);
                    s[field] = json!(cleaned);
                }
            }
            s
        })
        .collect();
    json!({ "enabled": true, "sessions": sessions })
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
    let record = json!({
        "kind": "handoff",
        "fleet_id": fleet_id,
        "agent_id": agent_id,
        "summary": sanitize::clean(summary, sanitize::MAX_LONG_FIELD),
        "status": sanitize::clean(status, sanitize::MAX_SHORT_FIELD),
        "next": next.map(|n| sanitize::clean(n, sanitize::MAX_LONG_FIELD)),
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
        let out = history(dir.path(), "oxidant", None, None);
        assert_eq!(out["enabled"], false);
        assert_eq!(out["sessions"].as_array().unwrap().len(), 0);
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
}
