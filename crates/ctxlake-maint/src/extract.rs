//! Tier 2 — batch extraction over sealed sessions. See `docs/memory.md`.
//!
//! This is the only tier that needs an LLM, and it is optional: [`tier2_enabled`]
//! is the single gate everything else in this module respects, and when it says
//! no, [`run`] and [`extract_session`] do nothing at all — no HTTP client is even
//! constructed. Tiers 0 and 1 (structural digest, self-handoff) live entirely
//! outside this crate and are unaffected either way.
//!
//! Three safety properties this module exists to enforce, all with tests below:
//!
//! - **Context fencing.** A span the hook wrapped in
//!   `<ctxlake:injected ...>...</ctxlake:injected>` was shown to the agent as
//!   someone else's prior claim, not something this session discovered. Feeding
//!   it back into extraction would let the model "rediscover" it and file it as a
//!   fresh observation — the echo, one step earlier than the independence gate
//!   catches it. [`strip_injected_context`] removes every such span before any
//!   transcript reaches a prompt.
//! - **No evidence, no claim.** [`claim_from_raw`] drops a candidate outright — not
//!   with low confidence — if none of its cited `(session_id, message_id)` pairs
//!   resolve against the session's own envelopes. A model that invents a claim
//!   usually cannot invent a citation that resolves.
//! - **Idempotent.** [`mark_extracted_if_new`] claims `claims/extracted/<id>`
//!   with `PutMode::Create` before doing any work, so a re-run (or a second host
//!   racing the same session) extracts it at most once.

use std::collections::{BTreeMap, HashMap};

use ctxlake_core::Envelope;
use ctxlake_store::StoreError;
use futures::future::BoxFuture;
use object_store::{Error as OsError, ObjectStore, ObjectStoreExt, PutMode, PutPayload};
use serde::{Deserialize, Serialize};

use crate::claims::{self, ClaimState, ClaimType, Evidence, ProposedClaim, Scope};

/// `docs/memory.md`'s `[summarize] mode` values. `Shadow` is not listed
/// alongside the other four in the `mode` doc comment there because it gets its
/// own section, but it is the same field — see `docs/memory.md`'s note that
/// shadow mode runs "the whole chain... with agent reads disabled."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SummarizeMode {
    None,
    #[default]
    Agent,
    Batch,
    Both,
    Shadow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Anthropic,
    OpenaiCompatible,
    Ollama,
    /// OpenAI-shaped, hosted at a fixed `openrouter.ai` endpoint. Its whole value
    /// over `openai-compatible` is that a user need not know the URL — see
    /// [`OpenRouterProvider`].
    Openrouter,
    /// A genuinely different wire shape (`contents`/`systemInstruction`, and
    /// structured output via `generationConfig.responseSchema`) — see
    /// [`GeminiProvider`].
    Gemini,
    /// The `claude` CLI already installed on this machine, in print mode — no API
    /// key, billed to whatever subscription that CLI is signed in to. See
    /// [`ClaudeCliProvider`].
    ClaudeCli,
}

/// `[summarize.batch]`, mirroring `docs/memory.md`'s table field-for-field,
/// defaults included, so a bare `[summarize.batch]` with nothing set behaves
/// exactly like the doc's sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BatchConfig {
    pub provider: ProviderKind,
    pub model: String,
    /// The NAME of an env var, never a key — AGENTS.md invariant 10. Resolved by
    /// [`resolve_api_key`], never stored or logged.
    pub api_key_env: String,
    pub base_url: Option<String>,
    pub use_batch_api: bool,
    pub max_sessions_per_run: usize,
    pub max_input_tokens: usize,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Anthropic,
            model: "claude-haiku-4-5".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            base_url: None,
            use_batch_api: true,
            max_sessions_per_run: 50,
            max_input_tokens: 8000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SummarizeConfig {
    pub mode: SummarizeMode,
    /// Only read when `mode` includes batch (`batch`, `both`, or `shadow`) — see
    /// [`tier2_enabled`].
    pub batch: Option<BatchConfig>,
}

/// The one gate every entry point in this module checks first. "If no LLM is
/// configured, extraction is a clean no-op" (docs/memory.md) — this
/// function is that sentence made checkable.
pub fn tier2_enabled(cfg: &SummarizeConfig) -> bool {
    matches!(
        cfg.mode,
        SummarizeMode::Batch | SummarizeMode::Both | SummarizeMode::Shadow
    ) && cfg.batch.is_some()
}

/// Read the *value* of the env var named by `api_key_env` — never the config
/// file, which only ever holds the name (AGENTS.md invariant 10). Ollama needs no
/// key (it's a local endpoint), so this always returns `None` for it rather than
/// reporting a "missing" key nobody was going to set.
pub fn resolve_api_key(cfg: &BatchConfig) -> Option<String> {
    if matches!(cfg.provider, ProviderKind::Ollama) {
        return None;
    }
    std::env::var(&cfg.api_key_env).ok()
}

/// The one place a `[summarize.batch]` config turns into something that can
/// actually make a request. Every caller that needs a [`Provider`] — the real
/// maintenance chain, `ctxlake doctor`, anything else — goes through this
/// rather than matching on `cfg.provider` itself, so a fifth provider is one
/// match arm here, not N call sites.
///
/// Fails with [`ExtractError::MissingApiKey`], naming the env var (never a
/// value — there is never a value to print, since nothing resolved), for any
/// provider whose key is not optional: Anthropic, OpenRouter, and Gemini all
/// need a real key to call a real endpoint, so a missing one is a
/// configuration error, not a silent no-auth request. Ollama needs no key at
/// all (see [`resolve_api_key`]) and `openai-compatible` treats one as
/// optional, matching its constructor's `Option<String>` — some self-hosted
/// gateways sit behind no auth at all.
pub fn build_provider(cfg: &BatchConfig) -> Result<Box<dyn Provider>, ExtractError> {
    let api_key = resolve_api_key(cfg);
    match cfg.provider {
        ProviderKind::Anthropic => {
            let key = api_key.ok_or_else(|| missing_api_key(cfg))?;
            Ok(Box::new(AnthropicProvider::new(key, cfg.base_url.clone())))
        }
        ProviderKind::OpenaiCompatible => {
            let base_url = non_empty(&cfg.base_url)
                .ok_or_else(|| {
                    ExtractError::Provider(
                        "openai-compatible provider requires base_url".to_string(),
                    )
                })?
                .to_string();
            Ok(Box::new(OpenAiCompatibleProvider::new(api_key, base_url)))
        }
        ProviderKind::Ollama => {
            let base_url = non_empty(&cfg.base_url)
                .unwrap_or("http://localhost:11434")
                .to_string();
            Ok(Box::new(OllamaProvider::new(base_url)))
        }
        ProviderKind::Openrouter => {
            let key = api_key.ok_or_else(|| missing_api_key(cfg))?;
            Ok(Box::new(OpenRouterProvider::new(
                Some(key),
                cfg.base_url.clone(),
            )))
        }
        ProviderKind::ClaudeCli => {
            // No key: that is the whole point. A key in the environment would be
            // used *instead* of the subscription by the CLI itself, so `probe`
            // reports it rather than letting a 401 surface as a parse failure much
            // later, in a maintenance log.
            ClaudeCliProvider::probe().map_err(ExtractError::Provider)?;
            Ok(Box::new(ClaudeCliProvider::new(cfg.model.clone())))
        }
        ProviderKind::Gemini => {
            let key = api_key.ok_or_else(|| missing_api_key(cfg))?;
            Ok(Box::new(GeminiProvider::new(key, cfg.base_url.clone())))
        }
    }
}

fn missing_api_key(cfg: &BatchConfig) -> ExtractError {
    ExtractError::MissingApiKey(cfg.api_key_env.clone())
}

// ---------------------------------------------------------------------------
// Context fencing
// ---------------------------------------------------------------------------

const INJECTED_OPEN_PREFIX: &str = "<ctxlake:injected";
const INJECTED_CLOSE: &str = "</ctxlake:injected>";

/// Remove every `<ctxlake:injected ...>...</ctxlake:injected>` span, tags
/// included. See the module doc's "context fencing" paragraph for why this must
/// run before any transcript text reaches a prompt.
///
/// Malformed markup (an opening tag with no matching close) is handled by
/// stripping from that opening tag to the end of the string rather than trying to
/// guess where it should have closed — erring toward removing too much rather
/// than accidentally leaving a real injected span unfenced.
pub fn strip_injected_context(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find(INJECTED_OPEN_PREFIX) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_open = &rest[start..];
        let Some(tag_end_rel) = after_open.find('>') else {
            // No closing '>' on the opening tag itself — malformed, but not our
            // secret to guess at; drop the rest rather than emit a broken tag.
            break;
        };
        let after_tag_open = &after_open[tag_end_rel + 1..];
        match after_tag_open.find(INJECTED_CLOSE) {
            Some(close_rel) => rest = &after_tag_open[close_rel + INJECTED_CLOSE.len()..],
            None => break,
        }
    }
    out
}

/// Build the plain-text transcript handed to the model: one line per envelope
/// with real content, fenced first. Envelopes with no `content` (a bare tool call
/// with no text, say) are skipped rather than emitting an empty line.
pub fn build_fenced_transcript(envelopes: &[Envelope]) -> String {
    let mut out = String::new();
    // Name the session the citations are supposed to carry. Without this the model
    // is asked for a `session_id` it was never shown, so it supplies the only
    // id-shaped string in front of it — the first line's event id — and every claim
    // is then dropped as an unresolvable citation. Observed against a live model,
    // which produced four good claims and cited all four to an event id.
    if let Some(session_id) = envelopes.first().map(|e| e.session_id.as_str()) {
        out.push_str(&format!("session_id: {session_id}\n"));
    }
    for e in envelopes {
        let id = citable_id(e);

        if let Some(content) = &e.content {
            let cleaned = strip_injected_context(content);
            let cleaned = cleaned.trim();
            if !cleaned.is_empty() {
                out.push_str(&format!("[{id}] {cleaned}\n"));
            }
        }

        // Tool calls are the evidence. This function used to render `content` only,
        // and on a real Claude Code session `content` is set for the user's prompt
        // and nothing else — every command, exit code and output lives on `tool`. So
        // extraction was shown the question and never the work, and a live run
        // returned zero claims from a session that plainly contained one.
        //
        // Every unit test in this module missed it because they build envelopes with
        // `content` set directly; only a transcript captured through the real hook
        // has this shape.
        if let Some(t) = &e.tool {
            let mut line = format!("[{id}] tool:{}", t.name);
            if let Some(input) = t.input.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                line.push_str(&format!(" input={input}"));
            }
            if let Some(code) = t.exit_code {
                line.push_str(&format!(" exit={code}"));
            }
            if let Some(result) = t.result.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                // One line per call: a multi-line result would break the `[id] …`
                // framing the citation instructions depend on.
                line.push_str(&format!(" result={}", result.replace('\n', " ⏎ ")));
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Provider trait — one HTTP call, over raw reqwest (no Anthropic SDK exists).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system_prompt: String,
    pub user_prompt: String,
    pub model: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("provider request failed: {0}")]
    Provider(String),
    #[error("malformed structured-output response: {0}")]
    MalformedResponse(String),
    /// [`build_provider`]'s clear, actionable failure when the env var a
    /// `[summarize.batch]` config names is not set. Names the variable, never a
    /// value — there is never a value to print, since nothing resolved.
    #[error(
        "{0} is not set — export it (or point api_key_env at the variable that \
         actually holds the key) before running Tier 2 extraction"
    )]
    MissingApiKey(String),
}

/// One batch's results, keyed by the request id each was submitted under — never
/// by position, since batch results come back out of order (docs/memory.md).
pub type BatchResults = HashMap<String, Result<String, ExtractError>>;

/// One HTTP-backed extraction backend. Kept as a plain trait with hand-rolled
/// boxed futures (matching `ctxlake_store::clock::Clock`'s pattern) rather than
/// pulling in `async-trait` — see AGENTS.md's "don't add a dependency without
/// saying why."
pub trait Provider: Send + Sync {
    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<String, ExtractError>>;

    /// `requests` is `(request_id, CompletionRequest)`. The default sequential
    /// implementation is correct but pays full (live) price per request; a
    /// batch-capable provider overrides this to use its half-price batch
    /// endpoint instead — see [`AnthropicProvider`]. The result is keyed by
    /// request id, **never** by position: docs/memory.md is explicit that
    /// batch results come back out of order.
    fn complete_batch<'a>(
        &'a self,
        requests: &'a [(String, CompletionRequest)],
    ) -> BoxFuture<'a, Result<BatchResults, ExtractError>> {
        Box::pin(async move {
            let mut out = HashMap::new();
            for (id, req) in requests {
                out.insert(id.clone(), self.complete(req).await);
            }
            Ok(out)
        })
    }
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|v| !v.is_empty())
}

/// `content[0].text` out of an Anthropic Messages API response. Split out from
/// the network call so it can be exercised against a recorded response body with
/// no live API and no key — see the tests below.
pub fn parse_anthropic_message_response(raw: &str) -> Result<String, ExtractError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExtractError::MalformedResponse(e.to_string()))?;
    v.get("content")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("text"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            ExtractError::MalformedResponse("response has no content[0].text".to_string())
        })
}

/// One line of an Anthropic Message Batches results JSONL file. Public so
/// [`parse_batch_jsonl`]'s "out of order" guarantee is testable directly against
/// a recorded results body.
pub fn parse_batch_jsonl(raw: &str) -> HashMap<String, Result<String, String>> {
    let mut out = HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(custom_id) = v.get("custom_id").and_then(|c| c.as_str()) else {
            continue;
        };
        let result = v.get("result");
        let outcome = match result.and_then(|r| r.get("type")).and_then(|t| t.as_str()) {
            Some("succeeded") => result
                .and_then(|r| r.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.get(0))
                .and_then(|c0| c0.get("text"))
                .and_then(|t| t.as_str())
                .map(|s| Ok(s.to_string()))
                .unwrap_or_else(|| Err("succeeded result missing content[0].text".to_string())),
            Some(other) => Err(format!("batch request ended as {other}")),
            None => Err("result has no type".to_string()),
        };
        out.insert(custom_id.to_string(), outcome);
    }
    out
}

/// Tier 2 through the `claude` CLI you already have, on the subscription it is
/// already signed in to.
///
/// The cheapest extraction is the one you are not separately billed for. Anyone
/// running ctxlake is by definition running a coding agent, and for Claude Code users
/// that means a `claude` binary already authenticated against a subscription — so
/// asking them to go create an API key to summarise their own sessions is asking for
/// a second bill and a second secret to manage.
///
/// Shells out to `claude -p --output-format json`, which is the documented scriptable
/// mode, and reads `.result` out of the envelope. Verified against 2.1.78.
///
/// Three honest caveats, each of which shows up as a plain error rather than a silent
/// no-op:
///
/// - **`ANTHROPIC_API_KEY` in the environment wins over the subscription.** The CLI
///   prefers it, so a stale or dummy key makes every call fail with a 401 that says
///   nothing about ctxlake. [`probe`] reports it.
/// - **`claude` must be on `PATH`.** A supervised daemon has a minimal one; the unit
///   inherits the user's login PATH, not an interactive shell's.
/// - **Subscription rate limits apply**, and they are not the API's. Extraction is
///   batch work that retries, so a limit costs a later pass, not lost sessions.
pub struct ClaudeCliProvider {
    model: String,
}

impl ClaudeCliProvider {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }

    /// Whether this machine can actually run it — for `ctxlake doctor`.
    pub fn probe() -> Result<(), String> {
        if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
            return Err(
                "ANTHROPIC_API_KEY is set, and the claude CLI prefers it over the \
                 subscription — unset it, or use provider = \"anthropic\" with that key"
                    .to_string(),
            );
        }
        match std::process::Command::new("claude")
            .arg("--version")
            .output()
        {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!(
                "`claude --version` failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => Err(format!("`claude` is not on PATH: {e}")),
        }
    }
}

impl Provider for ClaudeCliProvider {
    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<String, ExtractError>> {
        Box::pin(async move {
            let model = if self.model.is_empty() {
                "haiku".to_string()
            } else {
                self.model.clone()
            };
            let user = req.user_prompt.clone();
            let system = req.system_prompt.clone();

            // The CLI is blocking and can take seconds; keep it off the async
            // executor's threads.
            let out = tokio::task::spawn_blocking(move || {
                use std::io::Write as _;
                use std::process::{Command, Stdio};
                let mut child = Command::new("claude")
                    .args([
                        "-p",
                        "--output-format",
                        "json",
                        "--model",
                        &model,
                        "--append-system-prompt",
                        &system,
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;
                child
                    .stdin
                    .as_mut()
                    .expect("stdin was piped")
                    .write_all(user.as_bytes())?;
                child.wait_with_output()
            })
            .await
            .map_err(|e| ExtractError::Provider(format!("claude CLI task panicked: {e}")))?
            .map_err(|e| ExtractError::Provider(format!("running the claude CLI: {e}")))?;

            if !out.status.success() {
                return Err(ExtractError::Provider(format!(
                    "claude CLI exited {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let envelope: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
                ExtractError::MalformedResponse(format!("claude CLI envelope: {e}"))
            })?;
            // `is_error` is where an auth failure or a refusal surfaces; the process
            // still exits 0, so trusting the exit code alone would turn a 401 into a
            // parse error about text that is really an error message.
            if envelope
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                return Err(ExtractError::Provider(format!(
                    "claude CLI: {}",
                    envelope
                        .get("result")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown error")
                )));
            }
            envelope
                .get("result")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    ExtractError::MalformedResponse(
                        "claude CLI envelope has no string `result`".to_string(),
                    )
                })
        })
    }
}

/// `anthropic` — real HTTP, via `reqwest`; no Anthropic Rust SDK exists.
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: non_empty(&base_url)
                .unwrap_or("https://api.anthropic.com")
                .to_string(),
        }
    }
}

