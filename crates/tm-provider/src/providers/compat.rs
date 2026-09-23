//! The shared OpenAI-compatible core: wire shapes, translation, HTTP, retry and error mapping
//! for every backend in this crate that speaks (a close dialect of) the OpenAI Chat Completions
//! API. Eight of the eleven provider modules build on this file: [`crate::providers::openai`],
//! [`crate::providers::openrouter`], [`crate::providers::fast`], [`crate::providers::frontier`],
//! [`crate::providers::serverless`], [`crate::providers::local`], [`crate::providers::cloudflare`]
//! and this module's own [`DevPassProvider`] (the eleventh group, "DevPass + generic
//! OpenAI-compatible", is implemented directly here rather than in a separate stub, since it *is*
//! this generic core with nothing backend-specific added).
//!
//! Unlike [`crate::anthropic::AnthropicProvider`] this module is a reusable building block, not a
//! single provider: construct a [`CompatConfig`] describing one backend's base URL, auth header
//! style and path layout, hand it to [`CompatProvider::new`], and the result is a complete
//! [`crate::fabric::Provider`] implementation. A stub module wanting backend-specific behavior
//! (a different default model, a friendlier `from_env`, a non-standard finish reason) wraps a
//! [`CompatProvider`] as a struct field and delegates, rather than reimplementing any of this.
//!
//! ## Streaming
//!
//! [`crate::types::CompletionRequest::stream`] does not change [`crate::fabric::Provider`]'s
//! signature — `complete` still returns one [`crate::types::Completion`], not a stream of deltas,
//! because that is the contract [`crate::fabric::Fabric::execute`] already commits to. Setting
//! `stream: true` here instead asks the upstream API for Server-Sent Events and reassembles them
//! into that same single [`crate::types::Completion`] shape ([`parse_sse_body`] +
//! [`assemble_streamed_completion`]), rather than exposing partial deltas to the caller. This is
//! useful today mainly because some providers behave more reliably (or price differently) on
//! their streaming path even when the caller only wants the final answer; a true incremental
//! streaming surface would require widening the `Provider` trait itself, which is out of scope
//! for this crate's provider modules (`fabric.rs` owns that trait).
//!
//! ## Testing
//!
//! Every wire-shape and translation function here is pure and unit-tested against recorded JSON.
//! Nothing in `#[cfg(test)]` constructs an HTTP client or names a real host, per the workspace
//! rule that no test performs a network call.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::{missing_env_var, Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Candidate, Completion, CompletionRequest, ContentBlock, EmbedRequest, Embeddings, Message,
    MessageRole, ModelId, ProviderError, StopReason, Usage,
};

/// Default request timeout for the underlying HTTP client.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Default maximum retry attempts on 429/5xx before giving up.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Floor for exponential backoff when a provider gives no `Retry-After`.
const BACKOFF_FLOOR: Duration = Duration::from_millis(500);

/// How a [`CompatProvider`] authenticates its requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStyle {
    /// `Authorization: Bearer <api_key>`, the OpenAI convention most of this fleet follows.
    Bearer,
    /// A custom header name carrying the raw key, e.g. Azure's `api-key`.
    Header(String),
    /// No auth header at all (a local backend behind no gateway, e.g. bare Ollama).
    None,
}

/// Everything one OpenAI-compatible backend needs: where it lives, how it authenticates, and
/// which paths under its base URL serve chat and embeddings.
///
/// Built via [`CompatConfig::new`] plus the `with_*` builder methods; every field is otherwise
/// public for callers that construct one directly.
#[derive(Debug, Clone, PartialEq)]
pub struct CompatConfig {
    /// The provider slug this backend registers under, matching a `providers.toml` candidate's
    /// `provider` field (e.g. `"openai"`, `"groq"`).
    pub id: String,
    /// The base URL, with no trailing slash (e.g. `"https://api.openai.com/v1"`).
    pub base_url: String,
    /// The model id sent on every request.
    pub model: String,
    /// The API key, if this backend needs one at all (see [`AuthStyle::None`]).
    pub api_key: Option<String>,
    /// How [`CompatConfig::api_key`] is attached to requests.
    pub auth_style: AuthStyle,
    /// Extra static headers sent on every request (e.g. `OpenAI-Organization`).
    pub extra_headers: Vec<(String, String)>,
    /// Path appended to `base_url` for chat completions (default `"/chat/completions"`).
    pub chat_path: String,
    /// Path appended to `base_url` for embeddings, or `None` if this backend doesn't serve them.
    /// Defaults to `Some("/embeddings")`.
    pub embeddings_path: Option<String>,
    /// Maximum retry attempts on 429/5xx before giving up.
    pub max_retries: u32,
    /// Request timeout for the underlying HTTP client.
    pub timeout: Duration,
}

