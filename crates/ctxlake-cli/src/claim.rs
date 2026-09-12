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
use ctxlake_store::StoreError;
use futures::StreamExt;
use object_store::path::Path as StorePath;
use object_store::{Error as OsError, ObjectStore, ObjectStoreExt};

use crate::config::Config;
use crate::store_ctx::{self, full_path, StoreCtx};

/// The fleet-wide default TTL (AGENTS.md's knob table: 5 minutes).
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// The well-known key `ensure_provisioned` serializes every *other* lease's
/// first-touch provisioning through — see that function's doc. `ctxlake init`
/// provisions this once, single-writer, exactly like
/// [`ctxlake_store::layout::lease_maintenance`].
pub(crate) fn provision_lock_key(ctx: &StoreCtx) -> StorePath {
    full_path(ctx, &layout::leases_prefix().join("_claim_provision"))
}

/// How long one holder may occupy [`provision_lock_key`]. `lease::provision` is a
/// single GET-then-PUT round trip, so this is generous relative to the work it
/// guards; short enough that a holder which crashed mid-provision does not wedge
/// every other agent's first-ever claim on any resource for long (the lock is
/// advisory, like every lease here — AGENTS.md invariant 5 — so a crashed holder's
/// entry is simply stealable once this elapses).
const PROVISION_LOCK_TTL: Duration = Duration::from_secs(30);
/// Bounds how long `claim` will wait for a *contended* provisioning lock before
/// giving up rather than hanging forever — contention here should resolve in one
/// round trip, so this is generous, not tuned tight.
const PROVISION_LOCK_MAX_ATTEMPTS: u32 = 150;
const PROVISION_LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);