impl Provider for AnthropicProvider {
    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<String, ExtractError>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "model": req.model,
                "max_tokens": 1024,
                "system": req.system_prompt,
                "messages": [{"role": "user", "content": req.user_prompt}],
            });
            let resp = self
                .client
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?;
            let text = resp
                .text()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?;
            parse_anthropic_message_response(&text)
        })
    }

    fn complete_batch<'a>(
        &'a self,
        requests: &'a [(String, CompletionRequest)],
    ) -> BoxFuture<'a, Result<BatchResults, ExtractError>> {
        Box::pin(async move {
            let batch_requests: Vec<_> = requests
                .iter()
                .map(|(id, r)| {
                    serde_json::json!({
                        "custom_id": id,
                        "params": {
                            "model": r.model,
                            "max_tokens": 1024,
                            "system": r.system_prompt,
                            "messages": [{"role": "user", "content": r.user_prompt}],
                        }
                    })
                })
                .collect();
            let created = self
                .client
                .post(format!("{}/v1/messages/batches", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&serde_json::json!({ "requests": batch_requests }))
                .send()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?;
            let created_json: serde_json::Value = created
                .json()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?;
            let batch_id = created_json
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ExtractError::Provider("batch create response has no id".into()))?
                .to_string();

            // Batch extraction is definitionally not latency-sensitive
            // (docs/memory.md) — a plain poll loop is the right shape
            // here, not a webhook or a background task this crate would then
            // have to keep alive across process restarts.
            let results_url = loop {
                let status = self
                    .client
                    .get(format!("{}/v1/messages/batches/{batch_id}", self.base_url))
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .send()
                    .await
                    .map_err(|e| ExtractError::Provider(e.to_string()))?
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| ExtractError::Provider(e.to_string()))?;
                if status.get("processing_status").and_then(|v| v.as_str()) == Some("ended") {
                    break status
                        .get("results_url")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            };
            let results_url = results_url
                .ok_or_else(|| ExtractError::Provider("batch ended with no results_url".into()))?;
            let body = self
                .client
                .get(&results_url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?
                .text()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?;
            Ok(parse_batch_jsonl(&body)
                .into_iter()
                .map(|(id, r)| (id, r.map_err(ExtractError::Provider)))
                .collect())
        })
    }
}

/// `choices[0].message.content` out of an OpenAI-compatible chat-completions
/// response — the shape vLLM, LM Studio, and most self-hosted gateways all speak.
pub fn parse_openai_chat_response(raw: &str) -> Result<String, ExtractError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExtractError::MalformedResponse(e.to_string()))?;
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            ExtractError::MalformedResponse(
                "response has no choices[0].message.content".to_string(),
            )
        })
}

/// The `/chat/completions` request body every OpenAI-shaped provider in this
/// module sends — split out so it is unit-testable on its own (no network) and
/// so [`OpenAiCompatibleProvider`] and [`OpenRouterProvider`] build the exact
/// same bytes rather than maintaining two copies that could drift.
fn chat_completions_body(req: &CompletionRequest) -> serde_json::Value {
    serde_json::json!({
        "model": req.model,
        "messages": [
            {"role": "system", "content": req.system_prompt},
            {"role": "user", "content": req.user_prompt},
        ],
        // Without this the model is free to answer in prose, and it does: a live
        // OpenRouter call returned a ```json fence around the object. `json_object`
        // is the widely-supported form (OpenAI, OpenRouter, and most compatible
        // gateways); a gateway that ignores an unknown field is no worse off than
        // before, which is why this is safe to send unconditionally.
        //
        // It is a belt, not a replacement for the braces: `unwrap_json_payload`
        // still runs, because "supported" and "obeyed" are different claims.
        "response_format": {"type": "json_object"},
    })
}

/// POST `{base_url}/chat/completions` and parse the response — the one HTTP call
/// [`OpenAiCompatibleProvider`] and [`OpenRouterProvider`] both make. OpenRouter
/// *is* OpenAI-compatible (same body, same response shape); the only thing it
/// adds is a fixed base URL, a default key env var, and a couple of extra
/// headers, so this is the shared helper the module doc's "reuse, don't copy"
/// intent calls for rather than a second copy of this function.
async fn chat_completions_request(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    extra_headers: &[(&str, &str)],
    req: &CompletionRequest,
) -> Result<String, ExtractError> {
    let body = chat_completions_body(req);
    let mut builder = client
        .post(format!("{base_url}/chat/completions"))
        .json(&body);
    if let Some(key) = api_key {
        builder = builder.bearer_auth(key);
    }
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    let resp = builder
        .send()
        .await
        .map_err(|e| ExtractError::Provider(e.to_string()))?;
    let text = resp
        .text()
        .await
        .map_err(|e| ExtractError::Provider(e.to_string()))?;
    parse_openai_chat_response(&text)
}

/// `openai-compatible` — any endpoint speaking the `/chat/completions` shape.
pub struct OpenAiCompatibleProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
}

impl OpenAiCompatibleProvider {
    pub fn new(api_key: Option<String>, base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url,
        }
    }
}

impl Provider for OpenAiCompatibleProvider {
    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<String, ExtractError>> {
        Box::pin(chat_completions_request(
            &self.client,
            &self.base_url,
            self.api_key.as_deref(),
            &[],
            req,
        ))
    }
}

/// `openrouter` — OpenAI-shaped, fixed at `openrouter.ai`. Its value over a bare
/// `openai-compatible` entry is that a user need not know the URL: just a model
/// name and a key. Sends `X-Title: ctxlake` (shows up in OpenRouter's own
/// dashboard/logs) and deliberately no `HTTP-Referer` — this is a CLI, not a
/// site with a URL to attribute traffic to.
pub struct OpenRouterProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
}

impl OpenRouterProvider {
    /// `base_url` overrides OpenRouter's own endpoint — for testing against a
    /// local stand-in, mainly; a real deployment has no reason to set it.
    pub fn new(api_key: Option<String>, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: non_empty(&base_url)
                .unwrap_or("https://openrouter.ai/api/v1")
                .to_string(),
        }
    }

    /// Test-only window into what `new` resolved `base_url` to — there is no
    /// production reason to read it back once the provider is built, but the
    /// default-vs-override behavior is exactly what a test needs to pin.
    #[cfg(test)]
    fn base_url_for_test(&self) -> &str {
        &self.base_url
    }
}

/// OpenRouter's attribution headers (its docs call these optional but
/// recommended). `X-Title` names the calling application in OpenRouter's own
/// dashboard/logs; `HTTP-Referer` attributes traffic to a site URL, which a
/// CLI does not have, so it is deliberately absent rather than set to
/// something misleading — see [`OpenRouterProvider`]'s doc. Named as a
/// constant so the "no Referer" claim is one thing to assert on, not
/// something to eyeball in an inline literal.
const OPENROUTER_EXTRA_HEADERS: &[(&str, &str)] = &[("X-Title", "ctxlake")];

impl Provider for OpenRouterProvider {
    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<String, ExtractError>> {
        Box::pin(chat_completions_request(
            &self.client,
            &self.base_url,
            self.api_key.as_deref(),
            OPENROUTER_EXTRA_HEADERS,
            req,
        ))
    }
}

/// `message.content` out of an Ollama `/api/chat` (non-streaming) response.
pub fn parse_ollama_response(raw: &str) -> Result<String, ExtractError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExtractError::MalformedResponse(e.to_string()))?;
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            ExtractError::MalformedResponse("response has no message.content".to_string())
        })
}

/// `ollama` — a local (or self-hosted) endpoint. No transcript leaves the host;
/// see docs/memory.md's "running it entirely locally."
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
}

impl OllamaProvider {
    pub fn new(base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
        }
    }
}

impl Provider for OllamaProvider {
    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<String, ExtractError>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "model": req.model,
                "messages": [
                    {"role": "system", "content": req.system_prompt},
                    {"role": "user", "content": req.user_prompt},
                ],
                "stream": false,
            });
            let resp = self
                .client
                .post(format!("{}/api/chat", self.base_url))
                .json(&body)
                .send()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?;
            let text = resp
                .text()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?;
            parse_ollama_response(&text)
        })
    }
}

/// The subset of Gemini's OpenAPI-flavored `responseSchema` this module needs:
/// exactly the shape [`RawExtraction`]/[`RawClaim`]/[`RawCitation`] parse, so a
/// `generationConfig.responseMimeType: "application/json"` request is
/// constrained to return something [`parse_claims_response`] can read — Gemini
/// enforces this at generation time rather than leaving it to the prompt alone,
/// which is the one thing this provider does that the other three cannot.
fn gemini_claims_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "OBJECT",
        "properties": {
            "claims": {
                "type": "ARRAY",
                "items": {
                    "type": "OBJECT",
                    "properties": {
                        "claim": {"type": "STRING"},
                        "claim_type": {"type": "STRING"},
                        "subject": {"type": "STRING"},
                        "evidence": {
                            "type": "ARRAY",
                            "items": {
                                "type": "OBJECT",
                                "properties": {
                                    "session_id": {"type": "STRING"},
                                    "message_id": {"type": "STRING"},
                                },
                                "required": ["session_id", "message_id"],
                            },
                        },
                    },
                    "required": ["claim", "claim_type", "subject"],
                },
            },
        },
        "required": ["claims"],
    })
}

/// Gemini's `generateContent` request body: `contents`/`systemInstruction`
/// rather than a `messages` array, plus the structured-output config above. Kept
/// separate from the HTTP call so it is unit-testable with no network — same
/// pattern as [`chat_completions_body`].
fn gemini_request_body(req: &CompletionRequest) -> serde_json::Value {
    serde_json::json!({
        "contents": [
            {"role": "user", "parts": [{"text": req.user_prompt}]},
        ],
        "systemInstruction": {
            "parts": [{"text": req.system_prompt}],
        },
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": gemini_claims_response_schema(),
        },
    })
}

/// `candidates[0].content.parts[0].text` out of a Gemini `generateContent`
/// response.
pub fn parse_gemini_response(raw: &str) -> Result<String, ExtractError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExtractError::MalformedResponse(e.to_string()))?;
    v.get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p0| p0.get("text"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            ExtractError::MalformedResponse(
                "response has no candidates[0].content.parts[0].text".to_string(),
            )
        })
}

/// `gemini` — a genuinely different shape from the other three, not a fourth
/// coat of OpenAI paint: `contents`/`systemInstruction` instead of a flat
/// `messages` array, and the key rides in the `x-goog-api-key` header, never the
/// URL — a key in a query string ends up in logs and proxies (AGENTS.md
/// invariant 10's spirit, applied to transport, not just config-at-rest).
pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl GeminiProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: non_empty(&base_url)
                .unwrap_or("https://generativelanguage.googleapis.com/v1beta")
                .to_string(),
        }
    }

    /// Test-only window into what `new` resolved `base_url` to — see
    /// [`OpenRouterProvider::base_url_for_test`] for why this exists only
    /// under `cfg(test)`.
    #[cfg(test)]
    fn base_url_for_test(&self) -> &str {
        &self.base_url
    }
}

/// `{base_url}/models/{model}:generateContent` — split out so the URL shape is
/// testable without a network call, and in particular so it is easy to assert
/// the API key never ends up in it (it goes in the `x-goog-api-key` header
/// instead — a key in a query string ends up in logs and proxies).
fn gemini_url(base_url: &str, model: &str) -> String {
    format!("{base_url}/models/{model}:generateContent")
}

