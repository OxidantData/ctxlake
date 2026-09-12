//! `ctxlake status` — render the roster.
//!
//! Two sections with two different freshness stories, and the output says so
//! explicitly rather than letting one section's staleness be mistaken for the
//! other's: the roster comes from the **local cache** (`ctxlake sync`'s
//! store-to-cache leg, refreshed on a poll interval — docs/architecture.md), so
//! this prints its age. Leases have no cache format yet (nothing publishes a
//! merged `leases.json` the way `roster::build` does for the roster), so that
//! section is fetched **live**, right now, and is labeled as such rather than
//! silently mixing a live read into what looks like a cache-only view.

use std::time::SystemTime;

use anyhow::{Context, Result};
use ctxlake_store::roster::RosterSnapshot;
use futures::StreamExt;
use object_store::ObjectStore;
use time::OffsetDateTime;

use crate::claim::decode_reason;
use crate::config::Config;
use crate::paths;
use crate::store_ctx::{self, full_path};

pub async fn run(cfg: &Config) -> Result<()> {
    print_roster(cfg)?;
    print_live_leases(cfg).await?;
    Ok(())
}

fn print_roster(cfg: &Config) -> Result<()> {
    let cache_path = paths::cache_dir(&cfg.fleet_id).join("roster.json");
    let text = match std::fs::read_to_string(&cache_path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", cache_path.display())),
    };
    let rendered = render_roster(&cfg.fleet_id, &cache_path, text.as_deref())
        .with_context(|| format!("parsing {}", cache_path.display()))?;
    print!("{rendered}");
    Ok(())
}

/// The pure half of [`print_roster`]: given the cache file's content (or `None` for
/// "never written"), produce the exact text to print. Split out so it's testable
/// without writing into a real `$XDG_DATA_HOME` — the only I/O in this module that
/// isn't itself the object store.
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
        out.push_str(&format!(
            "\n  {}  {}  {}  {}m\n",
            agent.agent_id,
            agent.runtime.as_str(),
            agent.repo.as_deref().unwrap_or("-"),
            entry_age.whole_minutes().max(0)
        ));
        if !agent.paths.is_empty() {
            out.push_str(&format!("           touching {}\n", agent.paths.join(", ")));
        }
        if let Some(task) = &agent.task {
            // Free text written by another agent's operator — untrusted, rendered
            // plainly rather than interpreted (AGENTS.md house rules).
            out.push_str(&format!("           \"{task}\"\n"));
        }
    }
    Ok(out)
}

async fn print_live_leases(cfg: &Config) -> Result<()> {
    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;
    let prefix = full_path(&ctx, &ctxlake_store::layout::leases_prefix());
    let mut stream = ctx.store.list(Some(&prefix));
    let now: OffsetDateTime = ctx
        .clock
        .now()
        .await
        .map(OffsetDateTime::from)
        .unwrap_or_else(|_| OffsetDateTime::now_utc());

    let mut held = Vec::new();
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        let Ok(state) = ctxlake_store::lease::read(ctx.store.as_ref(), &meta.location).await else {
            continue;
        };
        // `LeaseState::stealable` (the authoritative "is anyone live-holding this"
        // check `lease::acquire` itself uses) is crate-private to ctxlake-store;
        // this mirrors its two public fields directly rather than reaching for a
        // second, possibly-drifting definition of the same question.
        let currently_held = matches!(
            (&state.holder, state.expires_at),
            (Some(_), Some(expires_at)) if now < expires_at
        );
        if currently_held {
            // `reason` carries the resource string itself, folded in by `claim`
            // (`LeaseState` has no dedicated field for it — see claim.rs's module
            // doc) — decode it back so this prints "crates/foo/**", not the lease
            // key's sha256 hash, which is meaningless to a human reading `status`.
            let (resource, note) = state
                .reason
                .as_deref()
                .map(decode_reason)
                .unwrap_or((meta.location.as_ref(), None));
            held.push((
                resource.to_string(),
                state.holder.clone().unwrap_or_default(),
                note.map(str::to_string),
            ));
        }
    }

    println!("\nleases (live, fetched just now — not cached)");
    if held.is_empty() {
        println!("  none held");
        return Ok(());
    }
    for (resource, holder, note) in held {
        println!("{}", format_lease_line(&resource, &holder, note.as_deref()));
    }
    Ok(())
}

fn format_lease_line(resource: &str, holder: &str, note: Option<&str>) -> String {
    match note {
        Some(n) => format!("  {resource}  held by {holder}  \"{n}\""),
        None => format!("  {resource}  held by {holder}"),
    }
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
    fn lease_line_decodes_the_resource_out_of_a_folded_reason() {
        let (resource, note) = decode_reason("crates/foo/**: migrating the shell-out");
        let line = format_lease_line(resource, "cc-01", note);
        assert_eq!(
            line,
            "  crates/foo/**  held by cc-01  \"migrating the shell-out\""
        );
    }

    #[test]
    fn lease_line_omits_the_note_when_there_is_none() {
        let line = format_lease_line("crates/foo/**", "cc-01", None);
        assert_eq!(line, "  crates/foo/**  held by cc-01");
    }
}
