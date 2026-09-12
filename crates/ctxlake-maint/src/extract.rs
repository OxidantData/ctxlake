//! Tier 2 — batch extraction over sealed sessions. See `docs/summarization.md`.
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

/// `docs/summarization.md`'s `[summarize] mode` values. `Shadow` is not listed
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
}

/// `[summarize.batch]`, mirroring `docs/summarization.md`'s table field-for-field,
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
/// configured, extraction is a clean no-op" (docs/summarization.md) — this
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
    for e in envelopes {
        let Some(content) = &e.content else { continue };
        let cleaned = strip_injected_context(content);
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            continue;
        }
        let id = e.message_id.as_deref().unwrap_or(&e.event_id);
        out.push_str(&format!("[{id}] {cleaned}\n"));
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
}

/// One batch's results, keyed by the request id each was submitted under — never
/// by position, since batch results come back out of order (docs/summarization.md).
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
    /// request id, **never** by position: docs/summarization.md is explicit that
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
            // (docs/summarization.md) — a plain poll loop is the right shape
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
        Box::pin(async move {
            let body = serde_json::json!({
                "model": req.model,
                "messages": [
                    {"role": "system", "content": req.system_prompt},
                    {"role": "user", "content": req.user_prompt},
                ],
            });
            let mut builder = self
                .client
                .post(format!("{}/chat/completions", self.base_url))
                .json(&body);
            if let Some(key) = &self.api_key {
                builder = builder.bearer_auth(key);
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
        })
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
/// see docs/summarization.md's "running it entirely locally."
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
/// silently stored — docs/summarization.md's "structured output" rule.
pub fn parse_claims_response(raw: &str) -> Result<Vec<RawClaim>, ExtractError> {
    let parsed: RawExtraction = serde_json::from_str(raw.trim())
        .map_err(|e| ExtractError::MalformedResponse(e.to_string()))?;
    Ok(parsed.claims)
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

pub fn build_resolvable_index(envelopes: &[Envelope]) -> ResolvableIndex {
    let mut idx = HashMap::new();
    for e in envelopes {
        if let Some(message_id) = &e.message_id {
            idx.insert(
                (e.session_id.clone(), message_id.clone()),
                ResolvedCitation {
                    excerpt_hash: e.content_hash.clone(),
                    observed_at: e.emitted_at.clone(),
                },
            );
        }
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
    let norm_subject = claims::normalize_claim_text(subject);
    let norm_claim = claims::normalize_claim_text(claim_text);
    existing
        .values()
        .find(|s| {
            s.status != crate::claims::ClaimStatus::Retired
                && s.claim_type == claim_type
                && claims::normalize_claim_text(&s.subject) == norm_subject
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
            resolvable
                .get(&(c.session_id.clone(), c.message_id.clone()))
                .map(|resolved| Evidence {
                    session_id: c.session_id.clone(),
                    message_id: c.message_id.clone(),
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
/// run, or a racing holder on another host) already has.
///
/// This is a genuine create-if-absent lock, unlike a lease (AGENTS.md invariant
/// 4 is about leases specifically, which already exist and only ever move
/// between free/held via CAS `Update`). `claims/extracted/<id>` never exists
/// before the first extraction and never needs a second state, so `Create` is
/// the correct primitive here — with the same MinIO caveat as everywhere else in
/// this codebase that reaches for it (minio/minio#20346): on MinIO this call
/// fails outright rather than succeeding-or-losing-a-race, so a MinIO-backed
/// fleet will re-attempt extraction on every run until that is worked around.
/// `ctxlake doctor`'s put-if-absent probe is what surfaces this ahead of time.
pub async fn mark_extracted_if_new(
    store: &dyn ObjectStore,
    session_id: &str,
) -> Result<bool, StoreError> {
    let path = ctxlake_store::layout::claims_extracted(session_id);
    match store
        .put_opts(
            &path,
            PutPayload::from_static(b"{}"),
            PutMode::Create.into(),
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(OsError::AlreadyExists { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
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
    session_id: &str,
) -> Result<bool, StoreError> {
    let path = ctxlake_store::layout::claims_extracted(session_id);
    match store.get(&path).await {
        Ok(_) => Ok(true),
        Err(OsError::NotFound { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Find every sealed session under `sessions/` by locating `_SEALED` markers.
/// Whether a given one has already been extracted is [`mark_extracted_if_new`]'s
/// job, not this listing's — keeping "what exists" and "what's claimed" as
/// separate questions avoids a stale listing racing a marker that landed a
/// moment ago.
pub async fn list_sealed_sessions(store: &dyn ObjectStore) -> Result<Vec<SessionRef>, StoreError> {
    use futures::StreamExt;
    let prefix = object_store::path::Path::from("sessions");
    let mut out = Vec::new();
    let mut stream = store.list(Some(&prefix));
    while let Some(meta) = stream.next().await {
        let Ok(meta) = meta else { continue };
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

/// The extraction system prompt: the stable prefix docs/summarization.md's
/// "prompt caching" paragraph describes — byte-identical across every session,
/// so a caching-aware provider only pays for it once.
pub const EXTRACTION_SYSTEM_PROMPT: &str = r#"You extract atomic, evidence-backed claims from a coding-agent session transcript.
Respond with ONLY a JSON object of the shape:
{"claims": [{"claim": string, "claim_type": "environment"|"convention"|"outcome"|"preference"|"hypothesis", "subject": string, "evidence": [{"session_id": string, "message_id": string}]}]}
Every claim MUST cite at least one (session_id, message_id) pair that appears in the transcript's own [message_id] markers. Never invent a citation. If nothing in the transcript supports a durable claim, return {"claims": []}."#;

#[derive(Debug, Clone, Default)]
pub struct ExtractOutcome {
    pub session_id: String,
    pub claims_proposed: usize,
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
) -> Result<ExtractOutcome, ExtractError> {
    if !tier2_enabled(cfg) {
        return Ok(ExtractOutcome {
            session_id: session.session_id.clone(),
            ..Default::default()
        });
    }
    if !mark_extracted_if_new(store, &session.session_id).await? {
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
    let request = CompletionRequest {
        system_prompt: EXTRACTION_SYSTEM_PROMPT.to_string(),
        user_prompt: transcript_text,
        model: batch_cfg.model.clone(),
    };
    let raw_response = provider.complete(&request).await?;
    let raw_claims = parse_claims_response(&raw_response)?;
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
    let existing_events = crate::claims::list_events(store).await?;
    let existing = crate::claims::fold(existing_events.iter());

    let mut proposed = 0usize;
    for raw in raw_claims {
        if let Some(claim) =
            claim_from_raw(raw, &session.agent_id, &observed_at, &resolvable, &existing)
        {
            crate::claims::append_proposed(store, date, &claim).await?;
            proposed += 1;
        }
    }
    Ok(ExtractOutcome {
        session_id: session.session_id.clone(),
        claims_proposed: proposed,
        skipped_already_extracted: false,
    })
}

#[derive(Debug, Default)]
pub struct ExtractRunSummary {
    pub sessions_processed: usize,
    pub claims_proposed: usize,
}

/// The full Tier 2 pass: find sealed, not-yet-extracted sessions and extract
/// each, up to `max_sessions_per_run`. A clean no-op when [`tier2_enabled`] is
/// false — no listing, no HTTP client, nothing.
pub async fn run(
    store: &dyn ObjectStore,
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
    let sealed = list_sealed_sessions(store).await?;
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
        if is_already_extracted(store, &session_ref.session_id).await? {
            continue;
        }
        let transcript = load_transcript(store, &session_ref).await?;
        let date = transcript
            .envelopes
            .first()
            .and_then(|e| e.emitted_at.get(0..10))
            .unwrap_or("1970-01-01")
            .to_string();
        let outcome = extract_session(store, cfg, provider, &transcript, &date).await?;
        if !outcome.skipped_already_extracted {
            summary.sessions_processed += 1;
        }
        summary.claims_proposed += outcome.claims_proposed;
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
        )
        .await
        .unwrap();
        assert_eq!(outcome.claims_proposed, 1);
    }

    // ---- no evidence, no claim ----

    fn no_existing_claims() -> BTreeMap<String, ClaimState> {
        BTreeMap::new()
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

    #[test]
    fn parse_claims_response_rejects_malformed_json_as_a_parse_error() {
        let err = parse_claims_response("not json at all").unwrap_err();
        assert!(matches!(err, ExtractError::MalformedResponse(_)));
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

    /// Deliberately out of order: `req-2`'s line comes before `req-1`'s, the
    /// exact scenario docs/summarization.md warns about ("results come back out
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
        )
        .await
        .unwrap();
        assert_eq!(outcome.claims_proposed, 0);
        assert!(!outcome.skipped_already_extracted);
        // No idempotency marker should even be written — a true no-op touches
        // the store not at all.
        let marker_exists = store
            .get(&ctxlake_store::layout::claims_extracted("s1"))
            .await
            .is_ok();
        assert!(!marker_exists);
    }

    // ---- idempotency: mark_extracted_if_new ----

    #[tokio::test]
    async fn mark_extracted_if_new_claims_exactly_once() {
        let store = object_store::memory::InMemory::new();
        assert!(mark_extracted_if_new(&store, "s1").await.unwrap());
        assert!(
            !mark_extracted_if_new(&store, "s1").await.unwrap(),
            "a second caller for the same session must not also claim it"
        );
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

        let summary = run(&store, &shadow_cfg(), &TriggerSensitiveProvider)
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
        mark_extracted_if_new(&store, "s1").await.unwrap();
        mark_extracted_if_new(&store, "s2").await.unwrap();

        let cfg = SummarizeConfig {
            mode: SummarizeMode::Shadow,
            batch: Some(BatchConfig {
                max_sessions_per_run: 1,
                ..BatchConfig::default()
            }),
        };
        let summary = run(&store, &cfg, &SessionMarkerProvider).await.unwrap();

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
        assert!(is_already_extracted(&store, "s3").await.unwrap());
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

        let lease_key = ctxlake_store::layout::lease_maintenance();
        ctxlake_store::lease::provision(&store, &lease_key)
            .await
            .unwrap();
        let lease = match ctxlake_store::lease::acquire(
            &store,
            &ctxlake_store::clock::SystemClock,
            &lease_key,
            "test-maintenance-runner",
            None,
            std::time::Duration::from_secs(300),
        )
        .await
        .unwrap()
        {
            ctxlake_store::lease::AcquireOutcome::Acquired(h) => h,
            ctxlake_store::lease::AcquireOutcome::NotAcquired { .. } => unreachable!(),
        };

        let summary = crate::gate::run(
            &store,
            &lease,
            "2026-09-12T10:00:00Z",
            &known_agents,
            &windows,
            &injected,
            |_e| true,
        )
        .await
        .unwrap();

        assert_eq!(
            summary.promoted, 0,
            "one observation wearing two reporters must not promote a convention \
             (which requires 2 INDEPENDENT sessions)"
        );
        assert_eq!(summary.sent_to_review, 1);
        assert!(
            crate::claims::list_fleet_claims(&store)
                .await
                .unwrap()
                .is_empty(),
            "the echoed claim must never reach fleet scope"
        );
    }
}