impl CompatConfig {
    /// Start a config with the OpenAI-standard defaults: Bearer auth, `/chat/completions` and
    /// `/embeddings` under `base_url`, 3 retries, a 120s timeout, no extra headers.
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        CompatConfig {
            id: id.into(),
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            auth_style: AuthStyle::Bearer,
            extra_headers: Vec::new(),
            chat_path: "/chat/completions".to_string(),
            embeddings_path: Some("/embeddings".to_string()),
            max_retries: DEFAULT_MAX_RETRIES,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Set the API key.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Override the auth style (default [`AuthStyle::Bearer`]).
    pub fn with_auth_style(mut self, style: AuthStyle) -> Self {
        self.auth_style = style;
        self
    }

    /// Add one static header sent on every request.
    pub fn with_extra_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Override the chat completions path (default `"/chat/completions"`).
    pub fn with_chat_path(mut self, path: impl Into<String>) -> Self {
        self.chat_path = path.into();
        self
    }

    /// Override the embeddings path (default `"/embeddings"`).
    pub fn with_embeddings_path(mut self, path: impl Into<String>) -> Self {
        self.embeddings_path = Some(path.into());
        self
    }

    /// Mark this backend as not serving embeddings at all; [`CompatProvider::embed`] will then
    /// always return [`ProviderError::InvalidRequest`].
    pub fn without_embeddings(mut self) -> Self {
        self.embeddings_path = None;
        self
    }

    /// Override the retry budget (default 3).
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Override the HTTP client timeout (default 120s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// A [`crate::fabric::Provider`] implementation for any OpenAI Chat-Completions-shaped backend,
/// driven entirely by a [`CompatConfig`]. See the module docs for how the eight compat-based
/// provider modules in this crate build on it.
pub struct CompatProvider {
    config: CompatConfig,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
}

impl CompatProvider {
    /// Build a provider from an explicit config.
    pub fn new(config: CompatConfig, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| ProviderError::Unavailable(format!("failed to build HTTP client: {e}")))?;
        Ok(CompatProvider {
            config,
            http,
            clock,
        })
    }

    /// Build a provider by reading `{prefix}_API_KEY` (optional — omitted entirely for
    /// [`AuthStyle::None`] backends, but this generic constructor always uses [`AuthStyle::Bearer`]
    /// when a key is present), `{prefix}_BASE_URL` (required) and `{prefix}_MODEL` (required).
    ///
    /// This is the "generic OpenAI-compatible" half of provider group 11: any backend that speaks
    /// this API can be wired up with three env vars and no code, via
    /// `CompatProvider::from_env_prefix("MY_BACKEND", "my-backend", clock)`. [`DevPassProvider`]
    /// is the one named instance of this pattern this crate ships.
    pub fn from_env_prefix(
        prefix: &str,
        id: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ProviderError> {
        let base_url_var = format!("{prefix}_BASE_URL");
        let model_var = format!("{prefix}_MODEL");
        let api_key_var = format!("{prefix}_API_KEY");

        let base_url = std::env::var(&base_url_var).map_err(|_| missing_env_var(&base_url_var))?;
        let model = std::env::var(&model_var).map_err(|_| missing_env_var(&model_var))?;
        let api_key = std::env::var(&api_key_var).ok();

        let mut config = CompatConfig::new(id, base_url, model);
        if let Some(key) = api_key {
            config = config.with_api_key(key);
        }
        CompatProvider::new(config, clock)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.base_url, path)
    }

    /// Send one JSON-bodied POST, retrying on retryable [`ProviderError`]s up to
    /// `config.max_retries` times, honoring a `Retry-After` response header. Returns the raw
    /// response body plus the measured latency and receipt timestamp on success.
    async fn send_with_retry<B: Serialize + ?Sized>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<(Vec<u8>, Duration, tm_types::Timestamp), ProviderError> {
        let headers = build_headers(&self.config)?;
        let mut attempt: u32 = 0;
        loop {
            let started = self.clock.now();
            let response = self
                .http
                .post(url)
                .headers(headers.clone())
                .json(body)
                .send()
                .await;

            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    let err = if e.is_timeout() {
                        ProviderError::Timeout(e.to_string())
                    } else {
                        ProviderError::Unavailable(e.to_string())
                    };
                    if err.is_retryable() && attempt < self.config.max_retries {
                        attempt += 1;
                        tokio::time::sleep(BACKOFF_FLOOR * attempt).await;
                        continue;
                    }
                    return Err(err);
                }
            };

            let status = response.status();
            let retry_after_header = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let body_bytes = response
                .bytes()
                .await
                .map_err(|e| {
                    ProviderError::Unavailable(format!("failed to read response body: {e}"))
                })?
                .to_vec();

            match classify_status(status, retry_after_header.as_deref(), &body_bytes) {
                Ok(()) => {
                    let finished = self.clock.now();
                    let latency =
                        Duration::from_millis(finished.millis_since(started).max(0) as u64);
                    return Ok((body_bytes, latency, finished));
                }
                Err(err) => {
                    if err.is_retryable() && attempt < self.config.max_retries {
                        attempt += 1;
                        let wait = err.retry_after().unwrap_or(BACKOFF_FLOOR * attempt);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }
}

#[async_trait]
impl Provider for CompatProvider {
    fn id(&self) -> &str {
        &self.config.id
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let wire_request = build_wire_request(&self.config.model, &req);
        let url = self.url(&self.config.chat_path);
        let (body, latency, received_at) = self.send_with_retry(&url, &wire_request).await?;

        if req.stream {
            let chunks = parse_sse_body(&body)?;
            assemble_streamed_completion(
                &chunks,
                &self.config.model,
                &self.config.id,
                latency,
                received_at,
            )
        } else {
            parse_wire_response(
                &body,
                &self.config.model,
                &self.config.id,
                latency,
                received_at,
            )
        }
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let Some(path) = self.config.embeddings_path.clone() else {
            return Err(ProviderError::InvalidRequest(format!(
                "{} does not support embeddings",
                self.config.id
            )));
        };
        let wire_request = WireEmbedRequest {
            model: self.config.model.clone(),
            input: req.inputs,
        };
        let url = self.url(&path);
        let (body, _latency, _received_at) = self.send_with_retry(&url, &wire_request).await?;
        parse_wire_embed_response(&body, &self.config.model, &self.config.id)
    }
}

/// A named OpenAI-compatible backend, "DevPass" — a thin, fully env-driven wrapper over
/// [`CompatProvider`]. It exists so provider group 11 ("DevPass + generic OpenAI-compatible") has
/// one concrete, registrable name; the generic half of that group is
/// [`CompatProvider::from_env_prefix`] itself, usable with any prefix for any other bespoke
/// OpenAI-compatible endpoint a deployment wants to point at without writing a new module.
pub struct DevPassProvider {
    inner: CompatProvider,
}

impl DevPassProvider {
    /// Env vars: `DEVPASS_API_KEY` (required), `DEVPASS_BASE_URL` (required, no trailing slash),
    /// `DEVPASS_MODEL` (required).
    pub fn from_env(clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let inner = CompatProvider::from_env_prefix("DEVPASS", "devpass", clock)?;
        Ok(DevPassProvider { inner })
    }

    /// The model DevPass should be preferred with, if and only if all three of
    /// `DEVPASS_API_KEY`, `DEVPASS_BASE_URL` and `DEVPASS_MODEL` are set to non-empty values —
    /// `None` otherwise, including when only some of the three are set.
    ///
    /// This is the single source of truth for "should DevPass be the default provider right
    /// now", used by both [`crate::role_config::RoleTable::default_table`] (to decide whether
    /// [`tm_types::Role::CoderFast`]'s primary candidate should be `devpass` instead of
    /// `anthropic`) and by `tm-cli`'s own fabric construction (to decide whether to actually
    /// build and register a [`DevPassProvider`] instead of, and not merely alongside, requiring
    /// `ANTHROPIC_API_KEY`). See `docs/providers.md`'s "DevPass" section.
    ///
    /// Deliberately stricter than [`DevPassProvider::from_env`] itself, which treats
    /// `DEVPASS_API_KEY` as optional (`.ok()`, not `?`) for a no-auth internal gateway — this
    /// gate is answering "was DevPass *deliberately and fully* configured as a default", not
    /// "would construction succeed", so a partial set (e.g. only `DEVPASS_API_KEY`) does not
    /// activate the preference.
    pub fn preferred_model() -> Option<String> {
        fn set_and_nonempty(var: &str) -> Option<String> {
            std::env::var(var).ok().filter(|v| !v.is_empty())
        }
        set_and_nonempty("DEVPASS_API_KEY")?;
        set_and_nonempty("DEVPASS_BASE_URL")?;
        set_and_nonempty("DEVPASS_MODEL")
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "devpass",
            display_name: "DevPass",
            env_vars: &[
                EnvVarRequirement {
                    name: "DEVPASS_API_KEY",
                    required: true,
                    description: "Bearer API key for the DevPass OpenAI-compatible gateway",
                },
                EnvVarRequirement {
                    name: "DEVPASS_BASE_URL",
                    required: true,
                    description: "Base URL of the DevPass endpoint, no trailing slash",
                },
                EnvVarRequirement {
                    name: "DEVPASS_MODEL",
                    required: true,
                    description: "Model id to request",
                },
            ],
            capabilities: Capabilities {
                completion: true,
                embedding: true,
                streaming: true,
                tool_use: true,
                vision: false,
            },
        }
    }
}

#[async_trait]
impl Provider for DevPassProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        self.inner.complete(req).await
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        self.inner.embed(req).await
    }
}

// ---- wire shapes: Chat Completions ----------------------------------------------------------
//
// These mirror the OpenAI Chat Completions JSON shape (snake_case, `type`-tagged tool calls) and
// exist only to (de)serialize at the HTTP boundary; `crate::types` is the provider-independent
// shape used everywhere else. None of these are recursive, so none needs `#[serde(tag = "...")]`
// in a way that would risk the recursive-internally-tagged-enum trap (see `SPEC.md`): tool
// results here flatten to a plain string rather than nesting `WireContentBlock`s.

/// The Chat Completions request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireRequest {
    /// The model id.
    pub model: String,
    /// Conversation turns.
    pub messages: Vec<WireMessage>,
    /// Tool definitions available to the model.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<WireTool>,
    /// Max tokens to generate.
    pub max_tokens: u32,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub stop: Vec<String>,
    /// Whether to stream the response over SSE.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub stream: bool,
    /// Requested number of independent candidates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// Requests a final usage-only SSE chunk; set automatically when `stream` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<WireStreamOptions>,
}

