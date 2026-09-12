//! `ctxlake init` — write `ctxlake.toml`, and prove the store actually answers
//! before declaring success.

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
    /// The Tier 2 model, if the operator is setting one up now.
    ///
    /// Configuring a model used to mean hand-editing `ctxlake.toml` after `init`,
    /// which is a strange thing to ask of the one command whose whole job is writing
    /// that file — and it meant the mistakes (a provider that needs a `base_url`, an
    /// env var nobody exported) were found later, by a daemon, in a log.
    pub llm: Option<LlmArgs<'a>>,
}

/// What `ctxlake init --llm ...` was given.
pub struct LlmArgs<'a> {
    pub provider: crate::config::ProviderKind,
    pub model: Option<&'a str>,
    pub api_key_env: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub mode: crate::config::SummarizeMode,
}

/// The model each provider gets when `--llm-model` is omitted.
///
/// Extraction is a small, highly structured job over a transcript — the cheapest
/// capable model is the right default, and picking one for the operator is most of
/// the value of having this flag at all.
fn default_model_for(p: crate::config::ProviderKind) -> &'static str {
    use crate::config::ProviderKind as P;
    match p {
        P::Anthropic => "claude-haiku-4-5",
        P::Openrouter => "anthropic/claude-haiku-4.5",
        P::Gemini => "gemini-2.0-flash",
        P::Ollama => "llama3.1",
        P::OpenaiCompatible => "gpt-4o-mini",
        // An alias rather than a pinned name, so it follows whatever the installed
        // CLI considers current.
        P::ClaudeCli => "haiku",
    }
}

