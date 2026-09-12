//! `ctxlake status` — render the roster.
//!
//! Roster only: who is active, on what branch, doing what. This used to have a
//! second, "leases (live, fetched just now)" section reporting who currently held
//! a reservation on a path — that concept is gone (see `docs/coordination.md`):
//! compaction, extraction, and the snapshot publish are each already safe under
//! concurrent writers by construction (content-addressed generations, a
//! create-once claim marker, a CAS pointer swap), so there was never a resource
//! here that needed a holder to protect it. Nothing this command shows depends on
//! the object store at all any more, only the local roster cache written by
//! `ctxlake sync`'s store-to-cache leg — see [`render_roster`] for the read path
//! and its staleness story, and [`render_status`] for the outer function `run`
//! actually prints.

use std::time::SystemTime;

use anyhow::{Context, Result};
use ctxlake_store::roster::RosterSnapshot;
use time::OffsetDateTime;

use crate::config::Config;
use crate::paths;
use crate::sanitize::sanitize;

pub async fn run(cfg: &Config) -> Result<()> {
    let rendered = render_status(cfg)?;
    print!("{rendered}");
    Ok(())
}

/// The I/O half of [`run`]: reads the roster cache and renders it, without
/// printing. Split out from `run` so a test can assert on the exact text `run`
/// would print instead of capturing stdout — see
/// `status_run_never_mentions_leases_and_does_not_touch_a_bare_store`.
fn render_status(cfg: &Config) -> Result<String> {
    let cache_path = paths::cache_dir(&cfg.fleet_id).join("roster.json");
    let text = match std::fs::read_to_string(&cache_path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", cache_path.display())),
    };
    render_roster(&cfg.fleet_id, &cache_path, text.as_deref())
        .with_context(|| format!("parsing {}", cache_path.display()))
}