/// `stream_options` on a streaming request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireStreamOptions {
    /// Ask the server to emit a final chunk carrying [`WireUsage`].
    pub include_usage: bool,
}

/// One conversation turn on the wire. `role` is `"system"`, `"user"`, `"assistant"` or `"tool"`
/// (the last has no equivalent in [`crate::types::MessageRole`]; it is synthesized from
/// [`crate::types::ContentBlock::ToolResult`] by [`build_wire_messages`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMessage {
    /// The wire role string.
    pub role: String,
    /// Text content. `None` for an assistant turn that is tool-calls-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Tool calls issued by an assistant turn.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<WireToolCall>,
    /// Present only on a `"tool"` role turn: the call this message answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// A tool definition on the wire, `type`-tagged as `"function"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireTool {
    /// Always `"function"` — the only tool type Chat Completions supports.
    #[serde(rename = "type")]
    pub kind: String,
    /// The function definition.
    pub function: WireFunctionDef,
}

/// A tool's function definition on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFunctionDef {
    /// The tool's name.
    pub name: String,
    /// The tool's description.
    pub description: String,
    /// The tool's JSON Schema input shape.
    pub parameters: serde_json::Value,
}

/// A full (non-streaming) tool call: the model asks for `function.arguments` (a JSON-encoded
/// string, not a nested object) to be run as `function.name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireToolCall {
    /// The call's id, echoed back in the matching `"tool"` role message.
    pub id: String,
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The call itself.
    pub function: WireFunctionCall,
}

/// The `function` object inside a [`WireToolCall`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFunctionCall {
    /// The tool's name.
    pub name: String,
    /// The call's arguments, JSON-encoded as a string (not a nested JSON object) — this is the
    /// OpenAI wire convention, unlike `crate::types::ContentBlock::ToolUse::input`.
    pub arguments: String,
}

/// The Chat Completions response body (non-streaming).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResponse {
    /// The model that served the request, if echoed back.
    #[serde(default)]
    pub model: Option<String>,
    /// One entry per requested candidate.
    pub choices: Vec<WireChoice>,
    /// Token usage for the whole request (not per choice).
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// One candidate in a non-streaming response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireChoice {
    /// This candidate's position, `0`-based.
    #[serde(default)]
    pub index: u32,
    /// The generated turn.
    pub message: WireResponseMessage,
    /// Why generation stopped. Absent is treated as `"stop"` by [`parse_wire_response`].
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The generated turn inside a [`WireChoice`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResponseMessage {
    /// Always `"assistant"` in practice; not consulted by [`parse_wire_response`].
    #[serde(default)]
    pub role: Option<String>,
    /// Generated text, if any.
    #[serde(default)]
    pub content: Option<String>,
    /// Tool calls the model issued, if any.
    #[serde(default)]
    pub tool_calls: Vec<WireToolCall>,
}

/// Usage on the wire, for both chat and embeddings responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireUsage {
    /// Input/prompt tokens.
    #[serde(default)]
    pub prompt_tokens: u32,
    /// Output/completion tokens. Always `0` on an embeddings response.
    #[serde(default)]
    pub completion_tokens: u32,
    /// Prompt-cache detail, where the backend reports it (OpenAI's `prompt_tokens_details`).
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
}

/// Prompt-cache detail nested in [`WireUsage`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePromptTokensDetails {
    /// Input tokens served from cache.
    #[serde(default)]
    pub cached_tokens: u32,
}

/// The OpenAI-style error envelope: `{"error": {"message": ..., "type": ..., "code": ...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireErrorEnvelope {
    /// The error detail.
    pub error: WireErrorDetail,
}

/// One error detail on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireErrorDetail {
    /// A human-readable message.
    pub message: String,
    /// The provider's error type/category, if it sends one.
    #[serde(rename = "type", default)]
    pub error_type: Option<String>,
    /// A provider-specific error code, if it sends one.
    #[serde(default)]
    pub code: Option<serde_json::Value>,
}

// ---- wire shapes: streaming (SSE `data:` chunks) ---------------------------------------------

/// One SSE `data:` chunk in a streaming Chat Completions response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireStreamChunk {
    /// The model, usually only present on the first chunk.
    #[serde(default)]
    pub model: Option<String>,
    /// Per-candidate deltas in this chunk.
    #[serde(default)]
    pub choices: Vec<WireStreamChoice>,
    /// Usage, present only on the final chunk when `stream_options.include_usage` was set.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// One candidate's delta within a [`WireStreamChunk`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WireStreamChoice {
    /// This candidate's position, stable across chunks.
    #[serde(default)]
    pub index: u32,
    /// The incremental content for this chunk.
    #[serde(default)]
    pub delta: WireStreamDelta,
    /// Set only on the chunk that ends this candidate.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The incremental payload of one [`WireStreamChoice`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WireStreamDelta {
    /// Set only on the first chunk for this candidate.
    #[serde(default)]
    pub role: Option<String>,
    /// A fragment of generated text to append.
    #[serde(default)]
    pub content: Option<String>,
    /// Tool call fragments to merge by [`WireToolCallDelta::index`].
    #[serde(default)]
    pub tool_calls: Vec<WireToolCallDelta>,
}

/// One fragment of one tool call, identified by `index` (stable across chunks for the same call,
/// *not* the same as [`WireChoice::index`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireToolCallDelta {
    /// Which tool call within this candidate this fragment belongs to.
    pub index: u32,
    /// Set only on the chunk that starts this call.
    #[serde(default)]
    pub id: Option<String>,
    /// The function name/arguments fragment.
    #[serde(default)]
    pub function: WireFunctionCallDelta,
}

/// The `function` object inside a [`WireToolCallDelta`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WireFunctionCallDelta {
    /// Set only on the chunk that starts this call.
    #[serde(default)]
    pub name: Option<String>,
    /// A fragment of the JSON-encoded arguments string to append.
    #[serde(default)]
    pub arguments: Option<String>,
}

// ---- wire shapes: embeddings -------------------------------------------------------------------

/// The embeddings request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEmbedRequest {
    /// The model id.
    pub model: String,
    /// The texts to embed, in order.
    pub input: Vec<String>,
}

/// The embeddings response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEmbedResponse {
    /// The model that served the request, if echoed back.
    #[serde(default)]
    pub model: Option<String>,
    /// One entry per input, not guaranteed to be in input order (see [`WireEmbedDatum::index`]).
    pub data: Vec<WireEmbedDatum>,
    /// Token usage for the request.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// One embedding vector on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEmbedDatum {
    /// This vector's position in the original `input` order.
    pub index: u32,
    /// The embedding itself.
    pub embedding: Vec<f32>,
}

