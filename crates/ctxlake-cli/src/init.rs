//! `ctxlake init` — write `ctxlake.toml`, provision the maintenance lease key, and
//! verify the store actually answers before declaring success.

use std::path::Path;

use anyhow::{bail, Context, Result};
use object_store::{ObjectStoreExt, PutPayload};

use crate::config::{self, Config};
use crate::store_ctx::{self, full_path};

pub struct InitArgs<'a> {
    pub store: &'a str,
    pub fleet_id: &'a str,
    pub agent_id: Option<&'a str>,
    pub force: bool,
}

/// A stable, host-derived fallback when `--agent-id` is omitted. AGENTS.md's own
/// knob table calls agent-id stability "operator-assigned" — deriving from the
/// hostname rather than a fresh random id each run at least keeps it stable across
/// re-running `init` on the same host, which a random default would not.
fn default_agent_id() -> String {
    for var in ["CTXLAKE_AGENT_ID", "HOSTNAME", "COMPUTERNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return sanitize(v);
            }
        }
    }
    if let Ok(etc) = std::fs::read_to_string("/etc/hostname") {
        let v = etc.trim();
        if !v.is_empty() {
            return sanitize(v);
        }
    }
    "unnamed-agent".to_string()
}

/// Lowercased, with anything that is not alphanumeric/`-`/`_` collapsed to `-` — a
/// raw hostname (`Alices-MacBook-Pro.local`) is a fine agent id, but the `.` and
/// mixed case are worth normalizing rather than carrying into every roster entry.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

pub async fn run(args: InitArgs<'_>, config_path: &Path) -> Result<Config> {
    if config_path.exists() && !args.force {
        bail!(
            "{} already exists — pass --force to overwrite it",
            config_path.display()
        );
    }

    let agent_id = match args.agent_id {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => default_agent_id(),
    };
    let cfg = Config::new(args.store, args.fleet_id, agent_id);

    // "Verify the store is reachable" — a real round trip, not just that the URL
    // parses. This is deliberately lighter than `doctor`'s full CAS matrix: init's
    // job is "can we talk to this bucket at all," not "which conditional-write
    // primitives does it support."
    // A `file://` store has to exist before object_store will open it, and `init` is
    // the one command whose job is making a store usable — failing here because a
    // directory the user just named does not exist yet is a first-run papercut on the
    // very first command they type. getting-started.md documents exactly this form
    // (`--store file:///tmp/ctxlake-demo`), so the docs promised something that failed.
    //
    // Only for `file://`: an S3 bucket that does not exist is a real error a user must
    // resolve deliberately, and silently creating cloud resources is not `init`'s call.
    create_local_store_dir(&cfg.store)?;

    let ctx = store_ctx::connect(&cfg, &cfg.agent_id).context("building store connection")?;
    let probe_key = full_path(
        &ctx,
        &object_store::path::Path::from("_meta")
            .join("init-probe")
            .join(format!("{}.json", cfg.agent_id)),
    );
    ctx.store
        .put(&probe_key, PutPayload::from_static(b"{}"))
        .await
        .with_context(|| format!("store at {} is not reachable", cfg.store))?;
    let _ = ctx.store.delete(&probe_key).await; // best-effort scratch cleanup

    // The maintenance lease key this crate provisions up front, single-writer,
    // before any contender exists — see `ctxlake_store::lease::provision`'s doc for
    // why that precondition matters. `ctxlake maint` (crates/ctxlake-maint) is the
    // only thing that ever acquires it; provisioning it here, once, is `init`'s
    // entire involvement with that mechanism.
    let maintenance_key = full_path(&ctx, &ctxlake_store::layout::lease_maintenance());
    ctxlake_store::lease::provision(ctx.store.as_ref(), &maintenance_key)
        .await
        .context("provisioning the maintenance lease")?;

    config::save(config_path, &cfg)?;
    Ok(cfg)
}