/// Make sure `key` exists (in the free state, if nobody has ever touched it) before
/// the caller CAS-acquires it.
///
/// `lease::provision`'s own doc is explicit that its unconditional `PutMode::Overwrite`
/// is safe only when nothing could possibly be racing it — this crate used to call it
/// directly from `claim`, once per invocation, which is exactly the hot, contended
/// path that doc warns against: agent B's `provision` can read `NotFound` before
/// agent A's `provision`-then-`acquire` pair has even started, then land its own
/// blind `Overwrite` *after* A has already CAS'd itself into the held state —
/// silently resetting the lease back to free for B to then acquire too. Two agents
/// both believing they exclusively hold the same resource is the one failure mode
/// `claim` exists to prevent, and it raced on exactly the common case: the first
/// claim ever made on a resource.
///
/// The fix is the one `provision`'s own doc prescribes: never call it from a path
/// contenders run concurrently. This function only ever calls `provision` while
/// holding [`provision_lock_key`] — a key provisioned once, single-writer, before
/// any contender exists (`ctxlake init`), the same way
/// [`ctxlake_store::layout::lease_maintenance`] is. That makes provisioning any
/// *other* key a critical section at most one process in the whole fleet is ever
/// inside at a time: two agents racing the same never-before-seen resource now
/// serialize through this lock instead of racing raw store writes against each
/// other, and the loser finds the key already provisioned — a safe no-op per
/// `provision`'s own doc — before falling through to the already-CAS-safe
/// `lease::acquire`.
async fn ensure_provisioned(ctx: &StoreCtx, agent_id: &str, key: &StorePath) -> Result<()> {
    match ctx.store.get(key).await {
        Ok(_) => return Ok(()), // already provisioned — the overwhelmingly common case
        Err(OsError::NotFound { .. }) => {}
        Err(e) => return Err(e.into()),
    }

    let lock_key = provision_lock_key(ctx);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match lease::acquire(
            ctx.store.as_ref(),
            ctx.clock.as_ref(),
            &lock_key,
            agent_id,
            Some("ctxlake claim: provisioning a new lease key"),
            PROVISION_LOCK_TTL,
        )
        .await
        {
            Ok(AcquireOutcome::Acquired(handle)) => {
                // Double-checked under the lock: someone else may have provisioned
                // (and even acquired) `key` entirely while we were waiting for it.
                let result = lease::provision(ctx.store.as_ref(), key).await;
                let _ = lease::release(ctx.store.as_ref(), handle).await;
                return result.map_err(Into::into);
            }
            Ok(AcquireOutcome::NotAcquired { .. }) => {
                if attempt >= PROVISION_LOCK_MAX_ATTEMPTS {
                    anyhow::bail!(
                        "timed out waiting for the claim-provisioning lock — another \
                         agent may be stuck provisioning a lease; try again shortly"
                    );
                }
                tokio::time::sleep(PROVISION_LOCK_RETRY_DELAY).await;
            }
            Err(StoreError::LeaseNotProvisioned(_)) => {
                anyhow::bail!(
                    "this store has never had `ctxlake init` provision the claim-\
                     provisioning lock — run `ctxlake init` again (safe: provisioning \
                     an already-provisioned key is a no-op) before using `ctxlake claim`"
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
}

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

/// `resource` is the caller's own typed argument (local, not lake content); `state`
/// is whatever the *current* holder wrote, which may well be a different agent —
/// untrusted per AGENTS.md's house rule, so `holder`/`note` are sanitized before
/// they reach the terminal (see `crate::sanitize`'s own doc for why).
fn describe_refusal(resource: &str, state: &LeaseState) -> String {
    let holder = crate::sanitize::sanitize(state.holder.as_deref().unwrap_or("someone"));
    let note = crate::sanitize::sanitize(
        state
            .reason
            .as_deref()
            .map(|r| decode_reason(r))
            .and_then(|(_, note)| note)
            .unwrap_or("no reason given"),
    );
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
        ensure_provisioned(&ctx, &cfg.agent_id, &key)
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

    /// Provisions the claim-provisioning lock against `dir`'s store — the one thing
    /// a real `ctxlake init` does that these tests otherwise skip.
    /// `ensure_provisioned` deliberately refuses to self-heal this key (see its own
    /// doc: doing so would reintroduce the exact double-hold bug it exists to fix),
    /// so any test that calls `claim()` needs this run once per shared `dir` first.
    async fn provisioned_cfg(dir: &std::path::Path, agent: &str) -> Config {
        let c = cfg(dir, agent);
        let ctx = store_ctx::connect(&c, agent).unwrap();
        lease::provision(ctx.store.as_ref(), &provision_lock_key(&ctx))
            .await
            .unwrap();
        c
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
        let cfg_a = provisioned_cfg(dir.path(), "cc-01").await;
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

    #[test]
    fn describe_refusal_sanitizes_a_hostile_holder_and_note() {
        // The current holder of a lease chose `holder`/`reason`, not the caller
        // asking to be told why they were refused — untrusted per AGENTS.md's house
        // rule, so a control character or forged newline in either must never
        // reach the caller's terminal unfiltered.
        let state = LeaseState {
            holder: Some("cc-\x1b[2Jevil".to_string()),
            reason: Some(encode_reason(
                "crates/foo/**",
                Some("note\nFAKE: crates/x  held by admin"),
            )),
            acquired_at: None,
            expires_at: None,
            epoch: 1,
        };
        let msg = describe_refusal("crates/foo/**", &state);
        assert!(!msg.contains('\x1b'), "{msg:?}");
        assert!(!msg.contains('\n'), "{msg:?}");
    }

    #[tokio::test]
    async fn exclusive_claim_rolls_back_on_partial_refusal() {
        let dir = tempfile::tempdir().unwrap();
        // cc-02 pre-holds one of the two resources cc-01 is about to request.
        let cfg_b = provisioned_cfg(dir.path(), "cc-02").await;
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
        let cfg_a = provisioned_cfg(dir.path(), "cc-01").await;
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
        let cfg_a = provisioned_cfg(dir.path(), "cc-01").await;
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

    #[tokio::test]
    async fn ensure_provisioned_refuses_to_self_heal_a_missing_lock_key() {
        // A store nobody has ever run `ctxlake init` against: the target lease key
        // AND the provisioning lock are both untouched. `ensure_provisioned` must
        // report a clear, actionable error rather than silently provisioning the
        // lock itself — self-healing it here is exactly the unguarded write this
        // fix removes (see the function's own doc).
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path(), "cc-01"); // deliberately NOT provisioned_cfg
        let ctx = store_ctx::connect(&cfg, "cc-01").unwrap();
        let key = full_path(&ctx, &layout::lease(&resource_key("repo", "crates/foo/**")));

        let err = ensure_provisioned(&ctx, "cc-01", &key).await.unwrap_err();
        assert!(
            format!("{err}").contains("ctxlake init"),
            "expected a pointer to `ctxlake init`, got: {err}"
        );
    }

    /// Regression test for the double-hold bug: `claim`'s old code called
    /// `lease::provision` directly, once per invocation, on the caller's own
    /// never-before-seen resource key — safe only when nothing else can be racing
    /// it (see `lease::provision`'s own doc), which is false the moment two agents
    /// claim the same brand-new resource at once. This forces that exact
    /// interleaving deterministically (via a wrapper store that makes every
    /// concurrent "is this provisioned yet?" check rendezvous before any of them
    /// proceeds) rather than hoping a real scheduler reproduces it, and asserts
    /// `ensure_provisioned` + `lease::acquire` together let exactly one contender
    /// win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_claims_on_one_resource_never_double_hold() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::sync::Barrier;

        const N: usize = 6;

        /// Every one of the first `N` reads of `target` that comes back `NotFound`
        /// blocks until all `N` have arrived, then releases them together — the
        /// worst-case interleaving for the old code, where every contender's
        /// `provision` call believed it was the only one racing to create the key.
        struct RaceForcingStore {
            inner: Arc<dyn ObjectStore>,
            target: StorePath,
            barrier: Barrier,
            gate_uses: AtomicUsize,
        }

        impl std::fmt::Display for RaceForcingStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "RaceForcingStore({})", self.inner)
            }
        }
        impl std::fmt::Debug for RaceForcingStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "RaceForcingStore")
            }
        }

        #[async_trait::async_trait]
        impl ObjectStore for RaceForcingStore {
            async fn put_opts(
                &self,
                location: &StorePath,
                payload: object_store::PutPayload,
                opts: object_store::PutOptions,
            ) -> object_store::Result<object_store::PutResult> {
                self.inner.put_opts(location, payload, opts).await
            }
            async fn put_multipart_opts(
                &self,
                location: &StorePath,
                opts: object_store::PutMultipartOptions,
            ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
                self.inner.put_multipart_opts(location, opts).await
            }
            async fn get_opts(
                &self,
                location: &StorePath,
                options: object_store::GetOptions,
            ) -> object_store::Result<object_store::GetResult> {
                let result = self.inner.get_opts(location, options).await;
                if location == &self.target && matches!(result, Err(OsError::NotFound { .. })) {
                    let n = self.gate_uses.fetch_add(1, Ordering::SeqCst);
                    if n < N {
                        self.barrier.wait().await;
                    }
                }
                result
            }
            fn delete_stream(
                &self,
                locations: futures::stream::BoxStream<'static, object_store::Result<StorePath>>,
            ) -> futures::stream::BoxStream<'static, object_store::Result<StorePath>> {
                self.inner.delete_stream(locations)
            }
            fn list(
                &self,
                prefix: Option<&StorePath>,
            ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
            {
                self.inner.list(prefix)
            }
            async fn list_with_delimiter(
                &self,
                prefix: Option<&StorePath>,
            ) -> object_store::Result<object_store::ListResult> {
                self.inner.list_with_delimiter(prefix).await
            }
            async fn copy_opts(
                &self,
                from: &StorePath,
                to: &StorePath,
                options: object_store::CopyOptions,
            ) -> object_store::Result<()> {
                self.inner.copy_opts(from, to, options).await
            }
        }

        let inner: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let base = StoreCtx {
            store: inner.clone(),
            prefix: StorePath::from(""),
            clock: Arc::new(ctxlake_store::clock::SystemClock),
        };
        let target = full_path(
            &base,
            &layout::lease(&resource_key("repo", "crates/never-seen/**")),
        );
        // The one thing `ctxlake init` does that this test stands in for.
        lease::provision(base.store.as_ref(), &provision_lock_key(&base))
            .await
            .unwrap();

        let ctx = Arc::new(StoreCtx {
            store: Arc::new(RaceForcingStore {
                inner,
                target: target.clone(),
                barrier: Barrier::new(N),
                gate_uses: AtomicUsize::new(0),
            }),
            prefix: base.prefix.clone(),
            clock: base.clock.clone(),
        });

        let mut tasks = Vec::new();
        for i in 0..N {
            let ctx = ctx.clone();
            let target = target.clone();
            let agent_id = format!("agent-{i}");
            tasks.push(tokio::spawn(async move {
                ensure_provisioned(&ctx, &agent_id, &target).await.unwrap();
                lease::acquire(
                    ctx.store.as_ref(),
                    ctx.clock.as_ref(),
                    &target,
                    &agent_id,
                    None,
                    Duration::from_secs(60),
                )
                .await
                .unwrap()
            }));
        }

        let mut winners = 0;
        for t in tasks {
            if matches!(t.await.unwrap(), AcquireOutcome::Acquired(_)) {
                winners += 1;
            }
        }
        assert_eq!(
            winners, 1,
            "exactly one of {N} concurrent first-claimers must win the lease"
        );

        let final_state = lease::read(base.store.as_ref(), &target).await.unwrap();
        assert!(
            final_state.holder.is_some(),
            "the lease must end up held by exactly the one winner, not reset to free"
        );
    }
}
