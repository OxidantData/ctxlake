//! `ctxlake.toml` — see `docs/reference.md` for the authoritative reference this struct
//! must keep matching.
//!
//! AGENTS.md invariant 10 shapes every field here: nothing in this type can hold a
//! secret *value*, only the *name* of an environment variable (`api_key_env`). That
//! is not enforced by a runtime check — it is enforced by there being no field a
//! secret could be typed into in the first place.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// docs/memory.md's three tiers, selected by name. `None` still runs Tier 0
/// (structural digests) — there is no way to turn *that* off, since it costs no LLM
/// call and derives entirely from captured events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
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

impl std::fmt::Display for SummarizeMode {
    /// `ctxlake doctor`'s "what summarize mode is configured" line reads this — see
    /// `doctor.rs`. Matches `docs/memory.md`'s own spelling of each mode
    /// (`toml`'s `#[serde(rename_all = "snake_case")]` on this enum uses the same
    /// strings, so a doctor report and a `ctxlake.toml` line never disagree on what
    /// to call a mode).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SummarizeMode::None => "none",
            SummarizeMode::Agent => "agent",
            SummarizeMode::Batch => "batch",
            SummarizeMode::Both => "both",
            SummarizeMode::Shadow => "shadow",
        })
    }
}

/// `[summarize.batch] provider`, mirroring `ctxlake_maint::extract::ProviderKind`
/// field-for-field so [`BatchConfig`]'s `From` impl below is a plain rename, not
/// a place a fifth or sixth value could quietly diverge between the two crates.
/// A separate type rather than reusing `ctxlake_maint`'s directly: this crate's
/// `Config` is `ctxlake.toml`'s serde shape, and a config crate should not need
/// to change just because `ctxlake-maint` reshapes its own internal enum.
// `clap::ValueEnum` alongside serde so `--llm openrouter` and `provider =
// "openrouter"` are spelled identically — clap's derive is kebab-case by default,
// the same convention `#[serde(rename_all)]` applies here. Two spellings for one
// value is a support question waiting to happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Anthropic,
    OpenaiCompatible,
    Ollama,
    /// OpenAI-shaped, hosted at a fixed `openrouter.ai` endpoint — see
    /// `ctxlake_maint::extract::OpenRouterProvider`.
    Openrouter,
    /// `contents`/`systemInstruction`, not a `messages` array — see
    /// `ctxlake_maint::extract::GeminiProvider`.
    Gemini,
    /// The `claude` CLI already on this machine, on its existing subscription —
    /// no API key. Spelled `claude-cli` in `ctxlake.toml`.
    ClaudeCli,
}