/// Create the backing directory for a `file://` store, if that is what this is.
///
/// A no-op for every other scheme — see the call site for why cloud buckets are
/// deliberately not created here.
fn create_local_store_dir(store_url: &str) -> anyhow::Result<()> {
    let Ok(url) = url::Url::parse(store_url) else {
        return Ok(()); // Not our error to report; `connect` gives a better message.
    };
    if url.scheme() != "file" {
        return Ok(());
    }
    let Ok(path) = url.to_file_path() else {
        return Ok(());
    };
    std::fs::create_dir_all(&path)
        .with_context(|| format!("creating local store directory {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_config_and_provisions_the_maintenance_lease() {
        let store_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("ctxlake.toml");
        let store_url = format!("file://{}", store_dir.path().display());

        let cfg = run(
            InitArgs {
                store: &store_url,
                fleet_id: "myteam",
                agent_id: Some("cc-01"),
                force: false,
            },
            &config_path,
        )
        .await
        .unwrap();

        assert_eq!(cfg.fleet_id, "myteam");
        assert_eq!(cfg.agent_id, "cc-01");
        assert!(config_path.exists());

        let ctx = store_ctx::connect(&cfg, "cc-01").unwrap();
        let key = full_path(&ctx, &ctxlake_store::layout::lease_maintenance());
        let state = ctxlake_store::lease::read(ctx.store.as_ref(), &key)
            .await
            .unwrap();
        assert_eq!(state.holder, None, "provisioned lease must read back free");
    }

    #[tokio::test]
    async fn refuses_to_overwrite_without_force() {
        let store_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("ctxlake.toml");
        std::fs::write(
            &config_path,
            "store = \"x\"\nfleet_id=\"y\"\nagent_id=\"z\"\n",
        )
        .unwrap();
        let store_url = format!("file://{}", store_dir.path().display());

        let err = run(
            InitArgs {
                store: &store_url,
                fleet_id: "myteam",
                agent_id: Some("cc-01"),
                force: false,
            },
            &config_path,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // And the original file must survive untouched.
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "store = \"x\"\nfleet_id=\"y\"\nagent_id=\"z\"\n"
        );
    }

    #[tokio::test]
    async fn force_overwrites_an_existing_config() {
        let store_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("ctxlake.toml");
        std::fs::write(
            &config_path,
            "store = \"x\"\nfleet_id=\"y\"\nagent_id=\"z\"\n",
        )
        .unwrap();
        let store_url = format!("file://{}", store_dir.path().display());

        run(
            InitArgs {
                store: &store_url,
                fleet_id: "myteam",
                agent_id: Some("cc-01"),
                force: true,
            },
            &config_path,
        )
        .await
        .unwrap();
        let cfg = config::load(&config_path).unwrap();
        assert_eq!(cfg.fleet_id, "myteam");
    }

    #[tokio::test]
    async fn an_unreachable_store_is_reported_and_nothing_is_written() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("ctxlake.toml");
        // A file:// URL under a path that cannot exist (a file standing in for a
        // directory) fails the round-trip probe rather than the URL parse.
        let bogus_parent = tempfile::NamedTempFile::new().unwrap();
        let store_url = format!("file://{}/nested", bogus_parent.path().display());

        let err = run(
            InitArgs {
                store: &store_url,
                fleet_id: "myteam",
                agent_id: Some("cc-01"),
                force: false,
            },
            &config_path,
        )
        .await
        .unwrap_err();
        // `{:#}` prints the whole `anyhow` context chain, not just the outermost
        // message — `connect()`'s own "connecting to <url>" context sits one layer
        // in from `run()`'s "building store connection" wrapper.
        //
        // A `file://` store under an unreachable path now fails at directory creation
        // instead, one step earlier and with a more specific message. That is the same
        // condition reported better, so it counts: what this test guards is that an
        // unusable store is reported AND nothing is written, not which layer noticed.
        let full = format!("{err:#}");
        assert!(
            full.contains("not reachable")
                || full.contains("connecting")
                || full.contains("creating local store directory"),
            "got: {full}"
        );
        assert!(
            !config_path.exists(),
            "must not write a config for an unreachable store"
        );
    }

    #[test]
    fn default_agent_id_sanitizes_a_raw_hostname() {
        assert_eq!(
            sanitize("Alices-MacBook-Pro.local"),
            "alices-macbook-pro-local"
        );
        assert_eq!(sanitize("  weird   spacing  "), "weird-spacing");
    }
}
