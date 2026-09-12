//! `ctxlake claim` / `ctxlake release` — over `ctxlake_store::lease`.
//!
//! Every resource string a human types (`crates/oxidant-loom/**`, a package name, a
//! migration id) is hashed via `ctxlake_core::hash::resource_key(repo, resource)`
//! into its own independent lease key. Two different resource strings never
//! collide with each other, and the same string in two different repos never
//! collides either (that field-boundary guarantee is `resource_key`'s own test).
//!
//! `LeaseState` has no field for the plaintext resource string — only `holder`,
//! `reason`, and the timestamps (`crates/ctxlake-store/src/lease.rs`). So the
//! resource is folded into `reason` on write (`"<resource>: <note>"`, or just
//! `<resource>` with no note) and split back out on read. This is a CLI-side
//! convention, not a store-level guarantee: a resource string that itself contains
//! `": "` would round-trip as a slightly different split, which is an acceptable,
//! disclosed edge case for a human-readable field, not a correctness property
//! anything else in the system depends on.

use std::time::Duration;

use anyhow::{Context, Result};
use ctxlake_core::hash::resource_key;
use ctxlake_store::layout;
use ctxlake_store::lease::{self, AcquireOutcome, LeaseState};
use futures::StreamExt;
use object_store::path::Path as StorePath;
use object_store::ObjectStore;

use crate::config::Config;
use crate::store_ctx::{self, full_path, StoreCtx};

/// The fleet-wide default TTL (AGENTS.md's knob table: 5 minutes).
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

fn encode_reason(resource: &str, note: Option<&str>) -> String {
    match note {
        Some(n) if !n.is_empty() => format!("{resource}: {n}"),
        _ => resource.to_string(),
    }
}

/// Split a stored `reason` back into `(resource, note)`. Best-effort: a `reason`
/// that predates this convention, or one whose resource string happens to contain
/// `": "`, still yields something displayable rather than an error.
///
/// `pub(crate)`: `status`'s live-leases section decodes the same field for display.
pub(crate) fn decode_reason(reason: &str) -> (&str, Option<&str>) {
    match reason.split_once(": ") {
        Some((resource, note)) => (resource, Some(note)),
        None => (reason, None),
    }
}