impl From<ProviderKind> for ctxlake_maint::extract::ProviderKind {
    fn from(p: ProviderKind) -> Self {
        match p {
            ProviderKind::Anthropic => Self::Anthropic,
            ProviderKind::OpenaiCompatible => Self::OpenaiCompatible,
            ProviderKind::Ollama => Self::Ollama,
            ProviderKind::Openrouter => Self::Openrouter,
            ProviderKind::Gemini => Self::Gemini,
            ProviderKind::ClaudeCli => Self::ClaudeCli,
        }
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
    pub provider: ProviderKind,
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

impl From<SummarizeMode> for ctxlake_maint::extract::SummarizeMode {
    fn from(m: SummarizeMode) -> Self {
        match m {
            SummarizeMode::None => Self::None,
            SummarizeMode::Agent => Self::Agent,
            SummarizeMode::Batch => Self::Batch,
            SummarizeMode::Both => Self::Both,
            SummarizeMode::Shadow => Self::Shadow,
        }
    }
}

/// `ctxlake.toml`'s `u32` fields become `ctxlake_maint`'s `usize` ones — infallible
/// on every platform this ships for (usize is at least 32 bits), so this is a
/// plain `From`, not a `TryFrom` with an error path nothing can actually hit.
impl From<&BatchConfig> for ctxlake_maint::extract::BatchConfig {
    fn from(b: &BatchConfig) -> Self {
        Self {
            provider: b.provider.into(),
            model: b.model.clone(),
            api_key_env: b.api_key_env.clone(),
            base_url: b.base_url.clone(),
            use_batch_api: b.use_batch_api,
            max_sessions_per_run: b.max_sessions_per_run as usize,
            max_input_tokens: b.max_input_tokens as usize,
        }
    }
}

/// The bridge `maint_cmd.rs` uses to hand `ctxlake-maint::extract` and `::gate`
/// the config they need — this crate's own `SummarizeConfig` is `ctxlake.toml`'s
/// serde shape (kebab-case `provider`, `u32` counters); `ctxlake-maint`'s is the
/// shape its HTTP layer wants. Nothing converts the other way: `ctxlake-maint`
/// does not, and must not, depend back on `ctxlake-cli`.
impl From<&SummarizeConfig> for ctxlake_maint::extract::SummarizeConfig {
    fn from(s: &SummarizeConfig) -> Self {
        Self {
            mode: s.mode.into(),
            batch: s.batch.as_ref().map(Into::into),
        }
    }
}

/// The full contents of `ctxlake.toml`. See `docs/reference.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// `s3://`, `gs://`, `az://`/`abfs(s)://`, or `file://` — parsed by
    /// `ctxlake_store::backend::build`.
    pub store: String,
    /// The boundary of who sees whom (docs/getting-started.md). Everyone sharing a
    /// fleet id sees each other's roster entries and briefings.
    pub fleet_id: String,
    /// Stable across restarts by convention (AGENTS.md's knob table) — nothing here
    /// enforces that, but reusing an id from two hosts merges their roster identity.
    pub agent_id: String,
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
    let banner = "# Written by `ctxlake init`. See docs/reference.md for the full reference.\n\n";
    std::fs::write(path, format!("{banner}{body}"))
        .with_context(|| format!("writing config to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_mode_display_matches_its_toml_spelling() {
        // `ctxlake doctor` prints this Display impl; `ctxlake.toml` accepts the
        // serde `rename_all = "snake_case"` spelling — a doctor report naming a
        // mode differently from what an operator would type in the config file
        // would be confusing, not just cosmetic.
        for (mode, word) in [
            (SummarizeMode::None, "none"),
            (SummarizeMode::Agent, "agent"),
            (SummarizeMode::Batch, "batch"),
            (SummarizeMode::Both, "both"),
            (SummarizeMode::Shadow, "shadow"),
        ] {
            assert_eq!(mode.to_string(), word);
            let cfg = SummarizeConfig { mode, batch: None };
            let toml = toml::to_string(&cfg).unwrap();
            assert!(
                toml.contains(word),
                "Display and serde spelling diverged for {mode:?}: {toml}"
            );
        }
    }

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
        // A hand-written minimal file (no [summarize] at all) must still parse —
        // it is genuinely optional, not just "usually present."
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctxlake.toml");
        std::fs::write(
            &path,
            "store = \"file:///tmp/lake\"\nfleet_id = \"myteam\"\nagent_id = \"cc-01\"\n",
        )
        .unwrap();
        let cfg = load(&path).unwrap();
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
            provider: ProviderKind::Anthropic,
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
    fn provider_openrouter_and_gemini_parse_from_their_documented_toml_spelling() {
        // The exact strings docs/memory.md and docs/reference.md tell an operator
        // to type — a serde kebab-case default would otherwise render `Openrouter`
        // as `open-router`, which is not what either doc says to write.
        for (word, expected) in [
            ("anthropic", ProviderKind::Anthropic),
            ("openai-compatible", ProviderKind::OpenaiCompatible),
            ("ollama", ProviderKind::Ollama),
            ("openrouter", ProviderKind::Openrouter),
            ("gemini", ProviderKind::Gemini),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ctxlake.toml");
            std::fs::write(
                &path,
                format!(
                    "store = \"file:///tmp/lake\"\nfleet_id = \"myteam\"\nagent_id = \"cc-01\"\n\n\
                     [summarize]\nmode = \"batch\"\n\n\
                     [summarize.batch]\nprovider = \"{word}\"\nmodel = \"m\"\napi_key_env = \"K\"\n"
                ),
            )
            .unwrap();
            let cfg = load(&path).unwrap();
            assert_eq!(
                cfg.summarize.batch.unwrap().provider,
                expected,
                "word {word:?}"
            );
        }
    }

    #[test]
    fn batch_config_converts_into_ctxlake_maint_s_shape_field_for_field() {
        let cli_cfg = BatchConfig {
            provider: ProviderKind::Gemini,
            model: "gemini-2.0-flash".into(),
            api_key_env: "GEMINI_API_KEY".into(),
            base_url: Some("http://localhost:9999".into()),
            use_batch_api: false,
            max_sessions_per_run: 7,
            max_input_tokens: 1234,
        };
        let maint_cfg: ctxlake_maint::extract::BatchConfig = (&cli_cfg).into();
        assert_eq!(
            maint_cfg.provider,
            ctxlake_maint::extract::ProviderKind::Gemini
        );
        assert_eq!(maint_cfg.model, "gemini-2.0-flash");
        assert_eq!(maint_cfg.api_key_env, "GEMINI_API_KEY");
        assert_eq!(maint_cfg.base_url.as_deref(), Some("http://localhost:9999"));
        assert!(!maint_cfg.use_batch_api);
        assert_eq!(maint_cfg.max_sessions_per_run, 7);
        assert_eq!(maint_cfg.max_input_tokens, 1234);
    }

    #[test]
    fn summarize_config_conversion_carries_mode_and_an_absent_batch_through() {
        let cli_cfg = SummarizeConfig {
            mode: SummarizeMode::Shadow,
            batch: None,
        };
        let maint_cfg: ctxlake_maint::extract::SummarizeConfig = (&cli_cfg).into();
        assert_eq!(
            maint_cfg.mode,
            ctxlake_maint::extract::SummarizeMode::Shadow
        );
        assert!(maint_cfg.batch.is_none());
        // And tier2_enabled must agree: shadow mode with no batch config is not
        // actually enabled, on either side of the conversion.
        assert!(!ctxlake_maint::extract::tier2_enabled(&maint_cfg));
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
}