/// The pure half of [`render_status`]: given the cache file's content (or `None`
/// for "never written"), produce the exact text to print. Split out so it's
/// testable without writing into a real `$XDG_DATA_HOME` — the only I/O in this
/// module that isn't itself the object store.
fn render_roster(
    fleet_id: &str,
    cache_path: &std::path::Path,
    text: Option<&str>,
) -> Result<String> {
    let mut out = String::new();
    let Some(text) = text else {
        out.push_str(&format!(
            "fleet {fleet_id} · no roster cache yet at {}\n",
            cache_path.display()
        ));
        out.push_str("  (has `ctxlake sync` run and completed its first refresh?)\n");
        return Ok(out);
    };
    let snapshot: RosterSnapshot = serde_json::from_str(text)?;

    let age = SystemTime::now()
        .duration_since(SystemTime::from(snapshot.generated_at))
        .unwrap_or_default();
    out.push_str(&format!(
        "fleet {fleet_id} · {} agent(s) active · roster {}s old ({})\n",
        snapshot.agents.len(),
        age.as_secs(),
        cache_path.display()
    ));
    if age.as_secs() > 60 {
        out.push_str(
            "  (stale — this is a cached view, not a live one; see docs/architecture.md)\n",
        );
    }

    for agent in &snapshot.agents {
        let entry_age = OffsetDateTime::now_utc() - agent.updated_at;
        // Every field below is written by some other agent (or that agent's
        // operator) and read back here — untrusted input per AGENTS.md's house
        // rule, rendered plainly rather than interpreted, but still sanitized
        // before it reaches the terminal (see `sanitize`'s own doc for why
        // "plainly" doesn't mean "unfiltered").
        out.push_str(&format!(
            "\n  {}  {}  {}  {}m\n",
            sanitize(&agent.agent_id),
            agent.runtime.as_str(), // our own enum's Display, not lake content
            agent
                .repo
                .as_deref()
                .map(sanitize)
                .as_deref()
                .unwrap_or("-"),
            entry_age.whole_minutes().max(0)
        ));
        if !agent.paths.is_empty() {
            let paths: Vec<String> = agent.paths.iter().map(|p| sanitize(p)).collect();
            out.push_str(&format!("           touching {}\n", paths.join(", ")));
        }
        if let Some(task) = &agent.task {
            out.push_str(&format!("           \"{}\"\n", sanitize(task)));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::Runtime;
    use ctxlake_store::intent::Intent;
    use ctxlake_store::roster::RosterSource;

    #[test]
    fn render_roster_reports_a_never_refreshed_cache_without_erroring() {
        let out = render_roster("myteam", std::path::Path::new("/x/roster.json"), None).unwrap();
        assert!(out.contains("no roster cache yet"));
        assert!(out.contains("ctxlake sync"));
    }

    #[test]
    fn render_roster_shows_agents_task_and_paths() {
        let snapshot = ctxlake_store::roster::RosterSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            source: RosterSource::RosterBuild,
            agents: vec![Intent {
                agent_id: "cc-01".into(),
                fleet_id: "myteam".into(),
                runtime: Runtime::ClaudeCode,
                session_id: None,
                repo: Some("github.com/OxidantData/ctxlake".into()),
                branch: Some("wave2/cli".into()),
                cwd: None,
                task: Some("implementing ctxlake-cli".into()),
                paths: vec!["crates/ctxlake-cli/src/main.rs".into()],
                updated_at: OffsetDateTime::now_utc(),
            }],
        };
        let text = serde_json::to_string(&snapshot).unwrap();
        let out = render_roster(
            "myteam",
            std::path::Path::new("/x/roster.json"),
            Some(&text),
        )
        .unwrap();
        assert!(out.contains("cc-01"));
        assert!(out.contains("claude_code"));
        assert!(out.contains("implementing ctxlake-cli"));
        assert!(out.contains("crates/ctxlake-cli/src/main.rs"));
        assert!(
            !out.contains("stale"),
            "a just-generated snapshot must not be flagged stale: {out}"
        );
    }

    #[test]
    fn render_roster_flags_a_stale_snapshot() {
        let snapshot = ctxlake_store::roster::RosterSnapshot {
            generated_at: OffsetDateTime::now_utc() - time::Duration::minutes(5),
            source: RosterSource::RosterBuild,
            agents: vec![],
        };
        let text = serde_json::to_string(&snapshot).unwrap();
        let out = render_roster(
            "myteam",
            std::path::Path::new("/x/roster.json"),
            Some(&text),
        )
        .unwrap();
        assert!(out.contains("stale"), "{out}");
    }

    #[test]
    fn render_roster_reports_malformed_cache_rather_than_panicking() {
        let err = render_roster(
            "myteam",
            std::path::Path::new("/x/roster.json"),
            Some("not json"),
        )
        .unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn render_roster_sanitizes_hostile_agent_fields_and_bounds_a_huge_paths_entry() {
        // Reproduces the finding: a `task` carrying an ANSI screen-clear, an
        // embedded newline, a bidi override (U+202E), and a zero-width space
        // (U+200B), plus a 200 KB `paths` entry, must never reach the terminal
        // unfiltered — the sanitizer strips the former and bounds the latter.
        let huge_path = "x".repeat(200_000);
        let snapshot = ctxlake_store::roster::RosterSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            source: RosterSource::RosterBuild,
            agents: vec![Intent {
                agent_id: "cc-01".into(),
                fleet_id: "myteam".into(),
                runtime: Runtime::ClaudeCode,
                session_id: None,
                repo: Some("github.com/OxidantData/ctxlake".into()),
                branch: Some("wave2/cli".into()),
                cwd: None,
                task: Some("\x1b[2J\x1b[H\nFAKE: crates/x  held by admin\u{202E}\u{200B}".into()),
                paths: vec![huge_path],
                updated_at: OffsetDateTime::now_utc(),
            }],
        };
        let text = serde_json::to_string(&snapshot).unwrap();
        let out = render_roster(
            "myteam",
            std::path::Path::new("/x/roster.json"),
            Some(&text),
        )
        .unwrap();
        assert!(!out.contains('\x1b'), "ANSI escape leaked: {out:?}");
        assert!(
            !out.contains('\u{202E}'),
            "bidi override leaked into the output"
        );
        assert!(
            !out.contains('\u{200B}'),
            "zero-width space leaked into the output"
        );
        // A forged newline inside `task` must not produce a second line that looks
        // like independent ctxlake output — the injected text may still appear
        // (sanitize doesn't redact content, only hostile control characters), but
        // it must never start its own line.
        assert!(
            !out.contains("\nFAKE:"),
            "an embedded newline let untrusted text forge a fake output line: {out:?}"
        );
        assert!(
            out.len() < 10_000,
            "a 200 KB paths entry must be bounded, got {} bytes",
            out.len()
        );
    }

    /// Regression for the lease removal: `status` used to fetch `live/leases/`
    /// live from the object store on every run, in addition to the roster cache
    /// read above. `render_status` (what `run` prints, verbatim — see its own
    /// doc) must render cleanly, and it must do so from a `Config` whose `store`
    /// isn't even a parseable URL.
    ///
    /// That's deliberate, not incidental: `store_ctx::connect` calls `Url::parse`
    /// on `cfg.store` before anything else, so if this function ever grew a call
    /// to it back (to fetch `live/leases/`, say), `connect` would fail immediately
    /// and this `.unwrap()` would panic — a re-added live fetch cannot pass this
    /// test no matter what it does with the result, unlike a bare directory's file
    /// count, which stays 0 whether or not a list against a never-written prefix
    /// happened (see the review finding this replaced). A previous version of this
    /// test used a real `file://` tempdir and asserted exactly that file count;
    /// it passed even with a live lease fetch reinstated.
    #[tokio::test]
    async fn status_run_never_mentions_leases_and_does_not_touch_a_bare_store() {
        // Fleet id deliberately doesn't contain the substring "lease" anywhere —
        // it's echoed verbatim into the output below, and a fleet id like
        // "...-no-leases" would make the assertion pass by accident.
        let cfg = Config::new("not-a-valid-store-url", "status-test-fleet-alice", "cc-01");
        // `run` is the public entry point (what `main.rs` actually calls) — assert
        // it succeeds and prints via the exact same rendering `render_status`
        // returns, then check the content on that returned string rather than on
        // stdout, which tests in this binary don't capture.
        run(&cfg).await.unwrap();
        let out = render_status(&cfg).unwrap();
        assert!(
            !out.to_lowercase().contains("lease"),
            "status output must never mention leases: {out}"
        );
    }

    #[test]
    fn render_roster_output_never_mentions_leases() {
        let snapshot = ctxlake_store::roster::RosterSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            source: RosterSource::RosterBuild,
            agents: vec![Intent {
                agent_id: "cc-01".into(),
                fleet_id: "myteam".into(),
                runtime: Runtime::ClaudeCode,
                session_id: None,
                repo: Some("github.com/OxidantData/ctxlake".into()),
                branch: Some("wave2/cli".into()),
                cwd: None,
                task: Some("implementing ctxlake-cli".into()),
                paths: vec!["crates/ctxlake-cli/src/main.rs".into()],
                updated_at: OffsetDateTime::now_utc(),
            }],
        };
        let text = serde_json::to_string(&snapshot).unwrap();
        let out = render_roster(
            "myteam",
            std::path::Path::new("/x/roster.json"),
            Some(&text),
        )
        .unwrap();
        assert!(!out.to_lowercase().contains("lease"), "{out}");
    }
}