/// The repo identity `resource_key` hashes against. Prefers `git remote
/// origin`'s URL (stable across clones on different machines); falls back to the
/// git toplevel path, then the raw cwd, so a non-repo directory still gets a
/// consistent identity rather than failing to claim at all.
pub fn detect_repo() -> String {
    if let Some(url) = git_output(&["config", "--get", "remote.origin.url"]) {
        return url;
    }
    if let Some(top) = git_output(&["rev-parse", "--show-toplevel"]) {
        return top;
    }
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

fn git_output(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn describe_refusal(resource: &str, state: &LeaseState) -> String {
    let holder = state.holder.as_deref().unwrap_or("someone");
    let note = state
        .reason
        .as_deref()
        .map(|r| decode_reason(r))
        .and_then(|(_, note)| note)
        .unwrap_or("no reason given");
    match state.expires_at {
        Some(exp) => format!("{resource}: held by {holder} (\"{note}\") until {exp}"),
        None => format!("{resource}: held by {holder} (\"{note}\")"),
    }
}

pub async fn claim(
    cfg: &Config,
    resources: &[String],
    reason: Option<&str>,
    ttl_secs: Option<u64>,
    exclusive: bool,
) -> Result<()> {
    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;
    let repo = detect_repo();
    let ttl = ttl_secs.map(Duration::from_secs).unwrap_or(DEFAULT_TTL);

    let mut acquired = Vec::new();
    let mut refusals = Vec::new();

    for resource in resources {
        let key = full_path(&ctx, &layout::lease(&resource_key(&repo, resource)));
        lease::provision(ctx.store.as_ref(), &key)
            .await
            .with_context(|| format!("provisioning lease for {resource}"))?;
        let encoded = encode_reason(resource, reason);
        let outcome = lease::acquire(
            ctx.store.as_ref(),
            ctx.clock.as_ref(),
            &key,
            &cfg.agent_id,
            Some(&encoded),
            ttl,
        )
        .await
        .with_context(|| format!("acquiring lease for {resource}"))?;

        match outcome {
            AcquireOutcome::Acquired(handle) => {
                acquired.push((resource.clone(), key, handle));
            }
            AcquireOutcome::NotAcquired { .. } => {
                let state = lease::read(ctx.store.as_ref(), &key).await?;
                refusals.push((resource.clone(), key, describe_refusal(resource, &state)));
            }
        }
    }

    if exclusive && !refusals.is_empty() {
        // All-or-nothing: give back anything we did manage to acquire so a partial
        // claim never sits there silently after the command reports failure.
        for (_, _, handle) in acquired {
            let _ = lease::release(ctx.store.as_ref(), handle).await;
        }
        for (_, _, msg) in &refusals {
            eprintln!("refused: {msg}");
        }
        anyhow::bail!(
            "--exclusive: {} of {} resource(s) already held; claimed none",
            refusals.len(),
            resources.len()
        );
    }

    for (resource, _, handle) in &acquired {
        println!(
            "claimed {resource} (epoch {}, expires {})",
            handle.epoch, handle.expires_at
        );
    }
    for (_, _, msg) in &refusals {
        println!("refused: {msg}");
    }
    if !refusals.is_empty() {
        anyhow::bail!(
            "{} of {} resource(s) could not be claimed",
            refusals.len(),
            resources.len()
        );
    }
    Ok(())
}

pub async fn release(cfg: &Config, resources: &[String], all: bool) -> Result<()> {
    let ctx = store_ctx::connect(cfg, &cfg.agent_id)?;

    let keys: Vec<StorePath> = if all {
        list_our_leases(&ctx, &cfg.agent_id).await?
    } else {
        let repo = detect_repo();
        resources
            .iter()
            .map(|r| full_path(&ctx, &layout::lease(&resource_key(&repo, r))))
            .collect()
    };

    if keys.is_empty() {
        println!("nothing to release");
        return Ok(());
    }

    for key in keys {
        release_one(&ctx, &cfg.agent_id, &key).await?;
    }
    Ok(())
}

async fn list_our_leases(ctx: &StoreCtx, agent_id: &str) -> Result<Vec<StorePath>> {
    let prefix = full_path(ctx, &layout::leases_prefix());
    let mut stream = ctx.store.list(Some(&prefix));
    let mut ours = Vec::new();
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        if let Ok(state) = lease::read(ctx.store.as_ref(), &meta.location).await {
            if state.holder.as_deref() == Some(agent_id) {
                ours.push(meta.location);
            }
        }
    }
    Ok(ours)
}

/// Release one lease key. Every `ctxlake release` invocation is a fresh process, so
/// there is never an in-memory `LeaseHandle` from the `claim` that acquired it — the
/// documented, supported path (`lease.rs`'s own module doc: "the same holder
/// re-acquiring after losing its in-memory handle") is to re-acquire, which
/// succeeds trivially for the current holder, and immediately release the handle
/// that returns.
async fn release_one(ctx: &StoreCtx, agent_id: &str, key: &StorePath) -> Result<()> {
    let state = lease::read(ctx.store.as_ref(), key).await?;
    let (resource, _) = state
        .reason
        .as_deref()
        .map(decode_reason)
        .unwrap_or((key.as_ref(), None));

    match &state.holder {
        None => {
            println!("{resource}: already free");
            Ok(())
        }
        Some(holder) if holder != agent_id => {
            println!("{resource}: refused to release — held by {holder}, not {agent_id}");
            Ok(())
        }
        Some(_) => {
            let outcome = lease::acquire(
                ctx.store.as_ref(),
                ctx.clock.as_ref(),
                key,
                agent_id,
                state.reason.as_deref(),
                Duration::from_secs(1),
            )
            .await?;
            match outcome {
                AcquireOutcome::Acquired(handle) => {
                    lease::release(ctx.store.as_ref(), handle).await?;
                    println!("{resource}: released");
                }
                AcquireOutcome::NotAcquired { holder, .. } => {
                    println!(
                        "{resource}: was reclaimed by {} before release completed",
                        holder.unwrap_or_else(|| "someone else".to_string())
                    );
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &std::path::Path, agent: &str) -> Config {
        Config::new(format!("file://{}", dir.display()), "myteam", agent)
    }

    #[test]
    fn reason_round_trips_resource_and_note() {
        assert_eq!(
            decode_reason(&encode_reason("crates/foo/**", Some("splitting the cache"))),
            ("crates/foo/**", Some("splitting the cache"))
        );
        assert_eq!(
            decode_reason(&encode_reason("crates/foo/**", None)),
            ("crates/foo/**", None)
        );
    }

    #[tokio::test]
    async fn claim_then_a_second_agent_is_refused_with_holder_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_a = cfg(dir.path(), "cc-01");
        claim(
            &cfg_a,
            &["crates/oxidant-loom/**".to_string()],
            Some("splitting the S3 cache out"),
            None,
            false,
        )
        .await
        .unwrap();

        let cfg_b = cfg(dir.path(), "cc-02");
        let err = claim(
            &cfg_b,
            &["crates/oxidant-loom/**".to_string()],
            None,
            None,
            false,
        )
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("could not be claimed"), "{msg}");
    }

    #[tokio::test]
    async fn refusal_message_names_the_holder_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = store_ctx::connect(&cfg(dir.path(), "cc-01"), "cc-01").unwrap();
        let repo = detect_repo();
        let key = full_path(&ctx, &layout::lease(&resource_key(&repo, "crates/foo/**")));
        lease::provision(ctx.store.as_ref(), &key).await.unwrap();
        lease::acquire(
            ctx.store.as_ref(),
            ctx.clock.as_ref(),
            &key,
            "cc-01",
            Some(&encode_reason(
                "crates/foo/**",
                Some("migrating the shell-out"),
            )),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let state = lease::read(ctx.store.as_ref(), &key).await.unwrap();
        let msg = describe_refusal("crates/foo/**", &state);
        assert!(msg.contains("cc-01"), "{msg}");
        assert!(msg.contains("migrating the shell-out"), "{msg}");
    }

    #[tokio::test]
    async fn exclusive_claim_rolls_back_on_partial_refusal() {
        let dir = tempfile::tempdir().unwrap();
        // cc-02 pre-holds one of the two resources cc-01 is about to request.
        let cfg_b = cfg(dir.path(), "cc-02");
        claim(&cfg_b, &["crates/b/**".to_string()], None, None, false)
            .await
            .unwrap();

        let cfg_a = cfg(dir.path(), "cc-01");
        let err = claim(
            &cfg_a,
            &["crates/a/**".to_string(), "crates/b/**".to_string()],
            None,
            None,
            true,
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("--exclusive"));

        // cc-01 must hold NEITHER resource after the rollback.
        let ctx = store_ctx::connect(&cfg_a, "cc-01").unwrap();
        let repo = detect_repo();
        let key_a = full_path(&ctx, &layout::lease(&resource_key(&repo, "crates/a/**")));
        let state_a = lease::read(ctx.store.as_ref(), &key_a).await.unwrap();
        assert_eq!(
            state_a.holder, None,
            "must have rolled back the resource it did win"
        );
    }

    #[tokio::test]
    async fn release_all_releases_only_this_agents_own_leases() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_a = cfg(dir.path(), "cc-01");
        let cfg_b = cfg(dir.path(), "cc-02");
        claim(&cfg_a, &["crates/a/**".to_string()], None, None, false)
            .await
            .unwrap();
        claim(&cfg_b, &["crates/b/**".to_string()], None, None, false)
            .await
            .unwrap();

        release(&cfg_a, &[], true).await.unwrap();

        let ctx = store_ctx::connect(&cfg_a, "cc-01").unwrap();
        let repo = detect_repo();
        let key_a = full_path(&ctx, &layout::lease(&resource_key(&repo, "crates/a/**")));
        let key_b = full_path(&ctx, &layout::lease(&resource_key(&repo, "crates/b/**")));
        assert_eq!(
            lease::read(ctx.store.as_ref(), &key_a)
                .await
                .unwrap()
                .holder,
            None
        );
        assert_eq!(
            lease::read(ctx.store.as_ref(), &key_b)
                .await
                .unwrap()
                .holder,
            Some("cc-02".to_string()),
            "release --all must never touch another agent's lease"
        );
    }

    #[tokio::test]
    async fn releasing_a_lease_you_do_not_hold_is_refused_not_forced() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_a = cfg(dir.path(), "cc-01");
        claim(&cfg_a, &["crates/a/**".to_string()], None, None, false)
            .await
            .unwrap();

        let cfg_b = cfg(dir.path(), "cc-02");
        release(&cfg_b, &["crates/a/**".to_string()], false)
            .await
            .unwrap();

        let ctx = store_ctx::connect(&cfg_a, "cc-01").unwrap();
        let repo = detect_repo();
        let key_a = full_path(&ctx, &layout::lease(&resource_key(&repo, "crates/a/**")));
        assert_eq!(
            lease::read(ctx.store.as_ref(), &key_a)
                .await
                .unwrap()
                .holder,
            Some("cc-01".to_string()),
            "cc-02 must not have released cc-01's lease"
        );
    }
}