// ---- shaping: crate::types -> wire (pure, unit-tested against recorded JSON) -----------------

fn wire_tool_call_from_content_block(
    id: &str,
    name: &str,
    input: &serde_json::Value,
) -> WireToolCall {
    WireToolCall {
        id: id.to_string(),
        kind: "function".to_string(),
        function: WireFunctionCall {
            name: name.to_string(),
            arguments: serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()),
        },
    }
}

fn render_text_blocks(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// Translate one crate-internal [`Message`] into zero or more [`WireMessage`]s, appending them to
/// `out`.
///
/// - `System` content collapses to one `"system"` message (empty content is dropped entirely).
/// - `User` content splits on each [`ContentBlock::ToolResult`]: any [`ContentBlock::Text`]
///   accumulated so far flushes as one `"user"` message, then the tool result becomes its own
///   `"tool"` message carrying `tool_call_id`. A [`ContentBlock::ToolUse`] appearing on a `User`
///   turn (not a valid combination in practice) is dropped rather than fabricated as a call under
///   the wrong role.
/// - `Assistant` content collapses to exactly one message: all `Text` blocks join into `content`,
///   all `ToolUse` blocks become `tool_calls`. A [`ContentBlock::ToolResult`] on an `Assistant`
///   turn is dropped for the same reason as above.
fn push_wire_messages(msg: &Message, out: &mut Vec<WireMessage>) {
    match msg.role {
        MessageRole::System => {
            let text = render_text_blocks(&msg.content);
            if !text.is_empty() {
                out.push(WireMessage {
                    role: "system".to_string(),
                    content: Some(text),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
        }
        MessageRole::User => {
            let mut buffer = String::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !buffer.is_empty() {
                            buffer.push('\n');
                        }
                        buffer.push_str(text);
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        if !buffer.is_empty() {
                            out.push(WireMessage {
                                role: "user".to_string(),
                                content: Some(std::mem::take(&mut buffer)),
                                tool_calls: Vec::new(),
                                tool_call_id: None,
                            });
                        }
                        out.push(WireMessage {
                            role: "tool".to_string(),
                            content: Some(render_text_blocks(content)),
                            tool_calls: Vec::new(),
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                    ContentBlock::ToolUse { .. } => {}
                }
            }
            if !buffer.is_empty() {
                out.push(WireMessage {
                    role: "user".to_string(),
                    content: Some(buffer),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
        }
        MessageRole::Assistant => {
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text: t } => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push(wire_tool_call_from_content_block(id, name, input));
                    }
                    ContentBlock::ToolResult { .. } => {}
                }
            }
            out.push(WireMessage {
                role: "assistant".to_string(),
                content: if text.is_empty() { None } else { Some(text) },
                tool_calls,
                tool_call_id: None,
            });
        }
    }
}

/// Translate a full message history into wire messages. See [`push_wire_messages`] for the
/// per-role mapping.
pub fn build_wire_messages(messages: &[Message]) -> Vec<WireMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        push_wire_messages(msg, &mut out);
    }
    out
}

/// Build the wire request body for `req` against `model`. `req.n` maps directly to the wire `n`
/// field (unlike `crate::anthropic`, Chat Completions natively supports multiple candidates).
pub fn build_wire_request(model: &str, req: &CompletionRequest) -> WireRequest {
    let mut messages = Vec::new();
    if let Some(system) = &req.system {
        if !system.is_empty() {
            messages.push(WireMessage {
                role: "system".to_string(),
                content: Some(system.clone()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
        }
    }
    messages.extend(build_wire_messages(&req.messages));

    WireRequest {
        model: model.to_string(),
        messages,
        tools: req
            .tools
            .iter()
            .map(|t| WireTool {
                kind: "function".to_string(),
                function: WireFunctionDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect(),
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        stop: req.stop_sequences.clone(),
        stream: req.stream,
        n: if req.n > 1 { Some(req.n) } else { None },
        stream_options: req.stream.then_some(WireStreamOptions {
            include_usage: true,
        }),
    }
}

// ---- shaping: wire -> crate::types -------------------------------------------------------------

/// Map an OpenAI-style `finish_reason` to [`StopReason`]. `"stop"` -> `EndTurn`, `"length"` and
/// `"incomplete"` -> `MaxTokens`, `"tool_calls"`/the legacy `"function_call"` -> `ToolUse`;
/// anything else (including `"content_filter"`, which has no [`StopReason`] equivalent in this
/// crate) is a [`ProviderError::MalformedResponse`], matching `crate::anthropic`'s strictness.
///
/// `"incomplete"` is not part of OpenAI's own Chat Completions vocabulary, but the LLM Gateway
/// behind DevPass returns it on a real, billed 200 OK when a reasoning model (seen live with
/// `muse-spark-1.3-contributor`) spends its whole `max_tokens` budget before finishing — the
/// same situation `"length"` describes. Treating it as malformed failed the entire request as a
/// non-retryable error instead of surfacing a truncated reply the caller can act on.
pub fn map_finish_reason(reason: &str) -> Result<StopReason, ProviderError> {
    match reason {
        "stop" => Ok(StopReason::EndTurn),
        "length" | "incomplete" => Ok(StopReason::MaxTokens),
        "tool_calls" | "function_call" => Ok(StopReason::ToolUse),
        other => Err(ProviderError::MalformedResponse(format!(
            "unknown finish_reason: {other}"
        ))),
    }
}

fn wire_message_to_content(msg: &WireResponseMessage) -> Result<Vec<ContentBlock>, ProviderError> {
    let mut content = Vec::new();
    if let Some(text) = &msg.content {
        if !text.is_empty() {
            content.push(ContentBlock::Text { text: text.clone() });
        }
    }
    for tc in &msg.tool_calls {
        let input: serde_json::Value =
            serde_json::from_str(&tc.function.arguments).map_err(|e| {
                ProviderError::MalformedResponse(format!("tool call arguments not valid JSON: {e}"))
            })?;
        content.push(ContentBlock::ToolUse {
            id: tc.id.clone(),
            name: tc.function.name.clone(),
            input,
        });
    }
    Ok(content)
}

fn wire_usage_to_usage(wire: WireUsage) -> Usage {
    Usage {
        input_tokens: wire.prompt_tokens,
        output_tokens: wire.completion_tokens,
        cache_read_tokens: wire
            .prompt_tokens_details
            .map(|d| d.cached_tokens)
            .unwrap_or(0),
        cache_write_tokens: 0,
    }
}

/// Parse a successful non-streaming Chat Completions response into a provider-independent
/// [`Completion`]. `requested_model` fills in [`ModelId::model`] when the response omits `model`
/// (some backends don't echo it back).
pub fn parse_wire_response(
    body: &[u8],
    requested_model: &str,
    provider_id: &str,
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError> {
    let wire: WireResponse = serde_json::from_slice(body).map_err(|e| {
        ProviderError::MalformedResponse(format!("failed to parse chat completion response: {e}"))
    })?;

    if wire.choices.is_empty() {
        return Err(ProviderError::MalformedResponse(
            "response had no choices".to_string(),
        ));
    }

    let mut candidates = Vec::with_capacity(wire.choices.len());
    for choice in &wire.choices {
        let content = wire_message_to_content(&choice.message)?;
        let stop_reason = map_finish_reason(choice.finish_reason.as_deref().unwrap_or("stop"))?;
        candidates.push(Candidate {
            content,
            stop_reason,
        });
    }

    let usage = wire.usage.map(wire_usage_to_usage).unwrap_or_default();

    Ok(Completion {
        model: ModelId::new(
            provider_id,
            wire.model.unwrap_or_else(|| requested_model.to_string()),
        ),
        candidates,
        usage,
        latency,
        received_at,
    })
}

/// Parse `body` (an embeddings response) into provider-independent [`Embeddings`], restoring
/// input order via [`WireEmbedDatum::index`] regardless of the order the backend returned them in.
pub fn parse_wire_embed_response(
    body: &[u8],
    requested_model: &str,
    provider_id: &str,
) -> Result<Embeddings, ProviderError> {
    let wire: WireEmbedResponse = serde_json::from_slice(body).map_err(|e| {
        ProviderError::MalformedResponse(format!("failed to parse embeddings response: {e}"))
    })?;

    let mut data = wire.data;
    data.sort_by_key(|d| d.index);
    let vectors = data.into_iter().map(|d| d.embedding).collect();
    let usage = wire.usage.map(wire_usage_to_usage).unwrap_or_default();

    Ok(Embeddings {
        model: ModelId::new(
            provider_id,
            wire.model.unwrap_or_else(|| requested_model.to_string()),
        ),
        vectors,
        usage,
    })
}

// ---- streaming: SSE parsing + reassembly ------------------------------------------------------

/// Split a fully-buffered SSE response body into its `data:` events and parse each as a
/// [`WireStreamChunk`], skipping the terminal `data: [DONE]` sentinel and blank keep-alive lines.
///
/// This crate reads the whole response body before parsing (see [`CompatProvider::complete`]'s
/// use of [`CompatProvider::send_with_retry`]) rather than parsing incrementally off a live
/// `tokio_stream`/`futures` byte stream, since [`crate::fabric::Provider::complete`] only ever
/// needs the fully-assembled [`Completion`] at the end regardless. A caller that does want
/// incremental delivery can feed each line of a live stream through the same per-event parsing
/// this function does; nothing here assumes the whole body is available except this function's
/// own signature.
pub fn parse_sse_body(body: &[u8]) -> Result<Vec<WireStreamChunk>, ProviderError> {
    let text = std::str::from_utf8(body).map_err(|e| {
        ProviderError::MalformedResponse(format!("SSE body was not valid UTF-8: {e}"))
    })?;

    let mut chunks = Vec::new();
    for event in text.split("\n\n") {
        for line in event.lines() {
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let chunk: WireStreamChunk = serde_json::from_str(data).map_err(|e| {
                ProviderError::MalformedResponse(format!("failed to parse SSE chunk: {e}"))
            })?;
            chunks.push(chunk);
        }
    }
    Ok(chunks)
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Default)]
struct ChoiceAccumulator {
    content: String,
    tool_calls: BTreeMap<u32, ToolCallAccumulator>,
    finish_reason: Option<String>,
}

/// Reassemble a sequence of [`WireStreamChunk`]s (in arrival order) into the same [`Completion`]
/// shape [`parse_wire_response`] produces from a non-streaming response, merging each candidate's
/// text deltas and each tool call's argument fragments (keyed by [`WireToolCallDelta::index`]).
pub fn assemble_streamed_completion(
    chunks: &[WireStreamChunk],
    requested_model: &str,
    provider_id: &str,
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError> {
    let mut choices: BTreeMap<u32, ChoiceAccumulator> = BTreeMap::new();
    let mut model: Option<String> = None;
    let mut usage: Option<WireUsage> = None;

    for chunk in chunks {
        if model.is_none() {
            model = chunk.model.clone();
        }
        if chunk.usage.is_some() {
            usage = chunk.usage.clone();
        }
        for choice in &chunk.choices {
            let accum = choices.entry(choice.index).or_default();
            if let Some(text) = &choice.delta.content {
                accum.content.push_str(text);
            }
            for tc in &choice.delta.tool_calls {
                let entry = accum.tool_calls.entry(tc.index).or_default();
                if let Some(id) = &tc.id {
                    entry.id = Some(id.clone());
                }
                if let Some(name) = &tc.function.name {
                    entry.name = Some(name.clone());
                }
                if let Some(fragment) = &tc.function.arguments {
                    entry.arguments.push_str(fragment);
                }
            }
            if let Some(reason) = &choice.finish_reason {
                accum.finish_reason = Some(reason.clone());
            }
        }
    }

    if choices.is_empty() {
        return Err(ProviderError::MalformedResponse(
            "stream produced no choices".to_string(),
        ));
    }

    let mut candidates = Vec::with_capacity(choices.len());
    for (_, accum) in choices {
        let mut content = Vec::new();
        if !accum.content.is_empty() {
            content.push(ContentBlock::Text {
                text: accum.content,
            });
        }
        for (_, tc) in accum.tool_calls {
            let input: serde_json::Value = if tc.arguments.is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&tc.arguments).map_err(|e| {
                    ProviderError::MalformedResponse(format!(
                        "streamed tool call arguments not valid JSON: {e}"
                    ))
                })?
            };
            content.push(ContentBlock::ToolUse {
                id: tc.id.unwrap_or_default(),
                name: tc.name.unwrap_or_default(),
                input,
            });
        }
        let stop_reason = map_finish_reason(accum.finish_reason.as_deref().unwrap_or("stop"))?;
        candidates.push(Candidate {
            content,
            stop_reason,
        });
    }

    let usage = usage.map(wire_usage_to_usage).unwrap_or_default();

    Ok(Completion {
        model: ModelId::new(
            provider_id,
            model.unwrap_or_else(|| requested_model.to_string()),
        ),
        candidates,
        usage,
        latency,
        received_at,
    })
}

// ---- HTTP: headers + error classification ------------------------------------------------------

/// Build the auth and extra headers for a request against `config`.
pub fn build_headers(config: &CompatConfig) -> Result<reqwest::header::HeaderMap, ProviderError> {
    let mut headers = reqwest::header::HeaderMap::new();

    if let Some(api_key) = &config.api_key {
        match &config.auth_style {
            AuthStyle::Bearer => {
                let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}"))
                    .map_err(|e| {
                        ProviderError::InvalidRequest(format!(
                            "api key is not a valid header value: {e}"
                        ))
                    })?;
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
            AuthStyle::Header(name) => {
                let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| {
                        ProviderError::InvalidRequest(format!(
                            "invalid auth header name {name:?}: {e}"
                        ))
                    })?;
                let value = reqwest::header::HeaderValue::from_str(api_key).map_err(|e| {
                    ProviderError::InvalidRequest(format!(
                        "api key is not a valid header value: {e}"
                    ))
                })?;
                headers.insert(header_name, value);
            }
            AuthStyle::None => {}
        }
    }

    for (name, value) in &config.extra_headers {
        let header_name =
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                ProviderError::InvalidRequest(format!("invalid extra header name {name:?}: {e}"))
            })?;
        let header_value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
            ProviderError::InvalidRequest(format!("invalid extra header value for {name:?}: {e}"))
        })?;
        headers.insert(header_name, header_value);
    }

    Ok(headers)
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

fn parse_error_message(body: &[u8], fallback: &str) -> String {
    serde_json::from_slice::<WireErrorEnvelope>(body)
        .map(|env| env.error.message)
        .unwrap_or_else(|_| fallback.to_string())
}

/// Classify an HTTP response as success, a retryable failure, or a terminal failure.
///
/// 200-299 -> `Ok(())`. 429 -> [`ProviderError::RateLimited`] with `retry_after` parsed from the
/// `Retry-After` header (seconds only, matching `crate::anthropic`). 500-599 ->
/// [`ProviderError::Unavailable`]. 401/403 -> [`ProviderError::AuthFailed`]. 413 ->
/// [`ProviderError::TooLarge`]. Any other 4xx -> [`ProviderError::InvalidRequest`].
pub fn classify_status(
    status: reqwest::StatusCode,
    retry_after_header: Option<&str>,
    body: &[u8],
) -> Result<(), ProviderError> {
    if status.is_success() {
        return Ok(());
    }

    let retry_after = retry_after_header.and_then(parse_retry_after);

    if status.as_u16() == 429 {
        let message = parse_error_message(body, "rate limited");
        return Err(ProviderError::RateLimited {
            message,
            retry_after,
        });
    }

    if status.is_server_error() {
        let message = parse_error_message(body, "provider unavailable");
        return Err(ProviderError::Unavailable(message));
    }

    if status.as_u16() == 401 || status.as_u16() == 403 {
        let message = parse_error_message(body, "authentication failed");
        return Err(ProviderError::AuthFailed(message));
    }

    if status.as_u16() == 413 {
        let message = parse_error_message(body, "request too large");
        return Err(ProviderError::TooLarge(message));
    }

    let message = parse_error_message(body, &format!("request failed with status {status}"));
    Err(ProviderError::InvalidRequest(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Message, MessageRole, ToolDef};

    fn sample_request() -> CompletionRequest {
        CompletionRequest {
            system: Some("You are a helpful assistant.".to_string()),
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            }],
            tools: vec![ToolDef {
                name: "get_weather".to_string(),
                description: "Get the weather".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 1024,
            temperature: Some(0.0),
            stop_sequences: vec!["STOP".to_string()],
            stream: false,
            n: 1,
        }
    }

    // ---- build_wire_request / build_wire_messages ----

    #[test]
    fn build_wire_request_maps_system_and_user_messages() {
        let wire = build_wire_request("gpt-test", &sample_request());
        assert_eq!(wire.model, "gpt-test");
        assert_eq!(wire.messages.len(), 2);
        assert_eq!(wire.messages[0].role, "system");
        assert_eq!(
            wire.messages[0].content.as_deref(),
            Some("You are a helpful assistant.")
        );
        assert_eq!(wire.messages[1].role, "user");
        assert_eq!(wire.messages[1].content.as_deref(), Some("Hello"));
        assert_eq!(wire.max_tokens, 1024);
        assert_eq!(wire.temperature, Some(0.0));
        assert_eq!(wire.stop, vec!["STOP".to_string()]);
        assert!(!wire.stream);
        assert_eq!(wire.n, None);
        assert_eq!(wire.tools.len(), 1);
        assert_eq!(wire.tools[0].kind, "function");
        assert_eq!(wire.tools[0].function.name, "get_weather");
    }

    #[test]
    fn build_wire_request_maps_n_greater_than_one() {
        let mut req = sample_request();
        req.n = 3;
        let wire = build_wire_request("gpt-test", &req);
        assert_eq!(wire.n, Some(3));
    }

    #[test]
    fn build_wire_request_sets_stream_options_only_when_streaming() {
        let mut req = sample_request();
        req.stream = true;
        let wire = build_wire_request("gpt-test", &req);
        assert!(wire.stream);
        assert_eq!(wire.stream_options.map(|o| o.include_usage), Some(true));

        let wire_no_stream = build_wire_request("gpt-test", &sample_request());
        assert!(wire_no_stream.stream_options.is_none());
    }

    #[test]
    fn build_wire_messages_splits_tool_result_from_surrounding_text() {
        let messages = vec![Message {
            role: MessageRole::User,
            content: vec![
                ContentBlock::Text {
                    text: "before".to_string(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "72F".to_string(),
                    }],
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "after".to_string(),
                },
            ],
        }];
        let wire = build_wire_messages(&messages);
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[0].role, "user");
        assert_eq!(wire[0].content.as_deref(), Some("before"));
        assert_eq!(wire[1].role, "tool");
        assert_eq!(wire[1].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(wire[1].content.as_deref(), Some("72F"));
        assert_eq!(wire[2].role, "user");
        assert_eq!(wire[2].content.as_deref(), Some("after"));
    }

    #[test]
    fn build_wire_messages_collapses_assistant_text_and_tool_calls_into_one_message() {
        let messages = vec![Message {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Let me check.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    input: serde_json::json!({"city": "SF"}),
                },
            ],
        }];
        let wire = build_wire_messages(&messages);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "assistant");
        assert_eq!(wire[0].content.as_deref(), Some("Let me check."));
        assert_eq!(wire[0].tool_calls.len(), 1);
        assert_eq!(wire[0].tool_calls[0].id, "call_1");
        assert_eq!(wire[0].tool_calls[0].function.name, "get_weather");
        let args: serde_json::Value =
            serde_json::from_str(&wire[0].tool_calls[0].function.arguments).expect("valid JSON");
        assert_eq!(args["city"], "SF");
    }

    #[test]
    fn build_wire_messages_drops_empty_system_message() {
        let messages = vec![Message {
            role: MessageRole::System,
            content: vec![],
        }];
        assert!(build_wire_messages(&messages).is_empty());
    }

    // ---- parse_wire_response ----

    const RECORDED_TEXT_RESPONSE: &str = r#"{
        "model": "gpt-test-2026",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello there!"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 12, "completion_tokens": 5, "prompt_tokens_details": {"cached_tokens": 3}}
    }"#;

    #[test]
    fn parse_wire_response_maps_text_content_and_usage() {
        let ts = tm_types::Timestamp::from_unix_seconds(1_700_000_000);
        let completion = parse_wire_response(
            RECORDED_TEXT_RESPONSE.as_bytes(),
            "gpt-test",
            "openai",
            Duration::from_millis(250),
            ts,
        )
        .expect("parses recorded response");

        assert_eq!(completion.model, ModelId::new("openai", "gpt-test-2026"));
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "Hello there!".to_string()
            }]
        );
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(
            completion.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 5,
                cache_read_tokens: 3,
                cache_write_tokens: 0,
            }
        );
        assert_eq!(completion.latency, Duration::from_millis(250));
        assert_eq!(completion.received_at, ts);
    }

    const RECORDED_TOOL_CALL_RESPONSE: &str = r#"{
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\": \"SF\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 20, "completion_tokens": 8}
    }"#;

    #[test]
    fn parse_wire_response_maps_tool_calls_and_falls_back_to_requested_model() {
        let ts = tm_types::Timestamp::EPOCH;
        let completion = parse_wire_response(
            RECORDED_TOOL_CALL_RESPONSE.as_bytes(),
            "gpt-test",
            "openai",
            Duration::ZERO,
            ts,
        )
        .expect("parses recorded response");

        assert_eq!(completion.model, ModelId::new("openai", "gpt-test"));
        assert_eq!(completion.candidates[0].stop_reason, StopReason::ToolUse);
        match &completion.candidates[0].content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("expected tool use block, got {other:?}"),
        }
    }

    #[test]
    fn parse_wire_response_maps_multiple_choices() {
        let body = r#"{
            "choices": [
                {"index": 0, "message": {"content": "first"}, "finish_reason": "stop"},
                {"index": 1, "message": {"content": "second"}, "finish_reason": "length"}
            ]
        }"#;
        let completion = parse_wire_response(
            body.as_bytes(),
            "m",
            "p",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .expect("parses");
        assert_eq!(completion.candidates.len(), 2);
        assert_eq!(completion.candidates[1].stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn parse_wire_response_rejects_invalid_json() {
        let err = parse_wire_response(
            b"not json",
            "m",
            "p",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn parse_wire_response_rejects_empty_choices() {
        let err = parse_wire_response(
            br#"{"choices": []}"#,
            "m",
            "p",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn parse_wire_response_rejects_unknown_finish_reason() {
        let body =
            r#"{"choices": [{"message": {"content": "x"}, "finish_reason": "content_filter"}]}"#;
        let err = parse_wire_response(
            body.as_bytes(),
            "m",
            "p",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn map_finish_reason_treats_gateway_incomplete_like_length() {
        assert_eq!(
            map_finish_reason("incomplete").ok(),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            map_finish_reason("length").ok(),
            Some(StopReason::MaxTokens)
        );
    }

    #[test]
    fn parse_wire_response_accepts_gateway_incomplete_finish_reason() {
        // The shape the LLM Gateway returns when a reasoning model exhausts `max_tokens` before
        // producing visible output: a 200 OK with null content and `"incomplete"`.
        let body = r#"{"model": "muse-spark-1.3-contributor", "choices": [{"message": {"role": "assistant", "content": null}, "finish_reason": "incomplete"}], "usage": {"prompt_tokens": 12, "completion_tokens": 32}}"#;
        let completion = parse_wire_response(
            body.as_bytes(),
            "m",
            "devpass",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .expect("incomplete is a truncated reply, not a malformed one");
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(completion.candidates[0].stop_reason, StopReason::MaxTokens);
        assert!(completion.candidates[0].content.is_empty());
        assert_eq!(completion.usage.output_tokens, 32);
    }

    #[test]
    fn assemble_streamed_completion_accepts_gateway_incomplete_finish_reason() {
        let body = concat!(
            "data: {\"choices\": [{\"index\": 0, \"delta\": {\"content\": \"partial\"}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {}, \"finish_reason\": \"incomplete\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let chunks = parse_sse_body(body.as_bytes()).expect("parses");
        let completion = assemble_streamed_completion(
            &chunks,
            "m",
            "devpass",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .expect("incomplete is a truncated reply, not a malformed one");
        assert_eq!(completion.candidates[0].stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn parse_wire_response_rejects_malformed_tool_call_arguments() {
        let body = r#"{"choices": [{"message": {"tool_calls": [{"id": "1", "type": "function", "function": {"name": "f", "arguments": "not json"}}]}, "finish_reason": "tool_calls"}]}"#;
        let err = parse_wire_response(
            body.as_bytes(),
            "m",
            "p",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    // ---- embeddings ----

    #[test]
    fn parse_wire_embed_response_restores_input_order() {
        let body = r#"{
            "model": "embed-test",
            "data": [
                {"index": 1, "embedding": [0.4, 0.5]},
                {"index": 0, "embedding": [0.1, 0.2]}
            ],
            "usage": {"prompt_tokens": 7, "completion_tokens": 0}
        }"#;
        let embeddings =
            parse_wire_embed_response(body.as_bytes(), "embed-test", "openai").expect("parses");
        assert_eq!(embeddings.vectors, vec![vec![0.1, 0.2], vec![0.4, 0.5]]);
        assert_eq!(embeddings.usage.input_tokens, 7);
    }

    #[test]
    fn parse_wire_embed_response_rejects_invalid_json() {
        let err = parse_wire_embed_response(b"not json", "m", "p").unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    // ---- SSE streaming ----

    #[test]
    fn parse_sse_body_skips_done_sentinel_and_blank_lines() {
        let body = b"data: {\"choices\":[]}\n\ndata: [DONE]\n\n";
        let chunks = parse_sse_body(body).expect("parses");
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn parse_sse_body_rejects_invalid_json_in_a_data_line() {
        let err = parse_sse_body(b"data: not json\n\n").unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn assemble_streamed_completion_merges_text_deltas() {
        let body = concat!(
            "data: {\"model\": \"gpt-stream\", \"choices\": [{\"index\": 0, \"delta\": {\"role\": \"assistant\", \"content\": \"Hel\"}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {\"content\": \"lo!\"}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {}, \"finish_reason\": \"stop\"}]}\n\n",
            "data: {\"choices\": [], \"usage\": {\"prompt_tokens\": 4, \"completion_tokens\": 2}}\n\n",
            "data: [DONE]\n\n",
        );
        let chunks = parse_sse_body(body.as_bytes()).expect("parses");
        let completion = assemble_streamed_completion(
            &chunks,
            "gpt-test",
            "openai",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .expect("assembles");

        assert_eq!(completion.model, ModelId::new("openai", "gpt-stream"));
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "Hello!".to_string()
            }]
        );
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage.input_tokens, 4);
        assert_eq!(completion.usage.output_tokens, 2);
    }

    #[test]
    fn assemble_streamed_completion_merges_tool_call_argument_fragments() {
        let body = concat!(
            "data: {\"choices\": [{\"index\": 0, \"delta\": {\"tool_calls\": [{\"index\": 0, \"id\": \"call_1\", \"function\": {\"name\": \"get_weather\", \"arguments\": \"\"}}]}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {\"tool_calls\": [{\"index\": 0, \"function\": {\"arguments\": \"{\\\"city\\\":\"}}]}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {\"tool_calls\": [{\"index\": 0, \"function\": {\"arguments\": \"\\\"SF\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {}, \"finish_reason\": \"tool_calls\"}]}\n\n",
        );
        let chunks = parse_sse_body(body.as_bytes()).expect("parses");
        let completion = assemble_streamed_completion(
            &chunks,
            "gpt-test",
            "openai",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .expect("assembles");

        assert_eq!(completion.candidates[0].stop_reason, StopReason::ToolUse);
        match &completion.candidates[0].content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("expected tool use block, got {other:?}"),
        }
    }

    #[test]
    fn assemble_streamed_completion_rejects_empty_stream() {
        let err =
            assemble_streamed_completion(&[], "m", "p", Duration::ZERO, tm_types::Timestamp::EPOCH)
                .unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn assemble_streamed_completion_orders_multiple_choices_by_index() {
        let body = concat!(
            "data: {\"choices\": [{\"index\": 1, \"delta\": {\"content\": \"second\"}, \"finish_reason\": \"stop\"}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {\"content\": \"first\"}, \"finish_reason\": \"stop\"}]}\n\n",
        );
        let chunks = parse_sse_body(body.as_bytes()).expect("parses");
        let completion = assemble_streamed_completion(
            &chunks,
            "m",
            "p",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .expect("assembles");
        assert_eq!(completion.candidates.len(), 2);
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "first".to_string()
            }]
        );
        assert_eq!(
            completion.candidates[1].content,
            vec![ContentBlock::Text {
                text: "second".to_string()
            }]
        );
    }

    // ---- headers ----

    #[test]
    fn build_headers_uses_bearer_by_default() {
        let config = CompatConfig::new("openai", "https://api.openai.com/v1", "gpt-test")
            .with_api_key("sk-test");
        let headers = build_headers(&config).expect("builds headers");
        assert_eq!(
            headers.get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer sk-test"
        );
    }

    #[test]
    fn build_headers_uses_custom_header_name() {
        let config = CompatConfig::new("azure", "https://example.invalid", "gpt-test")
            .with_api_key("azure-key")
            .with_auth_style(AuthStyle::Header("api-key".to_string()));
        let headers = build_headers(&config).expect("builds headers");
        assert_eq!(headers.get("api-key").unwrap(), "azure-key");
        assert!(headers.get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn build_headers_sends_no_auth_header_for_auth_style_none() {
        let config = CompatConfig::new("ollama", "http://localhost:11434/v1", "llama3")
            .with_auth_style(AuthStyle::None);
        let headers = build_headers(&config).expect("builds headers");
        assert!(headers.get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn build_headers_includes_extra_headers() {
        let config = CompatConfig::new("openai", "https://api.openai.com/v1", "gpt-test")
            .with_extra_header("OpenAI-Organization", "org-123");
        let headers = build_headers(&config).expect("builds headers");
        assert_eq!(headers.get("OpenAI-Organization").unwrap(), "org-123");
    }

    // ---- classify_status ----

    #[test]
    fn classify_status_ok_on_200() {
        assert!(classify_status(reqwest::StatusCode::OK, None, b"{}").is_ok());
    }

    #[test]
    fn classify_status_rate_limited_with_retry_after_seconds() {
        let err = classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some("20"),
            br#"{"error": {"message": "slow down"}}"#,
        )
        .unwrap_err();
        match err {
            ProviderError::RateLimited {
                message,
                retry_after,
            } => {
                assert_eq!(message, "slow down");
                assert_eq!(retry_after, Some(Duration::from_secs(20)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_server_error_is_unavailable() {
        let err = classify_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            None,
            br#"{"error": {"message": "overloaded"}}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::Unavailable(m) if m == "overloaded"));
    }

    #[test]
    fn classify_status_401_and_403_are_auth_failed() {
        assert!(matches!(
            classify_status(reqwest::StatusCode::UNAUTHORIZED, None, b"{}").unwrap_err(),
            ProviderError::AuthFailed(_)
        ));
        assert!(matches!(
            classify_status(reqwest::StatusCode::FORBIDDEN, None, b"{}").unwrap_err(),
            ProviderError::AuthFailed(_)
        ));
    }

    #[test]
    fn classify_status_413_is_too_large() {
        assert!(matches!(
            classify_status(reqwest::StatusCode::PAYLOAD_TOO_LARGE, None, b"{}").unwrap_err(),
            ProviderError::TooLarge(_)
        ));
    }

    #[test]
    fn classify_status_generic_400_is_invalid_request() {
        let err = classify_status(
            reqwest::StatusCode::BAD_REQUEST,
            None,
            br#"{"error": {"message": "bad field"}}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(m) if m == "bad field"));
    }

    #[test]
    fn classify_status_falls_back_to_status_text_on_unparseable_error_body() {
        let err = classify_status(reqwest::StatusCode::BAD_REQUEST, None, b"not json").unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    // ---- config / construction ----

    #[test]
    fn compat_config_builders_apply_overrides() {
        let config = CompatConfig::new("groq", "https://api.groq.com/openai/v1", "llama-3.3-70b")
            .with_api_key("gsk-test")
            .with_max_retries(5)
            .with_timeout(Duration::from_secs(30))
            .without_embeddings();
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert_eq!(config.embeddings_path, None);
        assert_eq!(config.api_key.as_deref(), Some("gsk-test"));
    }

    #[test]
    fn compat_provider_new_builds_without_touching_the_environment() {
        let clock: Arc<dyn Clock> = Arc::new(tm_types::SystemClock);
        let config = CompatConfig::new("openai", "https://api.openai.com/v1", "gpt-test")
            .with_api_key("sk-test");
        let provider = CompatProvider::new(config, clock).expect("builds provider");
        assert_eq!(provider.id(), "openai");
    }

    #[test]
    fn devpass_info_lists_all_three_required_env_vars() {
        let info = DevPassProvider::info();
        assert_eq!(info.id, "devpass");
        assert_eq!(info.env_vars.len(), 3);
        assert!(info.env_vars.iter().all(|v| v.required));
    }

    // ---- DevPassProvider::preferred_model ----
    //
    // `preferred_model` is the only thing in this crate that reads the three `DEVPASS_*` env
    // vars for a *default-preference* decision, and these are the only tests in this crate that
    // set them, so a lock scoped to just these three tests is sufficient to stop them racing
    // each other under `cargo test`'s default multi-threaded runner — no other test anywhere in
    // this crate touches these var names. Nothing here calls
    // `RoleTable::default_table`/`default_table_with`: that logic is pure (see
    // `role_config.rs`), so it is tested there with no env involved and no lock needed at all.

    fn devpass_pref_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn clear_devpass_pref_env() {
        for var in ["DEVPASS_API_KEY", "DEVPASS_BASE_URL", "DEVPASS_MODEL"] {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn preferred_model_is_none_when_nothing_is_set() {
        let _guard = devpass_pref_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_devpass_pref_env();
        assert_eq!(DevPassProvider::preferred_model(), None);
    }

    #[test]
    fn preferred_model_is_none_when_only_api_key_is_set() {
        let _guard = devpass_pref_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_devpass_pref_env();
        std::env::set_var("DEVPASS_API_KEY", "sk-test");
        assert_eq!(DevPassProvider::preferred_model(), None);
        clear_devpass_pref_env();
    }

    #[test]
    fn preferred_model_is_none_when_base_url_and_model_are_set_but_key_is_empty() {
        let _guard = devpass_pref_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_devpass_pref_env();
        std::env::set_var("DEVPASS_API_KEY", "");
        std::env::set_var("DEVPASS_BASE_URL", "https://example.invalid/devpass");
        std::env::set_var("DEVPASS_MODEL", "some-model");
        assert_eq!(
            DevPassProvider::preferred_model(),
            None,
            "an empty-string var must count as absent, not merely unset"
        );
        clear_devpass_pref_env();
    }

    #[test]
    fn preferred_model_is_some_when_all_three_are_set_and_nonempty() {
        let _guard = devpass_pref_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_devpass_pref_env();
        std::env::set_var("DEVPASS_API_KEY", "sk-test");
        std::env::set_var("DEVPASS_BASE_URL", "https://example.invalid/devpass");
        std::env::set_var("DEVPASS_MODEL", "devpass-default-model");
        assert_eq!(
            DevPassProvider::preferred_model(),
            Some("devpass-default-model".to_string())
        );
        clear_devpass_pref_env();
    }
}