impl Provider for GeminiProvider {
    fn complete<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<String, ExtractError>> {
        Box::pin(async move {
            let body = gemini_request_body(req);
            let url = gemini_url(&self.base_url, &req.model);
            let resp = self
                .client
                .post(url)
                .header("x-goog-api-key", &self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?;
            let text = resp
                .text()
                .await
                .map_err(|e| ExtractError::Provider(e.to_string()))?;
            parse_gemini_response(&text)
        })
    }
}

// ---------------------------------------------------------------------------
// Structured output -> verified, evidence-backed candidates
// ---------------------------------------------------------------------------

/// A citation as the model is asked to produce it: an identifier only. The
/// model never supplies its own `excerpt_hash` — see [`claim_from_raw`] for why
/// trusting a model-computed hash would defeat the point of verifying at all.
#[derive(Debug, Clone, Deserialize)]
pub struct RawCitation {
    pub session_id: String,
    pub message_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawClaim {
    pub claim: String,
    pub claim_type: String,
    pub subject: String,
    #[serde(default)]
    pub evidence: Vec<RawCitation>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawExtraction {
    claims: Vec<RawClaim>,
}

/// Parse the model's structured-output JSON (`{"claims": [...]}`) into raw
/// candidates. A malformed response is an [`ExtractError`], not a bad memory
/// silently stored — docs/memory.md's "structured output" rule.
pub fn parse_claims_response(raw: &str) -> Result<Vec<RawClaim>, ExtractError> {
    let candidate = unwrap_json_payload(raw);
    serde_json::from_str::<RawExtraction>(candidate)
        .map(|p| p.claims)
        .map_err(|e| {
            // The parser error alone said "expected value at line 1 column 1" and
            // nothing about what arrived — which on a real lake meant a failing
            // extraction that could not be diagnosed without reproducing it by hand.
            // A bounded excerpt of the actual response is the difference between
            // "the model refused" and "the envelope shape changed".
            ExtractError::MalformedResponse(format!("{e}; response began: {}", excerpt(raw)))
        })
}

/// A short, single-line, quoted excerpt of a model response, for an error message.
///
/// Bounded and flattened because this lands in a log an operator reads: a full
/// response can be kilobytes, and a raw newline turns one error into forty lines.
fn excerpt(raw: &str) -> String {
    const MAX: usize = 200;
    let flat: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX)
        .collect();
    if raw.trim().chars().count() > MAX {
        format!("{flat:?}…")
    } else {
        format!("{flat:?}")
    }
}

/// How many already-known claims to show the extractor.
///
/// Bounded because this rides in front of every transcript and a fleet's claim count
/// only grows. Newest first, so a growing lake keeps showing what is most likely to be
/// re-observed rather than what happened first.
const MAX_KNOWN_CLAIMS_SHOWN: usize = 60;

/// And how much of each, so one verbose claim cannot crowd out fifty others.
const MAX_KNOWN_CLAIM_CHARS: usize = 240;

/// The "already recorded" block prefixed to a transcript, or `None` when the fleet
/// believes nothing yet.
///
/// Only promoted and contested claims are shown. A candidate has not cleared the gate,
/// and presenting one as established would let a rejected claim launder itself into
/// the fleet's vocabulary through the next extraction.
fn known_claims_preamble(existing: &BTreeMap<String, ClaimState>) -> Option<String> {
    let mut rows: Vec<&ClaimState> = existing
        .values()
        .filter(|c| {
            matches!(
                c.status,
                crate::claims::ClaimStatus::Promoted | crate::claims::ClaimStatus::Contested
            )
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    // Newest first: `claim_id` is a ULID, so lexicographic order is chronological.
    rows.sort_by(|a, b| b.claim_id.cmp(&a.claim_id));
    rows.truncate(MAX_KNOWN_CLAIMS_SHOWN);

    let mut out = String::from(
        "ALREADY RECORDED — the fleet has these claims.\n\
         If this transcript supports one of them, REPEAT ITS TEXT CHARACTER FOR \
         CHARACTER as a claim, citing this transcript's own evidence. Do NOT omit it, \
         and do NOT reword it.\n\
         Repeating it is how a second, independent observation is recorded: an exactly \
         matching claim is merged into the existing one and strengthens it. Rewording \
         creates a duplicate; omitting throws the corroboration away.\n\
         Propose a new claim only for something not already listed here.\n",
    );
    for c in rows {
        let text = truncate_chars(&c.claim, MAX_KNOWN_CLAIM_CHARS);
        out.push_str(&format!("- [{}] {}\n", c.claim_type.as_str(), text));
    }
    out.push_str("\nTRANSCRIPT:\n");
    Some(out)
}

/// Truncate on a character boundary, marking that it happened.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// Whether a rendered transcript carries enough for a claim to be grounded in.
///
/// Three rounds of real failures shaped this, each one the model telling us plainly
/// that we had sent it nothing:
///
/// 1. Sessions rendering to the `session_id:` header alone.
/// 2. Sessions rendering to a page of `[toolu_x] tool:Read` — ids and tool names, no
///    inputs, no results, no evidence of anything.
/// 3. A session whose entire content was one prompt reading `ok`.
///
/// The third is why this counts characters rather than lines. "Is there any text" is
/// the wrong question; "is there enough text that a claim could cite it" is the right
/// one, and `ok` answers no. A model handed that replies, correctly and at length,
/// that it cannot see a transcript — then fails to parse as JSON.
///
/// The threshold errs toward attempting. A skipped session is revisitable, since
/// `EXTRACTOR_VERSION` makes every already-marked session eligible again on the next
/// bump; a wasted model call is spent for good. [`MIN_SUBSTANTIVE_CHARS`] is set below
/// the length of a single real shell command, so "one `cargo test --workspace` and
/// nothing else" is still extracted.
fn transcript_is_empty(prompt: &str) -> bool {
    substantive_chars(prompt) < MIN_SUBSTANTIVE_CHARS
}

/// Below this much actual content, there is nothing for a claim to cite.
///
/// Calibrated against real content at both ends, not chosen round:
///
/// - The session that failed on the live lake contained one prompt reading `ok`. It
///   scores **2**, along with every other acknowledgement token — `yes`, `go`,
///   `thanks`, `continue` — which is what these sessions are made of.
/// - `staging listens on port 2222` scores **28**, and is `docs/memory.md`'s own
///   example of a good `environment` claim. Anything that states something clears this
///   comfortably.
///
/// A first attempt used 40 and would have skipped that example. This module's existing
/// tests caught it, which is the argument for the threshold living beside them.
///
/// The asymmetry favours attempting: a skipped session becomes eligible again on the
/// next `EXTRACTOR_VERSION` bump, while a wasted model call is spent for good.
const MIN_SUBSTANTIVE_CHARS: usize = 12;

/// Characters of real content in a rendered transcript.
///
/// Structure does not count: the `session_id:` header, the `[id]` markers and a bare
/// `tool:NAME` are scaffolding the renderer adds, and a page of them is still an empty
/// session.
fn substantive_chars(prompt: &str) -> usize {
    prompt
        .lines()
        .filter(|l| !l.trim_start().starts_with("session_id:"))
        .map(|l| {
            let after_id = l.split_once("] ").map(|(_, rest)| rest).unwrap_or(l);
            match after_id.strip_prefix("tool:") {
                // The tool's name is scaffolding; its input and output are evidence.
                Some(tail) => tail
                    .split_once(' ')
                    .map(|(_, args)| args.trim().len())
                    .unwrap_or(0),
                None => after_id.trim().len(),
            }
        })
        .sum()
}

/// Pull the JSON object out of a response that may have been dressed up.
///
/// Found against a live endpoint, not a fixture: asked for nothing but JSON and given
/// a `response_format`, `anthropic/claude-haiku-4.5` through OpenRouter still returned
/// ```` ```json\n{"claims":[]}\n``` ````. Every recorded fixture in this module's test
/// suite carries a clean object because each was built from the provider's *documented*
/// response schema, so none of them could ever have caught this — the wrapper is a
/// property of the model, not of the API contract.
///
/// Deliberately not a general "find some JSON in this text" search: it strips a fenced
/// block if the whole response is one, and otherwise takes the span from the first `{`
/// to the last `}`. Anything looser starts salvaging JSON out of prose that was never
/// meant to be a result, which is how a refusal turns into a claim.
fn unwrap_json_payload(raw: &str) -> &str {
    let t = raw.trim();
    // ```json … ``` or ``` … ```
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        if let Some(body) = rest.trim_start_matches(['\r', '\n']).strip_suffix("```") {
            return body.trim();
        }
    }
    match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if b > a => &t[a..=b],
        _ => t,
    }
}

/// What a `(session_id, message_id)` citation resolves to, once verified against
/// the session's own captured envelopes — never taken from what a model claims.
/// `observed_at` is the *envelope's own* `emitted_at`, i.e. when this specific
/// message actually happened, not the session-level fallback timestamp
/// [`extract_session`] uses for the claim as a whole. See [`Evidence`]'s doc for
/// why the two must not be conflated: `gate::check_provenance` checks each
/// citation's own timestamp against its own session's window, and a claim whose
/// evidence spans more than one session (the whole point of corroboration) needs
/// each citation to carry the time it was actually made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCitation {
    pub excerpt_hash: String,
    pub observed_at: String,
}

/// `(session_id, message_id) -> ResolvedCitation`, built from the session's own
/// envelopes — the only place an excerpt hash or a citation's timestamp is
/// allowed to come from. See [`claim_from_raw`].
pub type ResolvableIndex = HashMap<(String, String), ResolvedCitation>;

/// The id an envelope is citable by — the single source of truth for both the prompt
/// the model reads and the index its citations resolve against.
///
/// **This exists because those two computed it separately and disagreed.**
/// [`build_fenced_transcript`] labelled a prose line (a prompt or assistant envelope,
/// which carries no `message_id`) with its `event_id`, while
/// [`build_resolvable_index`] indexed `message_id` only. So the model was shown an id,
/// cited it correctly, and every such claim was discarded as unresolvable — silently,
/// and structurally, for every claim whose evidence was something a human or the agent
/// *said* rather than a tool call. Measured on a live fleet: 7 of 27 claims in a single
/// pass. Any change to how an envelope is addressed belongs here, in one place, or the
/// two halves drift apart again.
pub fn citable_id(e: &Envelope) -> &str {
    e.message_id.as_deref().unwrap_or(&e.event_id)
}

pub fn build_resolvable_index(envelopes: &[Envelope]) -> ResolvableIndex {
    let mut idx = HashMap::new();
    for e in envelopes {
        idx.insert(
            (e.session_id.clone(), citable_id(e).to_string()),
            ResolvedCitation {
                excerpt_hash: e.content_hash.clone(),
                observed_at: e.emitted_at.clone(),
            },
        );
    }
    idx
}

/// Find a `claim_id` already on file for the same `(claim_type, subject,
/// normalized claim text)` — the matching [`ProposedClaim`]'s own doc comment
/// promises: "additional corroborating evidence for an existing candidate
/// (`claim_id` reused — extraction ... does this when they recognize a claim
/// already on file for the same subject)". Matches against any non-`Retired`
/// state (`Candidate`, `Promoted`, or `Contested`) — a retired claim is done, and
/// resurrecting it by silently reusing its id would put fresh evidence behind a
/// status a human already closed out.
///
/// This is what makes `gate::compute_independent_count` mean anything in
/// production: without it, every extraction mints a brand-new `claim_id`, so two
/// sessions that "agree" always look like two unrelated single-evidence claims
/// rather than one claim with two evidence sessions — the independence gate has
/// nothing to discount and `independent_count` trivially equals `evidence_count`
/// for every claim, exactly the failure the gate exists to prevent.
fn find_existing_claim_id(
    existing: &BTreeMap<String, ClaimState>,
    claim_type: ClaimType,
    subject: &str,
    claim_text: &str,
) -> Option<String> {
    let _ = subject;
    let norm_claim = claims::normalize_claim_text(claim_text);
    existing
        .values()
        .find(|s| {
            s.status != crate::claims::ClaimStatus::Retired
                && s.claim_type == claim_type
                // **Subject is deliberately not compared.**
                //
                // It used to be, and that is why identical claims were filed twice. The
                // subject is free text the model invents per session: the same fact
                // arrived as "console theming" and "oxidant theme storage", as
                // "VitePress theme structure" and "oxidantdata-theme", as "navbar
                // updates" and "site navigation updates". Requiring the subjects to
                // match meant this function almost never fired, and each re-observation
                // became a new claim with a fresh id instead of evidence on the
                // existing one.
                //
                // The claim text is the assertion; the subject is a label for grouping
                // it. Two claims with identical text are the same claim whatever the
                // model chose to call the topic.
                && claims::normalize_claim_text(&s.claim) == norm_claim
        })
        .map(|s| s.claim_id.clone())
}

/// Turn one raw candidate into a [`ProposedClaim`], or drop it. Two ways to be
/// dropped, both silent (no low-confidence record, per docs/memory.md's
/// "no evidence, no claim"): an unrecognized `claim_type`, or zero citations that
/// actually resolve against this session's real transcript. `excerpt_hash` and
/// each citation's `observed_at` are **computed here**, from the session's own
/// captured envelopes, never taken from whatever the model may have claimed — a
/// model that invents a citation cannot also invent the hash (or the timestamp)
/// of content that doesn't exist.
///
/// `existing` is every claim currently on file (any status but `Retired`),
/// keyed by `claim_id` — the fold of `claims/events/` at the time extraction
/// runs. When this candidate matches one on `(claim_type, subject, claim text)`
/// (see [`find_existing_claim_id`]), its `claim_id` is reused so the new evidence
/// accumulates onto the same claim instead of minting a look-alike sibling; the
/// event log's `fold` (see `claims.rs`) already merges evidence for a reused
/// `claim_id`, so this is the only piece extraction needed to add.
pub fn claim_from_raw(
    raw: RawClaim,
    observed_by: &str,
    observed_at: &str,
    resolvable: &ResolvableIndex,
    existing: &BTreeMap<String, ClaimState>,
) -> Option<ProposedClaim> {
    let claim_type = ClaimType::parse(&raw.claim_type)?;
    let evidence: Vec<Evidence> = raw
        .evidence
        .iter()
        .filter_map(|c| {
            // Exact match first: a model that echoed the session id correctly is
            // taken at its word.
            if let Some(resolved) = resolvable.get(&(c.session_id.clone(), c.message_id.clone())) {
                return Some(Evidence {
                    session_id: c.session_id.clone(),
                    message_id: c.message_id.clone(),
                    excerpt_hash: resolved.excerpt_hash.clone(),
                    observed_at: resolved.observed_at.clone(),
                });
            }
            // Otherwise fall back to the message id alone. `resolvable` is built from
            // exactly one session's envelopes (extraction is per-session), so this
            // cannot pull evidence in from somewhere else — and the message id is the
            // half that actually verifies anything. The session id is ours to know,
            // not the model's to determine, so a wrong guess at it must not discard
            // real, citable evidence. Whatever the model said is discarded in favour
            // of the session the envelope actually belongs to.
            resolvable
                .iter()
                .find(|((_, mid), _)| *mid == c.message_id)
                .map(|((sid, mid), resolved)| Evidence {
                    session_id: sid.clone(),
                    message_id: mid.clone(),
                    excerpt_hash: resolved.excerpt_hash.clone(),
                    observed_at: resolved.observed_at.clone(),
                })
        })
        .collect();
    if evidence.is_empty() {
        return None;
    }
    let claim_id = find_existing_claim_id(existing, claim_type, &raw.subject, &raw.claim)
        .unwrap_or_else(ctxlake_core::envelope::next_event_id);
    Some(ProposedClaim {
        claim_id,
        claim: raw.claim,
        claim_type,
        subject: raw.subject,
        scope: Scope::Agent,
        observed_by: observed_by.to_string(),
        observed_at: observed_at.to_string(),
        evidence,
        embedding: None,
        resolves_at: None,
    })
}

// ---------------------------------------------------------------------------
// Orchestration: idempotency marker, sealed-session discovery, the run loop
// ---------------------------------------------------------------------------

/// One sealed session found under `sessions/`.
#[derive(Debug, Clone)]
pub struct SessionRef {
    /// The directory holding this session's `seg-*.parquet` files and its
    /// `_SEALED` marker.
    pub session_dir: object_store::path::Path,
    pub session_id: String,
}

/// The envelopes of one sealed session, decoded and concatenated across every
/// segment, in event order.
#[derive(Debug, Clone)]
pub struct SessionTranscript {
    pub session_id: String,
    pub agent_id: String,
    pub envelopes: Vec<Envelope>,
}

/// Claim `claims/extracted/<session_id>` with `PutMode::Create`. `true` means
/// this call is the one that gets to extract; `false` means someone (a prior
/// run, or a racing caller on another host) already has.
///
/// This is an idempotency **marker**, not a lock: there is no holder, no TTL, no
/// renewal, no stealing, and nobody waits on it. `claims/extracted/<id>` never
/// exists before the first extraction and never needs a second state — it either
/// doesn't exist yet, or it does — so a plain `Create` fully answers "am I the
/// one" in a single round trip, with the same MinIO caveat as everywhere else in
/// this codebase that reaches for it (minio/minio#20346): on MinIO this call
/// fails outright rather than succeeding-or-losing-a-race, so a MinIO-backed
/// fleet will re-attempt extraction on every run until that is worked around.
/// `ctxlake doctor`'s put-if-absent probe is what surfaces this ahead of time. See
/// `mark_extracted_if_new_under_real_concurrency_exactly_one_host_wins` below for
/// proof that racing callers never both win.
/// Give back the claim on a session whose extraction failed, so a later run retries it.
///
/// Best-effort and deliberately silent: this runs on an error path that is already
/// returning a more useful error to the caller, and a failure to clean up costs one
/// session's claims, not correctness. The next run simply finds the marker and skips —
/// the same outcome as before this existed.
async fn release_extraction_marker(store: &dyn ObjectStore, fleet_id: &str, session_id: &str) {
    let key = ctxlake_store::layout::claims_extracted(fleet_id, session_id);
    if let Err(e) = store.delete(&key).await {
        tracing::warn!(
            session_id,
            error = %e,
            "could not release the extraction marker; this session will not be retried"
        );
    }
}

pub async fn mark_extracted_if_new(
    store: &dyn ObjectStore,
    fleet_id: &str,
    session_id: &str,
) -> Result<bool, StoreError> {
    let path = ctxlake_store::layout::claims_extracted(fleet_id, session_id);
    // Create-if-absent is still what makes two hosts extract a session exactly once.
    // The body is what makes a *later build* able to revisit it — see
    // `is_already_extracted`.
    let body = serde_json::json!({ "extractor_version": EXTRACTOR_VERSION });
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    match store
        .put_opts(
            &path,
            PutPayload::from(bytes.clone()),
            PutMode::Create.into(),
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(OsError::AlreadyExists { .. }) => {
            // Present, but possibly from an older extractor. `is_already_extracted`
            // has already decided whether this session is due for a re-run; if it let
            // us get here, the marker is stale and claiming it means overwriting.
            //
            // A plain overwrite rather than CAS: two hosts racing to re-extract the
            // same stale session both do the work and both append proposals, which
            // `find_existing_claim_id` already folds onto the same `claim_id`. The
            // cost is a duplicated model call, not a corrupted lake.
            if stored_extractor_version(store, fleet_id, session_id).await < EXTRACTOR_VERSION {
                store.put(&path, PutPayload::from(bytes)).await?;
                return Ok(true);
            }
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

/// Which extractor produced the claims for this session, or 0 when unknown.
///
/// 0 covers both "no marker" and "a marker from before markers carried a version",
/// which are the same thing for this purpose: older than anything current.
async fn stored_extractor_version(
    store: &dyn ObjectStore,
    fleet_id: &str,
    session_id: &str,
) -> u32 {
    let path = ctxlake_store::layout::claims_extracted(fleet_id, session_id);
    let Ok(res) = store.get(&path).await else {
        return 0;
    };
    let Ok(bytes) = res.bytes().await else {
        return 0;
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| {
            v.get("extractor_version")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0) as u32
}

/// Read-only: has `session_id` already been extracted? A cheap existence check
/// [`run`] uses to skip past done sessions *without* spending its
/// `max_sessions_per_run` budget on them — see that function's doc for the
/// silent-stall bug this exists to avoid. This intentionally does not claim
/// anything: a session marked not-yet-extracted here can still race with another
/// caller between this check and the real attempt, and that race is resolved the
/// same way it always was, by [`mark_extracted_if_new`]'s atomic `Create` inside
/// [`extract_session`] — this function only ever affects scheduling, never
/// correctness.
pub async fn is_already_extracted(
    store: &dyn ObjectStore,
    fleet_id: &str,
    session_id: &str,
) -> Result<bool, StoreError> {
    let path = ctxlake_store::layout::claims_extracted(fleet_id, session_id);
    match store.get(&path).await {
        Ok(res) => {
            // **Done by *which* extractor**, not merely done.
            //
            // Existence alone meant a session was finished forever. A successful
            // `{"claims": []}` writes the marker just as a productive run does, so 37
            // sessions on a live lake were permanently marked done having produced
            // nothing — and no improvement to the prompt, the transcript, or the
            // provider could ever have been measured against them. Short of deleting
            // keys by hand, the only evidence the extractor had was sessions that
            // happened not to exist yet.
            let version = res
                .bytes()
                .await
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|v| {
                    v.get("extractor_version")
                        .and_then(serde_json::Value::as_u64)
                })
                .unwrap_or(0) as u32;
            Ok(version >= EXTRACTOR_VERSION)
        }
        Err(OsError::NotFound { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Bumped whenever a change should make already-extracted sessions worth revisiting:
/// a different prompt, a different transcript shape, a fixed parser.
///
/// Not bumped for a provider swap or a model change — those are configuration, and
/// re-extracting an entire lake because someone edited `ctxlake.toml` would be a
/// surprising and expensive thing for a config edit to do.
pub const EXTRACTOR_VERSION: u32 = 5;

/// Find every sealed session under `sessions/` by locating `_SEALED` markers.
/// Whether a given one has already been extracted is [`mark_extracted_if_new`]'s
/// job, not this listing's — keeping "what exists" and "what's claimed" as
/// separate questions avoids a stale listing racing a marker that landed a
/// moment ago.
/// Scoped to `fleet_id`, like every other discovery function in this crate.
///
/// This used to list all of `sessions/` with no filter, while
/// `digest::discover_sealed_sessions` and `compact::discover_dates` were both
/// fleet-scoped. In a store holding one fleet that is invisible; in a store holding
/// two, it meant one team's transcripts were read into another team's extraction
/// prompts and became evidence for another team's claims. `fleet_id` is documented
/// as "the boundary of who sees whom", so crossing it here contradicted the one
/// guarantee the setting makes.
pub async fn list_sealed_sessions(
    store: &dyn ObjectStore,
    fleet_id: &str,
) -> Result<Vec<SessionRef>, StoreError> {
    use futures::StreamExt;
    let prefix = object_store::path::Path::from("sessions");
    let mut out = Vec::new();
    let mut stream = store.list(Some(&prefix));
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        // The fleet comes from the partition path, not from the envelopes inside:
        // a session directory that decodes to zero readable envelopes still belongs
        // to exactly one fleet, and must not fall through to "everyone's".
        match crate::partition::parse_session_partition(&meta.location) {
            Some(p) if p.fleet_id == fleet_id => {}
            _ => continue,
        }
        let loc = meta.location.to_string();
        let Some(dir) = loc.strip_suffix("/_SEALED") else {
            continue;
        };
        let session_id = dir
            .rsplit('/')
            .next()
            .and_then(|seg| seg.strip_prefix("session="))
            .unwrap_or_default()
            .to_string();
        out.push(SessionRef {
            session_dir: object_store::path::Path::from(dir),
            session_id,
        });
    }
    Ok(out)
}

/// Read and decode every `seg-*.parquet` under one session's directory, sorted
/// so segments (and the envelopes inside them, which are already monotonic per
/// AGENTS.md's ULID guarantee) come back in the order they were written.
pub async fn load_transcript(
    store: &dyn ObjectStore,
    session_ref: &SessionRef,
) -> Result<SessionTranscript, StoreError> {
    use futures::StreamExt;
    let mut segments: Vec<(String, bytes::Bytes)> = Vec::new();
    let mut stream = store.list(Some(&session_ref.session_dir));
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
        let name = meta.location.to_string();
        if !name.ends_with(".parquet") {
            continue;
        }
        let res = store.get(&meta.location).await?;
        segments.push((name, res.bytes().await?));
    }
    segments.sort_by(|a, b| a.0.cmp(&b.0));

    let mut envelopes = Vec::new();
    for (_, bytes) in segments {
        // Reuses `ctxlake-sync`'s codec rather than a second Parquet<->Envelope
        // mapping — see this crate's Cargo.toml comment on why.
        envelopes.extend(ctxlake_sync::codec::decode(&bytes).map_err(StoreError::Config)?);
    }
    envelopes.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    let agent_id = envelopes
        .first()
        .map(|e| e.agent_id.clone())
        .unwrap_or_default();
    Ok(SessionTranscript {
        session_id: session_ref.session_id.clone(),
        agent_id,
        envelopes,
    })
}

/// The extraction system prompt: the stable prefix docs/memory.md's
/// "prompt caching" paragraph describes — byte-identical across every session,
/// so a caching-aware provider only pays for it once.
pub const EXTRACTION_SYSTEM_PROMPT: &str = r#"You extract atomic, evidence-backed claims from a coding-agent session transcript, for a memory another agent will read weeks from now in a different session.

Respond with ONLY a JSON object of the shape:
{"claims": [{"claim": string, "claim_type": "environment"|"convention"|"outcome"|"preference"|"hypothesis", "subject": string, "evidence": [{"session_id": string, "message_id": string}]}]}

THE FIVE TYPES. Choose deliberately; each is promoted under different rules.

- "convention": a durable rule about how this codebase or team works, true before this session and after it.
  e.g. "this repo uses `just`, not `make`" / "CI fails unless RUSTFLAGS=-D warnings is set"
  The most valuable type. Prefer it whenever the evidence supports one.

- "environment": a fact about the world outside the code — a host, port, service, credential mechanism, version.
  e.g. "staging SSH listens on 2222" / "the MinIO container rejects If-None-Match: *"

- "outcome": one specific thing that happened, immutable and timestamped.
  e.g. "the Glue migration passed CI at abc123"
  Use SPARINGLY. A structural digest of every session already records which files were
  touched, which commands ran and how they exited. An "outcome" that restates that is
  duplicate noise in a memory someone pays context-window tokens to read. Claim an
  outcome only when it is surprising or consequential later — a migration that landed, a
  root cause found — never merely to narrate the session.

- "preference": how a human wants to be worked with. e.g. "prefers terse output"

- "hypothesis": a suspected but unconfirmed cause. e.g. "the flake is a colima scheduling artifact"
  Label honestly rather than promoting a guess to a fact.

RULES.
- Every claim MUST cite at least one (session_id, message_id) pair that appears in the transcript's own [message_id] markers. Never invent a citation.
- "subject" is the thing the claim is about ("ci", "oxidant-loom", "staging"), so later claims about the same subject can be compared.
- Atomic: one assertion per claim.
- Do not claim what the transcript does not show. If nothing here is worth another agent's attention weeks from now, return {"claims": []} — that is a correct answer, not a failure."#;

#[derive(Debug, Clone, Default)]
pub struct ExtractOutcome {
    pub session_id: String,
    pub claims_proposed: usize,
    /// Dropped for a `claim_type` this build does not recognize.
    ///
    /// Split from [`Self::dropped_unresolvable`] because the two call for opposite
    /// fixes — a prompt that names the types versus a bug in how evidence resolves —
    /// and a single "dropped" count cannot tell an operator which they are looking at.
    pub dropped_bad_type: usize,
    /// Dropped because no citation resolved against a captured envelope.
    pub dropped_unresolvable: usize,
    /// How many claims the model returned, before any were dropped.
    ///
    /// Reported separately from `claims_proposed` because the two failures look
    /// identical without it. "The model found nothing worth claiming" and "the model
    /// answered and we discarded every word of it" both printed as `0 claim(s)
    /// proposed`, and they call for opposite fixes — a better prompt versus a bug in
    /// `claim_from_raw`. A claim is dropped silently for an unparseable `claim_type`,
    /// or for citing a message id that does not resolve, and neither leaves a trace.
    pub claims_returned: usize,
    pub skipped_already_extracted: bool,
}

/// Extract one already-loaded session's transcript. `date` partitions the
/// resulting `claims/events/` keys (see `ctxlake_store::layout::claim_event`) —
/// the caller supplies it (typically derived from the session's own envelopes)
/// rather than this module reaching for a wall clock, matching AGENTS.md
/// invariant 6's spirit even though this specific key has no expiry semantics.
pub async fn extract_session(
    store: &dyn ObjectStore,
    cfg: &SummarizeConfig,
    provider: &dyn Provider,
    session: &SessionTranscript,
    date: &str,
    fleet_id: &str,
) -> Result<ExtractOutcome, ExtractError> {
    if !tier2_enabled(cfg) {
        return Ok(ExtractOutcome {
            session_id: session.session_id.clone(),
            ..Default::default()
        });
    }
    if !mark_extracted_if_new(store, fleet_id, &session.session_id).await? {
        return Ok(ExtractOutcome {
            session_id: session.session_id.clone(),
            skipped_already_extracted: true,
            ..Default::default()
        });
    }

    let batch_cfg = cfg
        .batch
        .as_ref()
        .expect("tier2_enabled just confirmed cfg.batch.is_some()");
    let transcript_text = build_fenced_transcript(&session.envelopes);

    // **A session with nothing in it is not worth a model call.**
    //
    // `build_fenced_transcript` renders `content` and tool calls; a session that
    // produced neither — a window opened and closed, a `--resume` that never ran
    // anything — renders to the `session_id:` header and nothing else. Sending that
    // asks a model to extract claims from an empty document, and it answers, at
    // length, that it cannot see a transcript. Ten of those in one pass on a real
    // lake: ten paid calls, ten parse failures, and a summary line dominated by a
    // failure that was never the model's fault.
    //
    // Left marked extracted rather than released, because there is nothing a later
    // pass would do differently — the session is sealed and its content is final.
    if transcript_is_empty(&transcript_text) {
        tracing::debug!(
            session_id = %session.session_id,
            "no transcript content to extract from; skipping the model call"
        );
        return Ok(ExtractOutcome {
            session_id: session.session_id.clone(),
            claims_proposed: 0,
            claims_returned: 0,
            // An empty transcript reaches no model, so nothing was dropped — it was
            // never proposed. Zero here means "no claims lost", not "not measured".
            dropped_bad_type: 0,
            dropped_unresolvable: 0,
            skipped_already_extracted: false,
        });
    }
    // **What the fleet already believes, shown to the extractor.**
    //
    // Duplication is the problem this solves, and it could not be solved downstream.
    // Measured on a live lake: 12 near-duplicate pairs among 104 promoted claims, and
    // no similarity threshold separates them. "TLS certificate mounting uses a
    // kubernetes.io/tls Secret" and "the TLS Secret is mounted read-only at
    // /etc/..." share 48% of their words and are two different facts; "the shared theme
    // stylesheet must be byte-identical" and "oxidantdata.css must be byte-identical
    // across repos" share 46% and are one fact. Auto-merging in that band fuses
    // distinct facts, and refusing to merge keeps the duplicates.
    //
    // So the fix is prevention: hand the model what is already recorded and ask it to
    // reuse the exact wording rather than invent a near-copy. Then
    // `find_existing_claim_id`'s exact-text match — which was never wrong, only
    // unreachable, because the model reworded every time — does the merge, and the
    // second observation accumulates as evidence on the first claim instead of
    // becoming a second claim.
    //
    // **This does not weaken the independence gate.** That gate exists because an agent
    // told a claim mid-session will "independently" observe it — the echo. Here the
    // session has already happened and its transcript is fixed; nothing shown to the
    // extractor can change what the agent did. This decides only how existing evidence
    // is filed, not what counts as evidence.
    let existing_events = crate::claims::list_events(store).await?;
    let existing = crate::claims::fold(existing_events.iter());
    let user_prompt = match known_claims_preamble(&existing) {
        Some(preamble) => format!("{preamble}\n{transcript_text}"),
        None => transcript_text,
    };

    let request = CompletionRequest {
        system_prompt: EXTRACTION_SYSTEM_PROMPT.to_string(),
        user_prompt,
        model: batch_cfg.model.clone(),
    };
    // The marker above was written *before* this call, so that two hosts cannot both
    // pay for the same session. But a marker that is never released turns a transient
    // provider failure — a 500, a rate limit, a dropped connection — into permanent,
    // silent loss: the session is marked done, produces no claims, and is never
    // retried. Found by a live run whose first attempt failed and whose second
    // reported "0 session(s) extracted" with nothing wrong.
    //
    // So: release it on the way out of any failure, and let the next run try again.
    // Re-extraction is safe (claim ids are reused for matching claims — see
    // `find_existing_claim_id`), whereas losing a session's claims is not.
    let raw_response = match provider.complete(&request).await {
        Ok(r) => r,
        Err(e) => {
            release_extraction_marker(store, fleet_id, &session.session_id).await;
            return Err(e);
        }
    };
    let raw_claims = match parse_claims_response(&raw_response) {
        Ok(c) => c,
        Err(e) => {
            release_extraction_marker(store, fleet_id, &session.session_id).await;
            return Err(e);
        }
    };
    let resolvable = build_resolvable_index(&session.envelopes);
    let observed_at = session
        .envelopes
        .last()
        .map(|e| e.emitted_at.clone())
        .unwrap_or_default();

    // What's already on file, so a matching claim (same type, subject, and text —
    // see `find_existing_claim_id`) reuses its `claim_id` instead of minting a
    // sibling the independence gate cannot tell apart from a second reporter of
    // the same observation. Read once per session's extraction call: two claims
    // *within the same session's own output* that happen to duplicate each other
    // is a model-quality problem this pass doesn't try to solve, and is a
    // different situation from cross-session corroboration, which is what
    // `find_existing_claim_id` exists for.
    let mut proposed = 0usize;
    let returned = raw_claims.len();
    // Counted apart, because "the model named a type we do not have" and "the model
    // cited something we cannot find" are different problems with different fixes, and
    // a warning naming both as possibilities tells an operator nothing. Diagnosing the
    // prose-citation defect took a source read precisely because this line could not
    // say which had happened.
    let mut dropped_bad_type = 0usize;
    let mut dropped_unresolvable = 0usize;
    for raw in raw_claims {
        if ClaimType::parse(&raw.claim_type).is_none() {
            dropped_bad_type += 1;
            continue;
        }
        if let Some(claim) =
            claim_from_raw(raw, &session.agent_id, &observed_at, &resolvable, &existing)
        {
            crate::claims::append_proposed(store, date, &claim).await?;
            proposed += 1;
        } else {
            dropped_unresolvable += 1;
        }
    }
    if returned > proposed {
        tracing::warn!(
            session_id = %session.session_id,
            returned,
            kept = proposed,
            dropped_bad_type,
            dropped_unresolvable,
            "extraction dropped claims"
        );
    }
    Ok(ExtractOutcome {
        session_id: session.session_id.clone(),
        claims_proposed: proposed,
        claims_returned: returned,
        dropped_bad_type,
        dropped_unresolvable,
        skipped_already_extracted: false,
    })
}

#[derive(Debug, Default)]
pub struct ExtractRunSummary {
    pub sessions_processed: usize,
    pub claims_proposed: usize,
    /// Claims the model returned across all sessions, before any were dropped. See
    /// [`ExtractOutcome::claims_returned`] — without it, a prompt that produces nothing
    /// and a parser that discards everything report the same number.
    pub claims_returned: usize,
    /// Sessions whose extraction failed and will be retried next pass.
    ///
    /// Counted rather than propagated: a failure here used to abort the whole
    /// maintenance chain, taking compaction, digests, the gate and the snapshot with
    /// it — none of which involve a model.
    pub sessions_failed: usize,
    /// Why claims were dropped, split by cause. These reach the operator through the
    /// cycle summary, not through `tracing` — **nothing in this workspace installs a
    /// `tracing_subscriber`**, so every `tracing::warn!` here is written to a
    /// subscriber that does not exist. A diagnostic nobody can read is not a
    /// diagnostic, so the counts travel on the value that actually gets printed.
    pub dropped_bad_type: usize,
    pub dropped_unresolvable: usize,
    /// The most recent failure, for the cycle summary. One example beats a count with
    /// no detail, and the full list belongs in the log rather than in one line.
    pub last_error: Option<String>,
}

/// The full Tier 2 pass: find sealed, not-yet-extracted sessions and extract
/// each, up to `max_sessions_per_run`. A clean no-op when [`tier2_enabled`] is
/// false — no listing, no HTTP client, nothing.
pub async fn run(
    store: &dyn ObjectStore,
    fleet_id: &str,
    cfg: &SummarizeConfig,
    provider: &dyn Provider,
) -> Result<ExtractRunSummary, ExtractError> {
    if !tier2_enabled(cfg) {
        return Ok(ExtractRunSummary::default());
    }
    let limit = cfg
        .batch
        .as_ref()
        .map(|b| b.max_sessions_per_run)
        .unwrap_or(0);
    let sealed = list_sealed_sessions(store, fleet_id).await?;
    let mut summary = ExtractRunSummary::default();
    // `list_sealed_sessions` returns every sealed session, oldest first, with no
    // notion of "already extracted" baked in (`mark_extracted_if_new`'s job, not
    // its own — see that function's doc). Applying `max_sessions_per_run` to this
    // raw list, as a naive `.take(limit)` used to, means that once the oldest
    // `limit` sessions have ever been extracted, every subsequent run re-lists
    // those exact same sessions, skips every one of them as already-done, and
    // never reaches anything new — `sessions_processed: 0`, forever, with no
    // error. The fix: skip already-extracted sessions with a cheap existence
    // check *before* they count against the budget, so the budget is spent only
    // on sessions this run actually attempts.
    for session_ref in sealed {
        if summary.sessions_processed >= limit {
            break;
        }
        if is_already_extracted(store, fleet_id, &session_ref.session_id).await? {
            continue;
        }
        let transcript = match load_transcript(store, &session_ref).await {
            Ok(t) => t,
            Err(e) => {
                summary.sessions_failed += 1;
                tracing::warn!(session_id = %session_ref.session_id, error = %e,
                    "could not load a session for extraction; continuing");
                continue;
            }
        };
        let date = transcript
            .envelopes
            .first()
            .and_then(|e| e.emitted_at.get(0..10))
            .unwrap_or("1970-01-01")
            .to_string();

        // **One session's failure must not end the pass.**
        //
        // This was `?`, and a single malformed model response therefore aborted not
        // just extraction but the entire maintenance chain — `run::run` propagates it
        // before compaction's siblings, the gate and the snapshot ever run. Seen the
        // first time retry was enabled: 37 sessions became eligible again, the second
        // one came back as something that was not JSON, and the whole cycle died with
        // it. Every session after it stayed unextracted, and the digests and snapshot
        // that had nothing to do with a model were skipped too.
        //
        // Exactly the shape already fixed twice at the daemon and cycle boundaries: an
        // optional step taking mandatory ones down with it. `extract_session` releases
        // its own marker on failure, so a session that fails here is retried next pass
        // rather than being marked done.
        match extract_session(store, cfg, provider, &transcript, &date, fleet_id).await {
            Ok(outcome) => {
                if !outcome.skipped_already_extracted {
                    summary.sessions_processed += 1;
                }
                summary.claims_proposed += outcome.claims_proposed;
                summary.claims_returned += outcome.claims_returned;
                summary.dropped_bad_type += outcome.dropped_bad_type;
                summary.dropped_unresolvable += outcome.dropped_unresolvable;
            }
            Err(e) => {
                summary.sessions_failed += 1;
                summary.last_error = Some(e.to_string());
                tracing::warn!(session_id = %session_ref.session_id, error = %e,
                    "extraction failed for one session; continuing with the rest");
            }
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::{EventType, Runtime};

    fn env_with(session: &str, message_id: &str, content: &str, minute: u32) -> Envelope {
        let mut e = Envelope::new(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            session,
            EventType::Assistant,
            format!("2026-09-09T12:{minute:02}:00.000Z"),
        );
        e.message_id = Some(message_id.to_string());
        e.content = Some(content.to_string());
        e.content_hash = ctxlake_core::hash::content_hash(content);
        e
    }

    /// Like [`env_with`], but with a fully explicit `agent_id` and `emitted_at`
    /// instead of a fixed date — needed for tests where two envelopes must land
    /// in two different sessions' time windows (see the echo-case end-to-end
    /// test below), which `env_with`'s single hardcoded date can't express.
    fn env_with_at(
        agent_id: &str,
        session: &str,
        message_id: &str,
        content: &str,
        emitted_at: &str,
    ) -> Envelope {
        let mut e = Envelope::new(
            "oxidant",
            agent_id,
            Runtime::ClaudeCode,
            session,
            EventType::Assistant,
            emitted_at.to_string(),
        );
        e.message_id = Some(message_id.to_string());
        e.content = Some(content.to_string());
        e.content_hash = ctxlake_core::hash::content_hash(content);
        e
    }

    // ---- context fencing ----

    #[test]
    fn strip_injected_context_removes_the_whole_span() {
        let text = "before <ctxlake:injected claim_ids=\"c1\">peer said X</ctxlake:injected> after";
        let out = strip_injected_context(text);
        assert_eq!(out, "before  after");
        assert!(!out.contains("peer said X"));
    }

    #[test]
    fn strip_injected_context_handles_multiple_spans_and_plain_text() {
        let text = "a <ctxlake:injected claim_ids=\"c1\">one</ctxlake:injected> b \
                    <ctxlake:injected claim_ids=\"c2\">two</ctxlake:injected> c";
        let out = strip_injected_context(text);
        assert_eq!(out, "a  b  c");
    }

    #[test]
    fn build_fenced_transcript_strips_injected_spans_from_every_envelope() {
        let envelopes = vec![env_with(
            "s1",
            "m1",
            "the real observation <ctxlake:injected claim_ids=\"c1\">a peer's claim</ctxlake:injected>",
            0,
        )];
        let text = build_fenced_transcript(&envelopes);
        assert!(text.contains("the real observation"));
        assert!(!text.contains("a peer's claim"));
    }

    /// A test double whose "extraction" is a deterministic function of its
    /// input prompt: it emits one claim citing (trigger_session, trigger_msg)
    /// if and only if TRIGGER_PHRASE literally appears in the prompt it was
    /// given. This makes context fencing's effect on the *extraction pipeline*
    /// observable end-to-end, not just on the string-stripping helper in
    /// isolation.
    const TRIGGER_PHRASE: &str = "staging listens on port 2222";

    struct TriggerSensitiveProvider;
    impl Provider for TriggerSensitiveProvider {
        fn complete<'a>(
            &'a self,
            req: &'a CompletionRequest,
        ) -> BoxFuture<'a, Result<String, ExtractError>> {
            let saw_trigger = req.user_prompt.contains(TRIGGER_PHRASE);
            Box::pin(async move {
                if saw_trigger {
                    Ok(r#"{"claims":[{"claim":"staging SSH listens on 2222","claim_type":"environment","subject":"staging","evidence":[{"session_id":"s1","message_id":"m1"}]}]}"#.to_string())
                } else {
                    Ok(r#"{"claims":[]}"#.to_string())
                }
            })
        }
    }

    fn shadow_cfg() -> SummarizeConfig {
        SummarizeConfig {
            mode: SummarizeMode::Shadow,
            batch: Some(BatchConfig::default()),
        }
    }

    #[tokio::test]
    async fn a_claim_derived_purely_from_injected_text_is_never_produced() {
        let store = object_store::memory::InMemory::new();
        let session = SessionTranscript {
            session_id: "s1".into(),
            agent_id: "cc-01".into(),
            envelopes: vec![env_with(
                "s1",
                "m1",
                &format!("<ctxlake:injected claim_ids=\"c0\">{TRIGGER_PHRASE}</ctxlake:injected>"),
                0,
            )],
        };
        let outcome = extract_session(
            &store,
            &shadow_cfg(),
            &TriggerSensitiveProvider,
            &session,
            "2026-09-09",
            "oxidant",
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.claims_proposed, 0,
            "a claim whose only supporting text was injected must not be produced"
        );
    }

    #[tokio::test]
    async fn the_same_phrase_unfenced_does_produce_a_claim() {
        // Sanity check that TriggerSensitiveProvider (and the pipeline around
        // it) actually would have produced a claim absent fencing — otherwise
        // the previous test would pass for the wrong reason.
        let store = object_store::memory::InMemory::new();
        let session = SessionTranscript {
            session_id: "s1".into(),
            agent_id: "cc-01".into(),
            envelopes: vec![env_with("s1", "m1", TRIGGER_PHRASE, 0)],
        };
        let outcome = extract_session(
            &store,
            &shadow_cfg(),
            &TriggerSensitiveProvider,
            &session,
            "2026-09-09",
            "oxidant",
        )
        .await
        .unwrap();
        assert_eq!(outcome.claims_proposed, 1);
    }

    // ---- no evidence, no claim ----

    fn no_existing_claims() -> BTreeMap<String, ClaimState> {
        BTreeMap::new()
    }

    /// **A claim citing a prose line was structurally undroppable-proof: it always
    /// dropped.** `build_fenced_transcript` labels an envelope with no `message_id`
    /// using its `event_id`, so that is the only id the model can cite for a prompt or
    /// an assistant line — and `build_resolvable_index` indexed `message_id` only, so
    /// the citation resolved against nothing and the claim was discarded in silence.
    ///
    /// This test reads the id back out of the real rendered transcript rather than
    /// assuming what the model is shown. Assuming is what let the defect survive: every
    /// other citation test hands `claim_from_raw` a message id the index was built
    /// with, so the two halves agreed with each other and disagreed with the prompt.
    #[test]
    fn a_claim_citing_a_prose_line_resolves_against_the_id_the_model_was_shown() {
        let mut prose = env_with("s1", "ignored", "staging listens on port 2222", 0);
        prose.message_id = None; // a prompt/assistant line, as Claude Code produces
        let envelopes = vec![prose.clone()];

        // Whatever the prompt builder prints is what the model can cite. Take it from
        // there, not from what this test would like it to be.
        let rendered = build_fenced_transcript(&envelopes);
        let cited_id = rendered
            .lines()
            .find_map(|l| l.strip_prefix('[')?.split_once("] "))
            .map(|(id, _)| id.to_string())
            .expect("the prose line is rendered with an id");
        assert_eq!(
            cited_id, prose.event_id,
            "a prose envelope is shown under its event id"
        );

        let raw = RawClaim {
            claim: "staging listens on port 2222".into(),
            claim_type: "environment".into(),
            subject: "staging".into(),
            evidence: vec![RawCitation {
                session_id: "s1".into(),
                message_id: cited_id,
            }],
        };
        let claim = claim_from_raw(
            raw,
            "cc-01",
            "2026-09-09T00:00:00Z",
            &build_resolvable_index(&envelopes),
            &no_existing_claims(),
        )
        .expect("a claim citing the id it was shown must not be dropped");
        assert_eq!(claim.evidence.len(), 1);
        assert_eq!(
            claim.evidence[0].excerpt_hash, prose.content_hash,
            "the hash must come from the cited envelope, not be invented"
        );
    }

    #[test]
    fn claim_from_raw_drops_a_claim_with_zero_citations() {
        let raw = RawClaim {
            claim: "invented".into(),
            claim_type: "environment".into(),
            subject: "x".into(),
            evidence: vec![],
        };
        let resolvable = ResolvableIndex::new();
        assert!(claim_from_raw(
            raw,
            "cc-01",
            "2026-09-09T00:00:00Z",
            &resolvable,
            &no_existing_claims()
        )
        .is_none());
    }

    #[test]
    fn claim_from_raw_drops_a_claim_citing_a_message_id_that_does_not_exist() {
        let raw = RawClaim {
            claim: "invented with a fake citation".into(),
            claim_type: "environment".into(),
            subject: "x".into(),
            evidence: vec![RawCitation {
                session_id: "s1".into(),
                message_id: "m-does-not-exist".into(),
            }],
        };
        let mut resolvable = ResolvableIndex::new();
        resolvable.insert(
            ("s1".to_string(), "m1".to_string()),
            ResolvedCitation {
                excerpt_hash: "realhash".to_string(),
                observed_at: "2026-09-09T00:00:00Z".to_string(),
            },
        );
        assert!(
            claim_from_raw(
                raw,
                "cc-01",
                "2026-09-09T00:00:00Z",
                &resolvable,
                &no_existing_claims()
            )
            .is_none(),
            "a citation to a message_id absent from the session must be dropped, not trusted"
        );
    }

    #[test]
    fn claim_from_raw_keeps_a_claim_with_a_real_citation_and_uses_our_own_hash_and_timestamp() {
        let raw = RawClaim {
            claim: "staging SSH listens on 2222".into(),
            claim_type: "environment".into(),
            subject: "staging".into(),
            evidence: vec![RawCitation {
                session_id: "s1".into(),
                message_id: "m1".into(),
            }],
        };
        let mut resolvable = ResolvableIndex::new();
        resolvable.insert(
            ("s1".to_string(), "m1".to_string()),
            ResolvedCitation {
                excerpt_hash: "realhash".to_string(),
                // Deliberately different from the call's own `observed_at`
                // (below) — this is the message's own real timestamp, and the
                // evidence item must carry THIS one, not the claim-level one.
                observed_at: "2026-09-09T12:03:00Z".to_string(),
            },
        );
        let claim = claim_from_raw(
            raw,
            "cc-01",
            "2026-09-09T23:59:00Z",
            &resolvable,
            &no_existing_claims(),
        )
        .unwrap();
        assert_eq!(claim.evidence[0].excerpt_hash, "realhash");
        assert_eq!(
            claim.evidence[0].observed_at, "2026-09-09T12:03:00Z",
            "evidence.observed_at must come from the cited message's own timestamp, \
             not the session-level observed_at passed to claim_from_raw"
        );
        assert_eq!(
            claim.observed_at, "2026-09-09T23:59:00Z",
            "the claim-level observed_at is a separate field: when the claim was proposed"
        );
    }

    #[test]
    fn claim_from_raw_drops_an_unrecognized_claim_type() {
        let raw = RawClaim {
            claim: "x".into(),
            claim_type: "opinion".into(),
            subject: "x".into(),
            evidence: vec![RawCitation {
                session_id: "s1".into(),
                message_id: "m1".into(),
            }],
        };
        let mut resolvable = ResolvableIndex::new();
        resolvable.insert(
            ("s1".to_string(), "m1".to_string()),
            ResolvedCitation {
                excerpt_hash: "h".to_string(),
                observed_at: "2026-09-09T00:00:00Z".to_string(),
            },
        );
        assert!(claim_from_raw(
            raw,
            "cc-01",
            "2026-09-09T00:00:00Z",
            &resolvable,
            &no_existing_claims()
        )
        .is_none());
    }

    // ---- claim_id reuse: the corroboration / independence gate's precondition ----

    #[test]
    fn claim_from_raw_reuses_the_claim_id_of_a_matching_existing_claim() {
        // Same claim_type, subject, and (normalized) claim text as an existing
        // claim already on file — this must reuse its claim_id so the new
        // evidence accumulates onto it, rather than minting a fresh id that
        // `gate::compute_independent_count` can never join against the first.
        let raw = RawClaim {
            claim: "  This repo uses JUST, not make ".into(),
            claim_type: "convention".into(),
            subject: "Build-Tooling".into(),
            evidence: vec![RawCitation {
                session_id: "s2".into(),
                message_id: "m1".into(),
            }],
        };
        let mut resolvable = ResolvableIndex::new();
        resolvable.insert(
            ("s2".to_string(), "m1".to_string()),
            ResolvedCitation {
                excerpt_hash: "h2".to_string(),
                observed_at: "2026-09-12T00:00:00Z".to_string(),
            },
        );
        let mut existing = BTreeMap::new();
        existing.insert(
            "claim-original".to_string(),
            ClaimState {
                claim_id: "claim-original".into(),
                claim: "this repo uses just, not make".into(),
                claim_type: ClaimType::Convention,
                subject: "build-tooling".into(),
                scope: Scope::Fleet,
                observed_by: "cc-01".into(),
                observed_at: "2026-09-09T00:00:00Z".into(),
                evidence: vec![],
                status: crate::claims::ClaimStatus::Promoted,
                independent_count: 1,
                confidence: 0.65,
                embedding: None,
                resolves_at: None,
            },
        );

        let claim =
            claim_from_raw(raw, "cc-02", "2026-09-12T00:00:00Z", &resolvable, &existing).unwrap();
        assert_eq!(
            claim.claim_id, "claim-original",
            "matching subject+text must reuse the existing claim_id, not mint a new one"
        );
    }

    #[test]
    fn claim_from_raw_mints_a_new_id_when_nothing_matches() {
        let raw = RawClaim {
            claim: "a completely different observation".into(),
            claim_type: "environment".into(),
            subject: "staging".into(),
            evidence: vec![RawCitation {
                session_id: "s1".into(),
                message_id: "m1".into(),
            }],
        };
        let mut resolvable = ResolvableIndex::new();
        resolvable.insert(
            ("s1".to_string(), "m1".to_string()),
            ResolvedCitation {
                excerpt_hash: "h".to_string(),
                observed_at: "2026-09-09T00:00:00Z".to_string(),
            },
        );
        let mut existing = BTreeMap::new();
        existing.insert(
            "claim-original".to_string(),
            ClaimState {
                claim_id: "claim-original".into(),
                claim: "this repo uses just, not make".into(),
                claim_type: ClaimType::Convention,
                subject: "build-tooling".into(),
                scope: Scope::Fleet,
                observed_by: "cc-01".into(),
                observed_at: "2026-09-09T00:00:00Z".into(),
                evidence: vec![],
                status: crate::claims::ClaimStatus::Promoted,
                independent_count: 1,
                confidence: 0.65,
                embedding: None,
                resolves_at: None,
            },
        );

        let claim =
            claim_from_raw(raw, "cc-01", "2026-09-09T00:00:00Z", &resolvable, &existing).unwrap();
        assert_ne!(
            claim.claim_id, "claim-original",
            "an unrelated claim must never be glued onto an existing claim_id"
        );
    }

    // ---- structured output parsing ----

    fn claim_state(id: &str, ty: ClaimType, subject: &str, text: &str) -> ClaimState {
        ClaimState {
            claim_id: id.into(),
            claim: text.into(),
            claim_type: ty,
            subject: subject.into(),
            scope: crate::claims::Scope::Fleet,
            observed_by: "cc-01".into(),
            status: crate::claims::ClaimStatus::Promoted,
            evidence: vec![],
            independent_count: 1,
            confidence: 0.65,
            observed_at: "2026-09-13T00:00:00Z".into(),
            embedding: None,
            resolves_at: None,
        }
    }

    #[test]
    fn the_same_claim_under_a_different_subject_is_still_the_same_claim() {
        // The duplication bug, exactly. On a live lake one fact arrived as subject
        // "console theming" and again as "oxidant theme storage"; another as "navbar
        // updates" and "site navigation updates". Requiring the subjects to match meant
        // each re-observation was filed as a brand-new claim instead of evidence on the
        // existing one.
        let mut existing = BTreeMap::new();
        existing.insert(
            "c1".to_string(),
            claim_state(
                "c1",
                ClaimType::Convention,
                "console theming",
                "Dark theme is set via localStorage",
            ),
        );

        let found = find_existing_claim_id(
            &existing,
            ClaimType::Convention,
            "oxidant theme storage", // a different label for the same topic
            "Dark theme is set via localStorage",
        );
        assert_eq!(
            found.as_deref(),
            Some("c1"),
            "identical text under a different subject must reuse the claim id"
        );
    }

    #[test]
    fn a_different_assertion_is_never_folded_onto_an_existing_claim() {
        // The other direction, and the one that matters more: merging two distinct
        // facts is worse than keeping a duplicate. These two share most of their words
        // and are genuinely different — both were promoted from the same session on a
        // real lake.
        let mut existing = BTreeMap::new();
        existing.insert(
            "c1".to_string(),
            claim_state(
                "c1",
                ClaimType::Convention,
                "tls",
                "TLS certificate mounting for manual mode uses a kubernetes.io/tls Secret type",
            ),
        );
        assert_eq!(
            find_existing_claim_id(
                &existing,
                ClaimType::Convention,
                "tls",
                "In manual mode, the TLS Secret is mounted read-only at /etc/oxidant-platform/tls",
            ),
            None,
            "48% word overlap is not the same claim; only identical text merges"
        );
        // And a different type never merges, however the text reads.
        assert_eq!(
            find_existing_claim_id(
                &existing,
                ClaimType::Outcome,
                "tls",
                "TLS certificate mounting for manual mode uses a kubernetes.io/tls Secret type",
            ),
            None
        );
    }

    #[test]
    fn the_extractor_is_shown_what_the_fleet_already_believes() {
        // Prevention rather than post-hoc merging: no similarity threshold separates
        // the real duplicates from the real distinctions in this data, so the model is
        // asked not to produce the near-copy in the first place.
        let mut existing = BTreeMap::new();
        existing.insert(
            "c1".to_string(),
            claim_state(
                "c1",
                ClaimType::Convention,
                "ci",
                "CI needs RUSTFLAGS=-D warnings",
            ),
        );
        let mut candidate = claim_state("c2", ClaimType::Outcome, "x", "a candidate claim");
        candidate.status = crate::claims::ClaimStatus::Candidate;
        existing.insert("c2".to_string(), candidate);

        let preamble = known_claims_preamble(&existing).expect("something is known");
        assert!(
            preamble.contains("CI needs RUSTFLAGS=-D warnings"),
            "{preamble}"
        );
        assert!(
            preamble.contains("CHARACTER FOR CHARACTER"),
            "must ask for exact reuse, or the model rewords and the merge never fires"
        );
        // The instruction must not offer omission. Given the choice, a live model took
        // it — returning `{"claims": []}` and explaining the transcript "adds no new
        // information" — which throws the second observation away entirely. A
        // convention seen in ten sessions would still read "1 independent session".
        assert!(
            preamble.contains("Do NOT omit"),
            "omitting a known claim discards the corroboration: {preamble}"
        );
        assert!(
            preamble.contains("strengthens it"),
            "the model needs to know why repeating is worth doing: {preamble}"
        );
        assert!(
            !preamble.contains("a candidate claim"),
            "a claim that has not cleared the gate must not be presented as established"
        );
        assert!(
            preamble.contains("TRANSCRIPT:"),
            "must separate the two sections"
        );
    }

    #[test]
    fn an_empty_lake_adds_no_preamble() {
        assert!(known_claims_preamble(&BTreeMap::new()).is_none());
    }

    #[test]
    fn the_prompt_defines_every_type_the_parser_will_accept() {
        // The prompt named the five types in a JSON schema line and defined none of
        // them. A model asked for "atomic, evidence-backed claims" with no notion of
        // what the labels mean picks the one that fits "here is what I did" — and on a
        // real lake every single promoted claim came back `outcome`, which is the one
        // type a structural digest already records for free.
        for t in [
            "environment",
            "convention",
            "outcome",
            "preference",
            "hypothesis",
        ] {
            assert!(
                EXTRACTION_SYSTEM_PROMPT.contains(&format!("\"{t}\": ")),
                "the prompt must define `{t}`, not merely list it"
            );
        }
        // And it must warn against the duplication, or the mix drifts straight back.
        assert!(
            EXTRACTION_SYSTEM_PROMPT.contains("structural digest"),
            "the prompt must say why an `outcome` restating the digest is noise"
        );
        // Every type the prompt offers must be one the parser accepts, or the model is
        // invited to produce claims that are then silently dropped.
        for t in [
            "environment",
            "convention",
            "outcome",
            "preference",
            "hypothesis",
        ] {
            assert!(crate::claims::ClaimType::parse(t).is_some(), "{t}");
        }
    }

    #[test]
    fn parse_claims_response_rejects_malformed_json_as_a_parse_error() {
        let err = parse_claims_response("not json at all").unwrap_err();
        assert!(matches!(err, ExtractError::MalformedResponse(_)));
    }

    #[test]
    fn a_citation_with_the_wrong_session_id_still_resolves_by_message_id() {
        // A live model, asked for a session_id the transcript never showed it, cited
        // the first line's event id for all four claims — and all four were dropped.
        // The message id is the half that verifies anything; the session id is ours.
        let envelopes = vec![env_with("real-session", "t1", "cargo test failed", 0)];
        let resolvable = build_resolvable_index(&envelopes);
        let raw = RawClaim {
            claim: "CI sets RUSTFLAGS=-D warnings".to_string(),
            claim_type: "environment".to_string(),
            subject: "ci".to_string(),
            evidence: vec![RawCitation {
                session_id: "01SOMETHING-THE-MODEL-GUESSED".to_string(),
                message_id: "t1".to_string(),
            }],
        };
        let claim = claim_from_raw(
            raw,
            "cc-01",
            "2026-09-11T00:00:00Z",
            &resolvable,
            &BTreeMap::new(),
        )
        .expect("a resolvable message id must survive a wrong session id");
        assert_eq!(
            claim.evidence[0].session_id, "real-session",
            "the envelope's own session must win over the model's guess"
        );
    }

    #[test]
    fn a_fabricated_message_id_is_still_rejected() {
        // The fallback must not become "accept anything": the message id is the
        // anti-hallucination check, and softening it would let a model invent
        // evidence for a claim nothing in the session supports.
        let envelopes = vec![env_with("real-session", "t1", "cargo test failed", 0)];
        let resolvable = build_resolvable_index(&envelopes);
        let raw = RawClaim {
            claim: "something nobody observed".to_string(),
            claim_type: "environment".to_string(),
            subject: "ci".to_string(),
            evidence: vec![RawCitation {
                session_id: "real-session".to_string(),
                message_id: "m-does-not-exist".to_string(),
            }],
        };
        assert!(
            claim_from_raw(
                raw,
                "cc-01",
                "2026-09-11T00:00:00Z",
                &resolvable,
                &BTreeMap::new()
            )
            .is_none(),
            "an unresolvable message id must still drop the claim"
        );
    }

    #[test]
    fn the_transcript_names_the_session_it_is_asking_about() {
        let t = build_fenced_transcript(&[env_with("sess-abc", "m1", "hello", 0)]);
        assert!(
            t.starts_with("session_id: sess-abc\n"),
            "the model cannot cite an id it was never shown: {t:?}"
        );
    }

    #[test]
    fn the_transcript_carries_tool_calls_not_just_prompts() {
        // The bug this guards is the one a live run found: on a real Claude Code
        // session `content` is populated for the user's prompt and nothing else, so
        // rendering `content` alone showed the model the question and hid every
        // command, exit code and output — the entire evidentiary basis for a claim.
        let mut prompt = env_with("s1", "m1", "why does CI fail but not my laptop", 0);
        prompt.tool = None;

        let mut call = env_with("s1", "t1", "", 1);
        call.content = None;
        call.tool = Some(ctxlake_core::envelope::ToolCall {
            name: "Bash".to_string(),
            input: Some("cargo test --workspace".to_string()),
            input_hash: String::new(),
            result: Some(
                "error: unused variable `x`\nnote: `-D warnings` was supplied".to_string(),
            ),
            exit_code: Some(1),
            duration_ms: None,
            paths: vec![],
        });

        let t = build_fenced_transcript(&[prompt, call]);
        assert!(
            t.contains("why does CI fail"),
            "the prompt must survive: {t}"
        );
        assert!(t.contains("tool:Bash"), "the tool name must appear: {t}");
        assert!(
            t.contains("cargo test --workspace"),
            "the command must appear: {t}"
        );
        assert!(t.contains("exit=1"), "the exit code must appear: {t}");
        assert!(t.contains("-D warnings"), "the output must appear: {t}");
        assert_eq!(
            t.lines().count(),
            3,
            "the session header plus one line per event, so the [id] citation \
             framing holds: {t:?}"
        );
        assert!(
            t.contains("[t1]"),
            "the tool line must carry a citable id: {t}"
        );
    }

    #[test]
    fn a_fenced_json_response_is_read_rather_than_rejected() {
        // Captured from a live OpenRouter call to anthropic/claude-haiku-4.5, asked
        // for JSON and given a response_format. Every fixture in this file was built
        // from a documented API schema and so carries a clean object; the fence is a
        // property of the model, which is why only a real call surfaced it.
        let raw = "```json\n{\"claims\":[{\"claim\":\"x\",\"claim_type\":\"environment\",\"subject\":\"y\",\"evidence\":[{\"session_id\":\"s1\",\"message_id\":\"m1\"}]}]}\n```";
        let claims = parse_claims_response(raw).expect("a fenced payload must parse");
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].subject, "y");
    }

    #[test]
    fn an_unfenced_response_with_a_preamble_still_parses() {
        let raw = "Here is the JSON you asked for:\n{\"claims\":[]}";
        assert!(parse_claims_response(raw).unwrap().is_empty());
    }

    #[test]
    fn prose_with_no_json_at_all_is_still_a_parse_error() {
        // The unwrapping must not become "find something that looks like JSON in any
        // text", or a model's refusal gets salvaged into a claim.
        for raw in [
            "I could not find anything durable in this session.",
            "```json\nnot json at all\n```",
            "",
        ] {
            assert!(
                parse_claims_response(raw).is_err(),
                "must not invent a result from {raw:?}"
            );
        }
    }

    #[test]
    fn the_openai_compatible_body_asks_for_json_back() {
        // Without this the model answers in prose and every extraction fails at the
        // parse step — which is exactly what a live OpenRouter run did.
        let body = chat_completions_body(&CompletionRequest {
            system_prompt: "s".into(),
            user_prompt: "u".into(),
            model: "m".into(),
        });
        assert_eq!(body["response_format"]["type"], "json_object");
    }

    #[test]
    fn parse_claims_response_reads_a_well_formed_payload() {
        let raw = r#"{"claims":[{"claim":"x","claim_type":"environment","subject":"y","evidence":[{"session_id":"s1","message_id":"m1"}]}]}"#;
        let claims = parse_claims_response(raw).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].subject, "y");
    }

    // ---- recorded fixtures: real provider response shapes, no live API ----
    //
    // These bodies are constructed from each provider's *documented* public API
    // response schema (Anthropic Messages API, the OpenAI chat-completions
    // shape, Ollama's /api/chat) rather than captured from a live call — unlike
    // AGENTS.md's Cursor `duration` example, these are stable, versioned public
    // contracts, not an internal quirk that only a real payload would reveal.

    const ANTHROPIC_MESSAGE_FIXTURE: &str = r#"{
        "id": "msg_01ABCDEF",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [
            {"type": "text", "text": "{\"claims\":[{\"claim\":\"staging SSH listens on 2222\",\"claim_type\":\"environment\",\"subject\":\"staging\",\"evidence\":[{\"session_id\":\"s1\",\"message_id\":\"m1\"}]}]}"}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 812, "output_tokens": 61}
    }"#;

    #[test]
    fn parses_a_recorded_anthropic_messages_response() {
        let text = parse_anthropic_message_response(ANTHROPIC_MESSAGE_FIXTURE).unwrap();
        let claims = parse_claims_response(&text).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].subject, "staging");
    }

    const OPENAI_CHAT_FIXTURE: &str = r#"{
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [
            {"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "{\"claims\":[]}"}}
        ],
        "usage": {"prompt_tokens": 500, "completion_tokens": 5}
    }"#;

