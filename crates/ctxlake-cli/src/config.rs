//! `ctxlake.toml` — see `docs/config.md` for the authoritative reference this struct
//! must keep matching.
//!
//! AGENTS.md invariant 10 shapes every field here: nothing in this type can hold a
//! secret *value*, only the *name* of an environment variable (`api_key_env`). That
//! is not enforced by a runtime check — it is enforced by there being no field a
//! secret could be typed into in the first place.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What happens when two agents' declared work overlaps. Advisory either way — see
/// AGENTS.md invariant 5 — this only controls how loudly ctxlake says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CollisionPolicy {
    /// Note the overlap in the briefing and in `ctxlake status`; never stop anyone.
    #[default]
    Warn,
    /// Additionally have the hook's pre-tool-use check refuse the call outright.
    /// Still advisory in the sense AGENTS.md means it (a lease can't be enforced by
    /// the store), but the *hook* can decline to let its own agent proceed.
    Block,
}

impl std::fmt::Display for CollisionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CollisionPolicy::Warn => "warn",
            CollisionPolicy::Block => "block",
        })
    }
}

impl std::str::FromStr for CollisionPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "warn" => Ok(CollisionPolicy::Warn),
            "block" => Ok(CollisionPolicy::Block),
            other => Err(format!("expected \"warn\" or \"block\", got {other:?}")),
        }
    }
}

/// docs/summarization.md's three tiers, selected by name. `None` still runs Tier 0
/// (structural digests) — there is no way to turn *that* off, since it costs no LLM
/// call and derives entirely from captured events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SummarizeMode {
    /// Tiers 0 and 1 only. The belief layer stays empty; nothing else changes.
    None,
    /// Tier 1: the agent that did the work writes its own handoff note. Default —
    /// costs nothing beyond one short turn on a key already in use.
    #[default]
    Agent,
    /// Tier 2 only: batch extraction into candidate claims. Needs `[summarize.batch]`.
    Batch,
    /// Tiers 1 and 2 together.
    Both,
    /// Tier 2 runs in full but promoted claims never reach a context window — the
    /// setting for watching extraction quality before agents ever see its output.
    Shadow,
}

impl SummarizeMode {
    /// Whether this mode ever runs Tier 2, the only tier that reads `[summarize.batch]`
    /// or needs an LLM key at all.
    pub fn needs_batch(self) -> bool {
        matches!(
            self,
            SummarizeMode::Batch | SummarizeMode::Both | SummarizeMode::Shadow
        )
    }
}

fn default_true() -> bool {
    true
}
fn default_max_sessions_per_run() -> u32 {
    50
}
fn default_max_input_tokens() -> u32 {
    8000
}

/// `[summarize.batch]` — only read when `mode` is one of `batch`, `both`, `shadow`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchConfig {
    pub provider: String,
    pub model: String,
    /// The NAME of an environment variable holding the API key — never the key
    /// itself (AGENTS.md invariant 10). `ctxlake doctor` reports whether it
    /// resolves, never what it resolves to.
    pub api_key_env: String,
    /// Set for a self-hosted or `ollama` endpoint; absent for a provider's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default = "default_true")]
    pub use_batch_api: bool,
    #[serde(default = "default_max_sessions_per_run")]
    pub max_sessions_per_run: u32,
    #[serde(default = "default_max_input_tokens")]
    pub max_input_tokens: u32,
}

/// `[summarize]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SummarizeConfig {
    #[serde(default)]
    pub mode: SummarizeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchConfig>,
}

fn summarize_is_default(s: &SummarizeConfig) -> bool {
    s.mode == SummarizeMode::Agent && s.batch.is_none()
}

fn default_collision_policy() -> CollisionPolicy {
    CollisionPolicy::Warn
}