/// The env var each provider conventionally reads, so `--llm-key-env` is optional.
fn default_key_env_for(p: crate::config::ProviderKind) -> &'static str {
    use crate::config::ProviderKind as P;
    match p {
        P::Anthropic => "ANTHROPIC_API_KEY",
        P::Openrouter => "OPENROUTER_API_KEY",
        P::Gemini => "GEMINI_API_KEY",
        P::OpenaiCompatible => "OPENAI_API_KEY",
        // Neither needs one: ollama is a local endpoint, and the claude CLI uses the
        // subscription it is already signed in to.
        P::Ollama | P::ClaudeCli => "",
    }
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
    // `hostname(1)`, last. On macOS none of the above ever resolves — interactive
    // zsh does not export `HOSTNAME`, `COMPUTERNAME` is a Windows convention, and
    // there is no `/etc/hostname` — so every Mac fell through to a hard-coded
    // `unnamed-agent`. That is worse than ugly: `agent_id` is the fleet identity,
    // and two hosts sharing one merges their roster entries and their claims'
    // attribution, so an entire fleet of Macs would have looked like a single agent.
    //
    // A subprocess is fine *here* and nowhere near the hook: `init` runs once,
    // interactively, while the hook has a 5ms budget per event and gets its agent id
    // handed to it by `ctxlake install` as an environment variable anyway.
    if let Ok(out) = std::process::Command::new("hostname").arg("-s").output() {
        if out.status.success() {
            let v = String::from_utf8_lossy(&out.stdout);
            let v = v.trim();
            if !v.is_empty() {
                return sanitize(v);
            }
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
    let mut cfg = Config::new(args.store, args.fleet_id, agent_id);

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

    // `init` used to pre-provision a maintenance lease key here, because a lease
    // object had to exist before anyone could compare-and-swap it. Nothing leases
    // anything now — `ctxlake maint`'s work is idempotent by content — so there is no
    // key to seed and `init`'s only remaining job against the store is proving it is
    // reachable and writable, which the probe above just did.
    // The model, if one was asked for — written into the same file, in the same
    // command, and *verified* before it is written. A config that names a provider
    // nobody can reach is worse than no config: it makes `ctxlake maint` fail on
    // every cycle, into a log, long after whoever typed it has moved on.
    if let Some(llm) = &args.llm {
        let batch = config::BatchConfig {
            provider: llm.provider,
            model: llm
                .model
                .unwrap_or_else(|| default_model_for(llm.provider))
                .to_string(),
            api_key_env: llm
                .api_key_env
                .unwrap_or_else(|| default_key_env_for(llm.provider))
                .to_string(),
            base_url: llm.base_url.map(str::to_string),
            use_batch_api: true,
            max_sessions_per_run: 50,
            max_input_tokens: 8000,
        };
        cfg.summarize = config::SummarizeConfig {
            mode: llm.mode,
            batch: Some(batch),
        };

        let extract_cfg: ctxlake_maint::extract::SummarizeConfig = (&cfg.summarize).into();
        if let Some(b) = extract_cfg.batch.as_ref() {
            let provider = ctxlake_maint::extract::build_provider(b).with_context(|| {
                format!(
                    "the {:?} provider is not usable on this machine",
                    llm.provider
                )
            })?;
            // One real call, one word back. Everything that can be wrong about a
            // model configuration — a revoked key, a name that does not exist, an
            // endpoint pointing at nothing, an account over quota — resolves an env
            // var perfectly and only shows up here.
            let req = ctxlake_maint::extract::CompletionRequest {
                system_prompt: "Reply with the single word: ok".to_string(),
                user_prompt: "ok".to_string(),
                model: b.model.clone(),
            };
            provider
                .complete(&req)
                .await
                .with_context(|| format!("{:?} did not answer a test request", llm.provider))?;
        }
    }

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

    #[test]
    fn every_provider_has_a_default_model_and_a_key_convention() {
        // The point of `--llm <provider>` is that it is the whole command. A
        // provider with no default model would force a second flag and put the
        // decision back on someone who just wanted it to work.
        use crate::config::ProviderKind as P;
        for p in [
            P::Anthropic,
            P::Openrouter,
            P::Gemini,
            P::Ollama,
            P::OpenaiCompatible,
            P::ClaudeCli,
        ] {
            assert!(
                !default_model_for(p).is_empty(),
                "{p:?} needs a default model"
            );
        }
        // Exactly the two that authenticate some other way: ollama is a local
        // endpoint, and the claude CLI uses the subscription it is signed in to.
        assert_eq!(default_key_env_for(P::Ollama), "");
        assert_eq!(default_key_env_for(P::ClaudeCli), "");
        for p in [P::Anthropic, P::Openrouter, P::Gemini, P::OpenaiCompatible] {
            assert!(
                default_key_env_for(p).ends_with("_API_KEY"),
                "{p:?} must name an env var, never carry a key"
            );
        }
    }

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
                llm: None,
            },
            &config_path,
        )
        .await
        .unwrap();

        assert_eq!(cfg.fleet_id, "myteam");
        assert_eq!(cfg.agent_id, "cc-01");
        assert!(config_path.exists());

        // `init` used to seed a maintenance lease object here, and this test used to
        // assert it read back free. The inverse is what is worth guarding now: no
        // lease key exists, because nothing leases anything. A lease creeping back in
        // would show up as a `live/leases/` prefix appearing under the store root.
        let leases = store_dir.path().join("live").join("leases");
        assert!(
            !leases.exists(),
            "init must create no lease objects; found {}",
            leases.display()
        );
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
                llm: None,
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
                llm: None,
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
                llm: None,
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
    fn the_default_agent_id_is_never_the_placeholder_on_a_real_machine() {
        // `agent_id` is the fleet identity: two hosts sharing one merges their roster
        // entries and their claims' attribution. On macOS every env source this looks
        // at is unset and there is no /etc/hostname, so the default used to be a
        // hard-coded `unnamed-agent` — meaning an entire fleet of Macs presented as a
        // single agent. Any machine with a working `hostname` must do better.
        let id = default_agent_id();
        assert!(!id.is_empty());
        assert_ne!(
            id, "unnamed-agent",
            "a host with a resolvable hostname must not fall through to the placeholder"
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