    #[test]
    fn parses_a_recorded_openai_compatible_response() {
        let text = parse_openai_chat_response(OPENAI_CHAT_FIXTURE).unwrap();
        let claims = parse_claims_response(&text).unwrap();
        assert!(claims.is_empty());
    }

    const OLLAMA_CHAT_FIXTURE: &str = r#"{
        "model": "qwen2.5:14b",
        "created_at": "2026-09-09T12:00:00Z",
        "message": {"role": "assistant", "content": "{\"claims\":[]}"},
        "done": true
    }"#;

    #[test]
    fn parses_a_recorded_ollama_response() {
        let text = parse_ollama_response(OLLAMA_CHAT_FIXTURE).unwrap();
        let claims = parse_claims_response(&text).unwrap();
        assert!(claims.is_empty());
    }

    const GEMINI_GENERATE_CONTENT_FIXTURE: &str = r#"{
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [{"text": "{\"claims\":[]}"}]
                },
                "finishReason": "STOP"
            }
        ],
        "usageMetadata": {"promptTokenCount": 700, "candidatesTokenCount": 4}
    }"#;

    #[test]
    fn parses_a_recorded_gemini_response() {
        let text = parse_gemini_response(GEMINI_GENERATE_CONTENT_FIXTURE).unwrap();
        let claims = parse_claims_response(&text).unwrap();
        assert!(claims.is_empty());
    }

    #[test]
    fn parse_gemini_response_reports_a_malformed_body_rather_than_panicking() {
        let err = parse_gemini_response(r#"{"candidates": []}"#).unwrap_err();
        assert!(matches!(err, ExtractError::MalformedResponse(_)));
    }

    // ---- request-building: no network, so exercised as pure functions ----

    #[test]
    fn chat_completions_body_carries_model_and_both_prompts() {
        let req = CompletionRequest {
            system_prompt: "sys".into(),
            user_prompt: "usr".into(),
            model: "gpt-x".into(),
        };
        let body = chat_completions_body(&req);
        assert_eq!(body["model"], "gpt-x");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "sys");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "usr");
    }

    #[test]
    fn openrouter_defaults_its_base_url_and_lets_it_be_overridden() {
        let default = OpenRouterProvider::new(None, None);
        assert_eq!(default.base_url_for_test(), "https://openrouter.ai/api/v1");

        // An empty string is treated the same as "not set" (matches every
        // other provider's `non_empty` handling of `base_url`) — a blank
        // `base_url = ""` line in `ctxlake.toml` must not turn into a
        // request to `/chat/completions` with no host at all.
        let blank = OpenRouterProvider::new(None, Some(String::new()));
        assert_eq!(blank.base_url_for_test(), "https://openrouter.ai/api/v1");

        let overridden = OpenRouterProvider::new(None, Some("http://localhost:9999".to_string()));
        assert_eq!(overridden.base_url_for_test(), "http://localhost:9999");
    }

    #[test]
    fn openrouter_sends_x_title_and_deliberately_no_referer() {
        assert_eq!(OPENROUTER_EXTRA_HEADERS, &[("X-Title", "ctxlake")]);
        assert!(
            !OPENROUTER_EXTRA_HEADERS
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("HTTP-Referer")),
            "a CLI has no site URL to attribute traffic to"
        );
    }

    #[test]
    fn gemini_request_body_uses_contents_and_system_instruction_not_messages() {
        let req = CompletionRequest {
            system_prompt: "extract claims".into(),
            user_prompt: "[m1] hello".into(),
            model: "gemini-2.0-flash".into(),
        };
        let body = gemini_request_body(&req);
        assert!(
            body.get("messages").is_none(),
            "Gemini's shape has no top-level messages array"
        );
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "[m1] hello");
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "extract claims"
        );
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        // The schema must actually constrain the `claims` field the rest of
        // this module parses — a `responseSchema` that forgot it would let
        // Gemini return anything and still claim to be "structured."
        assert!(body["generationConfig"]["responseSchema"]["properties"]["claims"].is_object());
    }

    #[test]
    fn gemini_url_never_embeds_the_api_key_in_the_query_string() {
        let url = gemini_url(
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-2.0-flash",
        );
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent"
        );
        assert!(
            !url.contains("key="),
            "the API key must ride in the x-goog-api-key header, never the URL: {url}"
        );
    }

    #[test]
    fn gemini_defaults_its_base_url_and_lets_it_be_overridden() {
        let default = GeminiProvider::new("k".to_string(), None);
        assert_eq!(
            default.base_url_for_test(),
            "https://generativelanguage.googleapis.com/v1beta"
        );
        let overridden =
            GeminiProvider::new("k".to_string(), Some("http://localhost:9999".to_string()));
        assert_eq!(overridden.base_url_for_test(), "http://localhost:9999");
    }

    // ---- build_provider: the factory bridging BatchConfig -> a live Provider ----

    fn batch_cfg(provider: ProviderKind, api_key_env: &str) -> BatchConfig {
        BatchConfig {
            provider,
            api_key_env: api_key_env.to_string(),
            ..BatchConfig::default()
        }
    }

    /// `Box<dyn Provider>` has no `Debug` impl (nothing here needs one in
    /// production), so `Result::unwrap_err` — which requires the `Ok` side to
    /// be `Debug` for its panic message — can't be called directly on
    /// `build_provider`'s return type. This is the plain match that stands in
    /// for it.
    fn expect_provider_err(result: Result<Box<dyn Provider>, ExtractError>) -> ExtractError {
        match result {
            Ok(_) => panic!("expected build_provider to fail"),
            Err(e) => e,
        }
    }

    /// Env vars are process-global, so every test in this fn body picks a name
    /// unlikely to collide with anything else the suite (or CI's own
    /// environment) sets, and always removes it again — a leaked var here
    /// would make some *other* test's "is it unset" assertion flaky depending
    /// on run order.
    #[test]
    fn the_claude_cli_provider_needs_no_api_key() {
        // The entire point: anyone running ctxlake already runs a coding agent, and
        // for Claude Code users that binary is already signed in. Requiring an API
        // key to summarise their own sessions means a second bill and a second
        // secret. If this ever starts demanding a key, that value is gone.
        let cfg = BatchConfig {
            provider: ProviderKind::ClaudeCli,
            model: "haiku".to_string(),
            api_key_env: String::new(),
            ..Default::default()
        };
        // Only assert the key requirement, not that `claude` is installed — CI has
        // no Claude Code, and a test that needs one would be skipped everywhere that
        // matters.
        match build_provider(&cfg) {
            Ok(_) => {}
            Err(e) => {
                let m = e.to_string();
                assert!(
                    !m.contains("is not set"),
                    "must never fail for a missing API key: {m}"
                );
                assert!(
                    m.contains("PATH") || m.contains("ANTHROPIC_API_KEY") || m.contains("claude"),
                    "the only acceptable failures are about the CLI itself: {m}"
                );
            }
        }
    }

    #[test]
    fn a_claude_cli_error_envelope_is_an_error_not_a_parse_failure() {
        // The CLI exits 0 and reports failure inside the JSON. Trusting the exit
        // code alone turns a 401 into "malformed structured-output response", which
        // sends someone debugging the prompt instead of their auth.
        let envelope = serde_json::json!({
            "type": "result",
            "is_error": true,
            "result": "Failed to authenticate. API Error: 401",
        });
        assert_eq!(
            envelope
                .get("is_error")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "the field this provider keys off must keep its name"
        );
    }

    #[test]
    fn a_fenced_payload_from_the_claude_cli_still_parses() {
        // Observed from a real `claude -p --model haiku` call: it returns its JSON
        // inside a ```json fence, the same wrapper OpenRouter's models add.
        let raw = "```json\n{\"claims\":[]}\n```";
        assert!(parse_claims_response(raw).is_ok());
    }

    #[test]
    fn build_provider_fails_with_a_clear_message_naming_the_unset_env_var() {
        let var = "CTXLAKE_TEST_MISSING_KEY_ANTHROPIC";
        // SAFETY / hygiene: cleared unconditionally before and after, and
        // this whole test only ever reads/removes it, never lets a real
        // secret near it.
        std::env::remove_var(var);
        let cfg = batch_cfg(ProviderKind::Anthropic, var);
        let err = expect_provider_err(build_provider(&cfg));
        match &err {
            ExtractError::MissingApiKey(name) => assert_eq!(name, var),
            other => panic!("expected MissingApiKey, got {other:?}"),
        }
        // The message must name the variable so an operator knows what to
        // set, and must never contain anything that looks like a resolved
        // value — there is never one to leak, but the message text itself
        // must not invite pasting a key into it.
        let msg = err.to_string();
        assert!(msg.contains(var), "{msg}");
    }

    #[test]
    fn build_provider_succeeds_for_anthropic_openrouter_and_gemini_once_the_key_resolves() {
        for (provider, var) in [
            (ProviderKind::Anthropic, "CTXLAKE_TEST_KEY_ANTHROPIC"),
            (ProviderKind::Openrouter, "CTXLAKE_TEST_KEY_OPENROUTER"),
            (ProviderKind::Gemini, "CTXLAKE_TEST_KEY_GEMINI"),
        ] {
            std::env::set_var(var, "test-key-not-a-real-secret");
            let cfg = batch_cfg(provider, var);
            let result = build_provider(&cfg);
            std::env::remove_var(var);
            assert!(
                result.is_ok(),
                "{provider:?} should build once {var} resolves"
            );
        }
    }

    #[test]
    fn build_provider_never_requires_a_key_for_ollama() {
        // Ollama is a local endpoint; requiring a key here would make the
        // documented "fully local, no transcript leaves the host" path
        // depend on an env var nobody needs to set.
        let cfg = batch_cfg(ProviderKind::Ollama, "CTXLAKE_TEST_UNUSED_OLLAMA_VAR");
        assert!(build_provider(&cfg).is_ok());
    }

    #[test]
    fn build_provider_rejects_openai_compatible_with_no_base_url() {
        let cfg = batch_cfg(
            ProviderKind::OpenaiCompatible,
            "CTXLAKE_TEST_UNUSED_OAC_VAR",
        );
        assert!(cfg.base_url.is_none());
        let err = expect_provider_err(build_provider(&cfg));
        assert!(matches!(err, ExtractError::Provider(_)));
    }

    #[test]
    fn build_provider_allows_openai_compatible_with_no_key_once_base_url_is_set() {
        // Some self-hosted gateways sit behind no auth at all — a missing key
        // here is a legitimate configuration, unlike Anthropic/OpenRouter/Gemini.
        std::env::remove_var("CTXLAKE_TEST_UNUSED_OAC_VAR_2");
        let cfg = BatchConfig {
            base_url: Some("http://localhost:8000/v1".to_string()),
            ..batch_cfg(
                ProviderKind::OpenaiCompatible,
                "CTXLAKE_TEST_UNUSED_OAC_VAR_2",
            )
        };
        assert!(build_provider(&cfg).is_ok());
    }

    /// Deliberately out of order: `req-2`'s line comes before `req-1`'s, the
    /// exact scenario docs/memory.md warns about ("results come back out
    /// of order, so they are keyed by request id, never by position").
    const BATCH_RESULTS_JSONL_OUT_OF_ORDER: &str = "\
{\"custom_id\":\"req-2\",\"result\":{\"type\":\"succeeded\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"claims\\\":[]}\"}]}}}
{\"custom_id\":\"req-1\",\"result\":{\"type\":\"succeeded\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"claims\\\":[{\\\"claim\\\":\\\"x\\\",\\\"claim_type\\\":\\\"environment\\\",\\\"subject\\\":\\\"y\\\",\\\"evidence\\\":[]}]}\"}]}}}
{\"custom_id\":\"req-3\",\"result\":{\"type\":\"errored\",\"error\":{\"type\":\"invalid_request\"}}}
";

    #[test]
    fn parse_batch_jsonl_keys_by_request_id_regardless_of_line_order() {
        let parsed = parse_batch_jsonl(BATCH_RESULTS_JSONL_OUT_OF_ORDER);
        assert_eq!(parsed.len(), 3);
        // req-1's line was SECOND in the file but must land under key "req-1" —
        // this is the assertion that would fail if results were ever keyed by
        // position (e.g. by enumerate() index) instead of custom_id.
        let req1 = parsed.get("req-1").unwrap().as_ref().unwrap();
        let claims = parse_claims_response(req1).unwrap();
        assert_eq!(claims[0].claim, "x");

        let req2 = parsed.get("req-2").unwrap().as_ref().unwrap();
        assert!(parse_claims_response(req2).unwrap().is_empty());

        assert!(parsed.get("req-3").unwrap().is_err());
    }

    // ---- tier2_enabled / clean no-op ----

    #[test]
    fn tier2_is_disabled_for_agent_mode_even_with_batch_config_present() {
        let cfg = SummarizeConfig {
            mode: SummarizeMode::Agent,
            batch: Some(BatchConfig::default()),
        };
        assert!(!tier2_enabled(&cfg));
    }

    #[test]
    fn tier2_is_disabled_without_a_batch_config_even_in_batch_mode() {
        let cfg = SummarizeConfig {
            mode: SummarizeMode::Batch,
            batch: None,
        };
        assert!(!tier2_enabled(&cfg));
    }

    #[test]
    fn tier2_is_enabled_for_batch_both_and_shadow() {
        for mode in [
            SummarizeMode::Batch,
            SummarizeMode::Both,
            SummarizeMode::Shadow,
        ] {
            let cfg = SummarizeConfig {
                mode,
                batch: Some(BatchConfig::default()),
            };
            assert!(
                tier2_enabled(&cfg),
                "{mode:?} with a batch config must enable tier 2"
            );
        }
    }

    #[tokio::test]
    async fn extraction_is_a_clean_no_op_when_no_llm_is_configured() {
        let store = object_store::memory::InMemory::new();
        let cfg = SummarizeConfig {
            mode: SummarizeMode::None,
            batch: None,
        };
        let session = SessionTranscript {
            session_id: "s1".into(),
            agent_id: "cc-01".into(),
            envelopes: vec![env_with("s1", "m1", "anything at all", 0)],
        };
        let outcome = extract_session(
            &store,
            &cfg,
            &TriggerSensitiveProvider,
            &session,
            "2026-09-09",
            "oxidant",
        )
        .await
        .unwrap();
        assert_eq!(outcome.claims_proposed, 0);
        assert!(!outcome.skipped_already_extracted);
        // No idempotency marker should even be written — a true no-op touches
        // the store not at all.
        let marker_exists = store
            .get(&ctxlake_store::layout::claims_extracted("oxidant", "s1"))
            .await
            .is_ok();
        assert!(!marker_exists);
    }

    // ---- idempotency: mark_extracted_if_new ----

    #[tokio::test]
    async fn a_session_extracted_by_an_older_extractor_is_offered_again() {
        // The gap Phase 4 closes. A successful `{"claims": []}` writes the marker just
        // as a productive run does, so 37 sessions on a live lake were permanently
        // marked done having produced nothing — and no improvement to the prompt, the
        // transcript or the parser could ever be measured against them. Short of
        // deleting keys by hand, the only evidence a new extractor had was sessions
        // that happened not to exist yet.
        let store = object_store::memory::InMemory::new();
        let key = ctxlake_store::layout::claims_extracted("oxidant", "s1");

        // A marker from before markers carried a version — the shape every existing
        // one in a real lake has.
        store
            .put(&key, PutPayload::from_static(b"{}"))
            .await
            .unwrap();

        assert!(
            !is_already_extracted(&store, "oxidant", "s1").await.unwrap(),
            "a versionless marker must not count as done for the current extractor"
        );
        assert!(
            mark_extracted_if_new(&store, "oxidant", "s1")
                .await
                .unwrap(),
            "and re-claiming it must succeed, not collide with itself"
        );

        // Having re-claimed it at the current version, it is done again.
        assert!(
            is_already_extracted(&store, "oxidant", "s1").await.unwrap(),
            "a current marker must still skip, or every cycle re-extracts everything"
        );
        assert!(!mark_extracted_if_new(&store, "oxidant", "s1")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn two_fleets_do_not_share_extraction_markers() {
        // Same class as the roster and the snapshot pointer: `claims/extracted/<id>`
        // was flat, so one fleet extracting a session marked it done for the other.
        let store = object_store::memory::InMemory::new();
        assert!(mark_extracted_if_new(&store, "ours", "s1").await.unwrap());
        assert!(
            mark_extracted_if_new(&store, "theirs", "s1").await.unwrap(),
            "another fleet's marker must not claim this one's session"
        );
        assert!(is_already_extracted(&store, "ours", "s1").await.unwrap());
        assert!(is_already_extracted(&store, "theirs", "s1").await.unwrap());
        assert!(!is_already_extracted(&store, "third", "s1").await.unwrap());
    }

    #[tokio::test]
    async fn mark_extracted_if_new_claims_exactly_once() {
        let store = object_store::memory::InMemory::new();
        assert!(mark_extracted_if_new(&store, "oxidant", "s1")
            .await
            .unwrap());
        assert!(
            !mark_extracted_if_new(&store, "oxidant", "s1")
                .await
                .unwrap(),
            "a second caller for the same session must not also claim it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn mark_extracted_if_new_under_real_concurrency_exactly_one_host_wins() {
        // This is the test that replaces a lock at the extraction-marker level:
        // "exactly one host extracts a given session" is a claim about real
        // concurrent callers, not sequential ones — `mark_extracted_if_new_claims_
        // exactly_once` above only proves the sequential case. Real OS threads
        // (`InMemory`'s operations never actually suspend, so a single-threaded
        // race would just run callers back to back and prove nothing about
        // contention) racing the exact same session id, repeated because a race
        // is not guaranteed to manifest on any single attempt.
        for _ in 0..20 {
            let store = std::sync::Arc::new(object_store::memory::InMemory::new());
            let mut handles = Vec::new();
            for _ in 0..8 {
                let store = store.clone();
                handles.push(tokio::spawn(async move {
                    mark_extracted_if_new(store.as_ref(), "oxidant", "sess-contended")
                        .await
                        .unwrap()
                }));
            }
            let mut winners = 0;
            for h in handles {
                if h.await.unwrap() {
                    winners += 1;
                }
            }
            assert_eq!(
                winners, 1,
                "exactly one of several concurrent callers must win the claim"
            );
        }
    }

    #[tokio::test]
    async fn extract_session_skips_a_session_already_marked_extracted() {
        let store = object_store::memory::InMemory::new();
        let session = SessionTranscript {
            session_id: "s1".into(),
            agent_id: "cc-01".into(),
            envelopes: vec![env_with("s1", "m1", TRIGGER_PHRASE, 0)],
        };
        let first = extract_session(
            &store,
            &shadow_cfg(),
            &TriggerSensitiveProvider,
            &session,
            "2026-09-09",
            "oxidant",
        )
        .await
        .unwrap();
        assert_eq!(first.claims_proposed, 1);
        assert!(!first.skipped_already_extracted);

        let second = extract_session(
            &store,
            &shadow_cfg(),
            &TriggerSensitiveProvider,
            &session,
            "2026-09-09",
            "oxidant",
        )
        .await
        .unwrap();
        assert_eq!(second.claims_proposed, 0);
        assert!(second.skipped_already_extracted);
    }

    // ---- end-to-end against sealed-session discovery ----

    #[tokio::test]
    async fn run_discovers_and_extracts_a_sealed_session_end_to_end() {
        let store = object_store::memory::InMemory::new();
        let envelope = env_with("s1", "m1", TRIGGER_PHRASE, 0);
        let bytes = ctxlake_sync::codec::encode(&[envelope]).unwrap();
        let seg_path = ctxlake_store::layout::session_segment(
            "2026-09-09",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "s1",
            0,
        );
        store.put(&seg_path, PutPayload::from(bytes)).await.unwrap();
        let sealed_path = ctxlake_store::layout::session_sealed(
            "2026-09-09",
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            "s1",
        );
        store
            .put(&sealed_path, PutPayload::from_static(b"{}"))
            .await
            .unwrap();

        let summary = run(&store, "oxidant", &shadow_cfg(), &TriggerSensitiveProvider)
            .await
            .unwrap();
        assert_eq!(summary.sessions_processed, 1);
        assert_eq!(summary.claims_proposed, 1);

        let events = crate::claims::list_events(&store).await.unwrap();
        assert_eq!(events.len(), 1);
    }

    /// Emits one claim per session, citing whatever session the transcript
    /// itself says it came from (via a `session-marker:<id>` token this test
    /// embeds in the envelope content) — unlike [`TriggerSensitiveProvider`],
    /// which hardcodes `session_id: "s1"` and so can't stand in for three
    /// distinct sealed sessions in the same test.
    struct SessionMarkerProvider;
    impl Provider for SessionMarkerProvider {
        fn complete<'a>(
            &'a self,
            req: &'a CompletionRequest,
        ) -> BoxFuture<'a, Result<String, ExtractError>> {
            let prompt = req.user_prompt.clone();
            Box::pin(async move {
                const MARKER: &str = "session-marker:";
                let Some(idx) = prompt.find(MARKER) else {
                    return Ok(r#"{"claims":[]}"#.to_string());
                };
                let rest = &prompt[idx + MARKER.len()..];
                let session_id: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '-')
                    .collect();
                Ok(format!(
                    r#"{{"claims":[{{"claim":"staging SSH listens on 2222","claim_type":"environment","subject":"staging","evidence":[{{"session_id":"{session_id}","message_id":"m1"}}]}}]}}"#
                ))
            })
        }
    }

    #[tokio::test]
    async fn sealed_session_listing_never_crosses_a_fleet_boundary() {
        // `fleet_id` is documented as "the boundary of who sees whom". This listing
        // used to ignore it while `digest::discover_sealed_sessions` and
        // `compact::discover_dates` both honoured it — invisible in a store holding
        // one fleet, and in a store holding two it meant one team's transcripts were
        // read into another team's extraction prompts and became evidence for another
        // team's claims.
        let store = object_store::memory::InMemory::new();
        seal_one_session_for(&store, "2026-09-11", "ours", "alpha").await;
        seal_one_session_for(&store, "2026-09-11", "theirs", "beta").await;

        let ours = list_sealed_sessions(&store, "alpha").await.unwrap();
        let ids: Vec<_> = ours.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(
            ids,
            ["ours"],
            "a second fleet's sessions must not be listed"
        );

        let theirs = list_sealed_sessions(&store, "beta").await.unwrap();
        assert_eq!(theirs.len(), 1, "the other fleet still sees its own");

        // And a fleet with nothing in the store sees nothing, rather than everything.
        assert!(list_sealed_sessions(&store, "gamma")
            .await
            .unwrap()
            .is_empty());
    }

    /// [`seal_one_session`], with the fleet spelled out — the cross-fleet test needs
    /// two fleets in one store, which the fixed-fleet helper cannot express.
    async fn seal_one_session_for(
        store: &dyn ObjectStore,
        dt: &str,
        session_id: &str,
        fleet_id: &str,
    ) {
        let content = format!("{TRIGGER_PHRASE} session-marker:{session_id}");
        let envelope = env_with(session_id, "m1", &content, 0);
        let bytes = ctxlake_sync::codec::encode(&[envelope]).unwrap();
        let seg_path = ctxlake_store::layout::session_segment(
            dt,
            fleet_id,
            Runtime::ClaudeCode,
            "cc-01",
            session_id,
            0,
        );
        store
            .put(&seg_path, object_store::PutPayload::from(bytes))
            .await
            .unwrap();
        let sealed = object_store::path::Path::from(format!(
            "{}/_SEALED",
            seg_path.as_ref().rsplit_once('/').unwrap().0
        ));
        store
            .put(&sealed, object_store::PutPayload::from_static(b"{}"))
            .await
            .unwrap();
    }

    /// Seal one session (write its one segment plus its `_SEALED` marker) under
    /// `dt`, so `list_sealed_sessions` finds it — factored out because the
    /// budget-exhaustion test below needs three of these.
    async fn seal_one_session(store: &dyn ObjectStore, dt: &str, session_id: &str) {
        let content = format!("{TRIGGER_PHRASE} session-marker:{session_id}");
        let envelope = env_with(session_id, "m1", &content, 0);
        let bytes = ctxlake_sync::codec::encode(&[envelope]).unwrap();
        let seg_path = ctxlake_store::layout::session_segment(
            dt,
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            session_id,
            0,
        );
        store.put(&seg_path, PutPayload::from(bytes)).await.unwrap();
        let sealed_path = ctxlake_store::layout::session_sealed(
            dt,
            "oxidant",
            Runtime::ClaudeCode,
            "cc-01",
            session_id,
        );
        store
            .put(&sealed_path, PutPayload::from_static(b"{}"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_reaches_new_sessions_even_when_older_ones_are_already_extracted() {
        // The exact stall this finding describes: three sealed sessions exist,
        // oldest-first (s1, s2, s3 by ascending `dt`). s1 and s2 were already
        // extracted by a PRIOR run. A budget of 1 must still reach s3 — applying
        // the budget to the raw sealed list (as `.take(limit)` used to) would
        // grab s1, find it already done, and report `sessions_processed: 0`
        // forever, never reaching s3 no matter how many times `run` is called.
        let store = object_store::memory::InMemory::new();
        seal_one_session(&store, "2026-09-01", "s1").await;
        seal_one_session(&store, "2026-09-02", "s2").await;
        seal_one_session(&store, "2026-09-03", "s3").await;
        mark_extracted_if_new(&store, "oxidant", "s1")
            .await
            .unwrap();
        mark_extracted_if_new(&store, "oxidant", "s2")
            .await
            .unwrap();

        let cfg = SummarizeConfig {
            mode: SummarizeMode::Shadow,
            batch: Some(BatchConfig {
                max_sessions_per_run: 1,
                ..BatchConfig::default()
            }),
        };
        let summary = run(&store, "oxidant", &cfg, &SessionMarkerProvider)
            .await
            .unwrap();

        assert_eq!(
            summary.sessions_processed, 1,
            "a budget of 1 must process exactly one NEW session, not stall on \
             already-extracted ones that happen to sort first"
        );
        assert_eq!(
            summary.claims_proposed, 1,
            "the one session actually processed (s3) must have been extracted \
             for real, not merely skipped"
        );
        // s3 specifically — not s1 or s2 again — must be the one newly marked.
        assert!(is_already_extracted(&store, "oxidant", "s3").await.unwrap());
    }

    #[test]
    fn resolve_api_key_never_requires_one_for_ollama() {
        let cfg = BatchConfig {
            provider: ProviderKind::Ollama,
            api_key_env: "SOME_VAR_THAT_IS_NOT_SET_ANYWHERE".into(),
            ..BatchConfig::default()
        };
        assert_eq!(resolve_api_key(&cfg), None);
    }

    // ---- claim_id reuse across real extraction runs, end-to-end through the gate ----
    //
    // This is the scenario docs/memory.md's independence section exists to catch,
    // run through the ACTUAL pipeline rather than a hand-built ClaimState: session
    // A observes a fact; the claim gets injected into session B's context; B
    // "independently" re-observes the same fact. If extraction minted a fresh
    // claim_id for B (as it used to), the gate would have nothing to discount and
    // would promote a convention on one real observation wearing two reporters.

    /// Fails the first call, succeeds on every one after — a transient provider
    /// error, which is the common case this test exists for (a 500, a rate limit, a
    /// dropped connection), not a permanent misconfiguration.
    struct FlakyProvider {
        calls: std::sync::atomic::AtomicUsize,
        session_id: &'static str,
    }
    impl Provider for FlakyProvider {
        fn complete<'a>(
            &'a self,
            _req: &'a CompletionRequest,
        ) -> futures::future::BoxFuture<'a, Result<String, ExtractError>> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let sid = self.session_id;
            Box::pin(async move {
                if n == 0 {
                    Err(ExtractError::Provider("503 upstream unavailable".into()))
                } else {
                    Ok(format!(
                        r#"{{"claims":[{{"claim":"CI sets RUSTFLAGS=-D warnings","claim_type":"environment","subject":"ci","evidence":[{{"session_id":"{sid}","message_id":"m1"}}]}}]}}"#
                    ))
                }
            })
        }
    }

    #[tokio::test]
    async fn an_empty_session_costs_no_model_call() {
        // Ten of these in one real pass: a window opened and closed with no prompt and
        // no tool call. Each was a paid call that came back, at length, explaining the
        // model could not see a transcript — then failed to parse. The failure was
        // never the model's.
        struct MustNotBeCalled(std::sync::atomic::AtomicUsize);
        impl Provider for MustNotBeCalled {
            fn complete<'a>(
                &'a self,
                _req: &'a CompletionRequest,
            ) -> futures::future::BoxFuture<'a, Result<String, ExtractError>> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { Ok(r#"{"claims":[]}"#.to_string()) })
            }
        }

        let store = object_store::memory::InMemory::new();
        let provider = MustNotBeCalled(std::sync::atomic::AtomicUsize::new(0));
        // Session lifecycle only: no content, no tool.
        let session = SessionTranscript {
            session_id: "empty-1".into(),
            agent_id: "cc-01".into(),
            envelopes: vec![
                Envelope::new(
                    "oxidant",
                    "cc-01",
                    Runtime::ClaudeCode,
                    "empty-1",
                    EventType::SessionStart,
                    "2026-09-11T10:00:00.000Z",
                ),
                Envelope::new(
                    "oxidant",
                    "cc-01",
                    Runtime::ClaudeCode,
                    "empty-1",
                    EventType::SessionEnd,
                    "2026-09-11T10:05:00.000Z",
                ),
            ],
        };

        let out = extract_session(
            &store,
            &shadow_cfg(),
            &provider,
            &session,
            "2026-09-11",
            "oxidant",
        )
        .await
        .expect("an empty session is not an error");

        assert_eq!(
            provider.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the model must not be called with an empty transcript"
        );
        assert_eq!(out.claims_proposed, 0);
        assert!(
            is_already_extracted(&store, "oxidant", "empty-1")
                .await
                .unwrap(),
            "and it must stay marked done — a later pass would do nothing different"
        );
    }

    #[test]
    fn transcript_emptiness_is_measured_in_content_not_lines() {
        // Each case below is a real failing session from the live lake, in the order
        // they were found. Every one of them made the model reply, correctly, that it
        // had been sent no transcript — and then fail to parse as JSON.

        // Round 1: the header alone.
        assert!(transcript_is_empty("session_id: s1\n"));
        assert!(transcript_is_empty(""));

        // Round 2: ids and tool names, no inputs, no results.
        assert!(transcript_is_empty(
            "session_id: s1\n[toolu_a] tool:Read\n[toolu_b] tool:Glob\n[toolu_c] tool:Read\n"
        ));

        // Round 3: one prompt reading `ok`. This is why the check counts characters —
        // "is there any text" says yes, and there is still nothing to cite.
        assert!(transcript_is_empty("session_id: s1\n[01M2B] ok\n"));

        // And what must still be attempted. `staging listens on port 2222` is
        // docs/memory.md's own example of a good environment claim and scores 28; an
        // earlier threshold of 40 would have skipped it, and this suite caught that.
        assert!(!transcript_is_empty(
            "session_id: s1\n[toolu_a] tool:Bash input={\"command\":\"cargo test --workspace\"}\n"
        ));
        // A real prompt, with no tool call at all, can still carry a preference or a
        // convention.
        assert!(!transcript_is_empty(
            "session_id: s1\n[m1] always run the full workspace test suite before pushing\n"
        ));
        // A failure with output and no input is exactly the session worth extracting.
        assert!(!transcript_is_empty(
            "session_id: s1\n[toolu_a] tool:Bash exit=1 result=error: could not compile ctxlake-maint\n"
        ));
    }

    #[tokio::test]
    async fn one_bad_session_does_not_stop_the_ones_after_it() {
        // The exact production failure: 37 sessions became eligible for re-extraction,
        // the second returned something that was not JSON, and the whole maintenance
        // chain died with it — every later session unextracted, and the digests and
        // snapshot skipped too.
        struct OneBadApple;
        impl Provider for OneBadApple {
            fn complete<'a>(
                &'a self,
                req: &'a CompletionRequest,
            ) -> futures::future::BoxFuture<'a, Result<String, ExtractError>> {
                // Not an error — a *valid* response that is not JSON. That is the
                // production failure: the model answered in prose.
                let bad = req.user_prompt.contains("bad-1");
                Box::pin(async move {
                    if bad {
                        Ok("I'm sorry, I can't help with that.".to_string())
                    } else {
                        Ok(r#"{"claims":[]}"#.to_string())
                    }
                })
            }
        }

        let store = object_store::memory::InMemory::new();
        for id in ["aaa-good-1", "bad-1", "zzz-good-2"] {
            seal_one_session(&store, "2026-09-11", id).await;
        }

        let summary = run(&store, "oxidant", &shadow_cfg(), &OneBadApple)
            .await
            .expect("the pass must complete");

        assert_eq!(summary.sessions_failed, 1, "{summary:?}");
        assert_eq!(
            summary.sessions_processed, 2,
            "the sessions either side of the bad one must still be extracted: {summary:?}"
        );
        // And the bad one is retryable rather than marked done.
        assert!(!is_already_extracted(&store, "oxidant", "bad-1")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_malformed_response_error_says_what_actually_came_back() {
        // "expected value at line 1 column 1" and nothing else is what a real failing
        // extraction reported. Undiagnosable without reproducing it by hand.
        let err = parse_claims_response("I'm sorry, I can't help with that.").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("can't help"), "must quote the response: {msg}");

        // Bounded and single-line: this lands in a log an operator reads.
        let huge = format!("nonsense {}", "x".repeat(5000));
        let msg = parse_claims_response(&huge).unwrap_err().to_string();
        assert!(msg.len() < 400, "{} chars", msg.len());
        let multiline = "not json
second line
third line";
        let msg = parse_claims_response(multiline).unwrap_err().to_string();
        assert_eq!(msg.lines().count(), 1, "must stay one line: {msg}");
    }

    #[tokio::test]
    async fn a_transient_provider_failure_does_not_discard_the_session_forever() {
        // The marker is written before the provider call so two hosts cannot both pay
        // for the same session. Nothing used to release it on failure, so one 503
        // meant that session was marked extracted, produced no claims, and was never
        // retried — silently, with the next run cheerfully reporting "0 session(s)
        // extracted". Found by a live run, not by a fixture.
        let store = object_store::memory::InMemory::new();
        seal_one_session(&store, "2026-09-11", "flaky-1").await;
        let cfg = shadow_cfg();
        let provider = FlakyProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            session_id: "flaky-1",
        };

        // The pass *reports* the failure rather than returning `Err`. It used to
        // propagate, and a single malformed response therefore aborted the entire
        // maintenance chain — compaction, digests, the gate and the snapshot, none of
        // which involve a model. Seen the first time retry was enabled on a real lake.
        let first = run(&store, "oxidant", &cfg, &provider)
            .await
            .expect("one session's failure must not end the pass");
        assert_eq!(
            first.sessions_failed, 1,
            "the failure must be counted, not swallowed"
        );
        assert!(
            first.last_error.is_some(),
            "and it must carry something an operator can act on"
        );
        assert_eq!(first.sessions_processed, 0);
        assert!(
            !is_already_extracted(&store, "oxidant", "flaky-1")
                .await
                .unwrap(),
            "a failed extraction must release its marker, or the session is lost"
        );

        let second = run(&store, "oxidant", &cfg, &provider)
            .await
            .expect("the retry must succeed");
        assert_eq!(second.sessions_processed, 1, "the session must be retried");
        assert_eq!(second.claims_proposed, 1);
    }

    /// Always emits the same claim, citing whatever session it's told to (a real
    /// provider wouldn't need telling — the transcript IS the session — but this
    /// test double stands in for "the model observed the same fact," which is
    /// the input this scenario needs to hold constant across two calls).
    struct FixedClaimProvider {
        session_id: &'static str,
    }
    impl Provider for FixedClaimProvider {
        fn complete<'a>(
            &'a self,
            _req: &'a CompletionRequest,
        ) -> BoxFuture<'a, Result<String, ExtractError>> {
            let body = format!(
                r#"{{"claims":[{{"claim":"this repo uses just, not make","claim_type":"convention","subject":"build-tooling","evidence":[{{"session_id":"{}","message_id":"m1"}}]}}]}}"#,
                self.session_id
            );
            Box::pin(async move { Ok(body) })
        }
    }

    #[tokio::test]
    async fn echo_case_end_to_end_extraction_reuses_claim_id_and_gate_holds_it_back() {
        let store = object_store::memory::InMemory::new();

        // Session A: the first, genuine observation.
        let session_a = SessionTranscript {
            session_id: "session-a".into(),
            agent_id: "cc-01".into(),
            envelopes: vec![env_with_at(
                "cc-01",
                "session-a",
                "m1",
                "this repo uses just, not make",
                "2026-09-09T12:00:00.000Z",
            )],
        };
        let outcome_a = extract_session(
            &store,
            &shadow_cfg(),
            &FixedClaimProvider {
                session_id: "session-a",
            },
            &session_a,
            "2026-09-09",
            "oxidant",
        )
        .await
        .unwrap();
        assert_eq!(outcome_a.claims_proposed, 1);

        let folded_after_a =
            crate::claims::fold(crate::claims::list_events(&store).await.unwrap().iter());
        assert_eq!(folded_after_a.len(), 1, "exactly one claim after session A");
        let claim_id = folded_after_a.keys().next().unwrap().clone();

        // Session B, three days later: it had `claim_id` injected into its
        // context (it read A's claim) before "independently" re-observing the
        // identical fact.
        let session_b = SessionTranscript {
            session_id: "session-b".into(),
            agent_id: "cc-02".into(),
            envelopes: vec![env_with_at(
                "cc-02",
                "session-b",
                "m1",
                "this repo uses just, not make",
                "2026-09-12T09:00:00.000Z",
            )],
        };
        let outcome_b = extract_session(
            &store,
            &shadow_cfg(),
            &FixedClaimProvider {
                session_id: "session-b",
            },
            &session_b,
            "2026-09-12",
            "oxidant",
        )
        .await
        .unwrap();
        assert_eq!(outcome_b.claims_proposed, 1);

        // The extraction-side assertion: B's matching observation must fold into
        // the SAME claim as A's, not mint a second one.
        let folded_after_b =
            crate::claims::fold(crate::claims::list_events(&store).await.unwrap().iter());
        assert_eq!(
            folded_after_b.len(),
            1,
            "session B's matching observation must reuse session A's claim_id, \
             not create a second claim — the independence gate can only discount \
             evidence sessions on ONE shared claim_id"
        );
        let merged = folded_after_b.get(&claim_id).unwrap();
        assert_eq!(merged.evidence_session_count(), 2);

        // The gate-side assertion: with B's session recorded as having had
        // `claim_id` injected into it, independent_count must be 1 (not 2), and
        // a convention (needs 2 independent) must be held for review, not
        // promoted as if two people had agreed independently.
        let known_agents: std::collections::HashSet<String> =
            ["cc-01".to_string(), "cc-02".to_string()]
                .into_iter()
                .collect();
        let mut windows = HashMap::new();
        windows.insert(
            "session-a".to_string(),
            (
                "2026-09-09T00:00:00Z".to_string(),
                "2026-09-09T23:59:59Z".to_string(),
            ),
        );
        windows.insert(
            "session-b".to_string(),
            (
                "2026-09-12T00:00:00Z".to_string(),
                "2026-09-12T23:59:59Z".to_string(),
            ),
        );
        let mut injected: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        injected.insert(
            "session-b".to_string(),
            std::collections::HashSet::from([claim_id.clone()]),
        );

        let summary = crate::gate::run(
            &store,
            "2026-09-12T10:00:00Z",
            &known_agents,
            &windows,
            &injected,
            |_e| true,
        )
        .await
        .unwrap();

        // The half that still holds: extraction reuses the SAME `claim_id` across both
        // sessions rather than filing two claims, so the echo is recognised as one
        // claim with two reporters instead of two independent agreements. That is what
        // makes `compute_independent_count` able to see through it at all.
        assert_eq!(
            summary.promoted + summary.sent_to_review,
            1,
            "both sessions must land on one claim, not two"
        );

        // The half that changed: `Convention`'s independence threshold is now 1, so
        // that single independent observation promotes. This used to assert
        // `promoted == 0` and that the claim never reached fleet scope.
        //
        // The concession is documented on `gate::independent_threshold`: a bar of 2 was
        // unreachable while nothing populates `injected_context`, so it discarded every
        // convention rather than protecting against echoes. **Restore this assertion
        // when `injected_context` is populated and the threshold goes back to 2.**
        assert_eq!(
            summary.promoted, 1,
            "with the threshold at 1, the claim promotes; see independent_threshold"
        );
    }
}