/// The full contents of `ctxlake.toml`. See `docs/config.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// `s3://`, `gs://`, `az://`/`abfs(s)://`, or `file://` — parsed by
    /// `ctxlake_store::backend::build`.
    pub store: String,
    /// The boundary of who sees whom (docs/getting-started.md). Everyone sharing a
    /// fleet id sees each other's roster entries, leases, and briefings.
    pub fleet_id: String,
    /// Stable across restarts by convention (AGENTS.md's knob table) — nothing here
    /// enforces that, but reusing an id from two hosts merges their roster identity.
    pub agent_id: String,
    #[serde(default = "default_collision_policy")]
    pub collision_policy: CollisionPolicy,
    #[serde(default, skip_serializing_if = "summarize_is_default")]
    pub summarize: SummarizeConfig,
}

impl Config {
    pub fn new(
        store: impl Into<String>,
        fleet_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Config {
            store: store.into(),
            fleet_id: fleet_id.into(),
            agent_id: agent_id.into(),
            collision_policy: CollisionPolicy::default(),
            summarize: SummarizeConfig::default(),
        }
    }
}

/// Read and parse `path`. Callers decide what a missing file means (`init` treats it
/// as "nothing to overwrite"; every other subcommand treats it as "run `ctxlake
/// init` first").
pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config at {}", path.display()))
}

/// Write `cfg` to `path` as TOML, creating parent directories as needed. Overwrites
/// unconditionally — callers (`init`) are responsible for the `--force` gate; this
/// function has no opinion about what was there before.
pub fn save(path: &Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(cfg).context("serializing config")?;
    let banner = "# Written by `ctxlake init`. See docs/config.md for the full reference.\n\n";
    std::fs::write(path, format!("{banner}{body}"))
        .with_context(|| format!("writing config to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctxlake.toml");
        let cfg = Config::new("s3://bucket/prefix", "myteam", "cc-01");
        save(&path, &cfg).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn defaults_apply_to_a_minimal_file() {
        // A hand-written minimal file (no collision_policy, no [summarize]) must
        // still parse — these are genuinely optional, not just "usually present."
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctxlake.toml");
        std::fs::write(
            &path,
            "store = \"file:///tmp/lake\"\nfleet_id = \"myteam\"\nagent_id = \"cc-01\"\n",
        )
        .unwrap();
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.collision_policy, CollisionPolicy::Warn);
        assert_eq!(cfg.summarize.mode, SummarizeMode::Agent);
        assert!(cfg.summarize.batch.is_none());
    }

    #[test]
    fn never_serializes_a_secret_shaped_field() {
        // There is no field to put a secret in, but assert the *shape* stays that
        // way: the only string near "key" in the whole schema is a *_env name.
        let mut cfg = Config::new("s3://b/p", "myteam", "cc-01");
        cfg.summarize.mode = SummarizeMode::Batch;
        cfg.summarize.batch = Some(BatchConfig {
            provider: "anthropic".into(),
            model: "claude-haiku-4-5".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            base_url: None,
            use_batch_api: true,
            max_sessions_per_run: 50,
            max_input_tokens: 8000,
        });
        let toml = toml::to_string_pretty(&cfg).unwrap();
        assert!(toml.contains("api_key_env"));
        assert!(
            !toml.to_lowercase().contains("api_key ="),
            "must never serialize a field literally named api_key: {toml}"
        );
    }

    #[test]
    fn load_reports_a_missing_file_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(&dir.path().join("nope.toml")).unwrap_err();
        assert!(err.to_string().contains("reading config"));
    }

    #[test]
    fn load_reports_malformed_toml_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctxlake.toml");
        std::fs::write(&path, "this is not [ toml").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.to_string().contains("parsing config"));
    }

    #[test]
    fn collision_policy_parses_from_str() {
        assert_eq!(
            "warn".parse::<CollisionPolicy>().unwrap(),
            CollisionPolicy::Warn
        );
        assert_eq!(
            "block".parse::<CollisionPolicy>().unwrap(),
            CollisionPolicy::Block
        );
        assert!("loud".parse::<CollisionPolicy>().is_err());
    }
}
