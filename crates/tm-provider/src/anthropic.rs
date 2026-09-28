//! [`AnthropicProvider`]: the Anthropic Messages API over `reqwest` + rustls.
//!
//! Owns request/response shaping (our wire-independent [`crate::types`] <-> the Messages API
//! JSON shape), tool-use blocks, streaming, usage accounting, prompt-cache-aware headers, and
//! retry on 429/5xx honoring `Retry-After`. The API key is read once at construction, via
//! [`tm_auth::EnvApiKey`] against [`API_KEY_ENV_VAR`] (`SPEC.md` §28.2's auth-adapter layer,
//! rather than a bare `std::env::var` call); nothing in this module reads environment or
//! wall-clock state elsewhere, so shaping is unit-testable against recorded JSON without a
//! network call.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_auth::EnvApiKey;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};
use crate::wire_names::WireNames;

/// The Anthropic Messages API endpoint this provider targets.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The Messages API version header value this provider speaks.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Name of the environment variable holding the API key.
pub const API_KEY_ENV_VAR: &str = "ANTHROPIC_API_KEY";

/// Default maximum retry attempts on 429/5xx before giving up.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Default request timeout for the underlying HTTP client.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Floor for exponential backoff when the provider gives no `Retry-After`.
const BACKOFF_FLOOR: Duration = Duration::from_millis(500);

/// A real Anthropic Messages API client.
pub struct AnthropicProvider {
    id: String,
    model: ModelId,
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    clock: std::sync::Arc<dyn Clock>,
    max_retries: u32,
}

impl AnthropicProvider {
    /// Build a provider for `model`, reading the API key from [`API_KEY_ENV_VAR`] through
    /// [`tm_auth::EnvApiKey`].
    pub fn from_env(
        model: ModelId,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self, ProviderError> {
        let api_key = EnvApiKey::new(API_KEY_ENV_VAR)
            .resolve()
            .map_err(|e| ProviderError::AuthFailed(e.to_string()))?
            .expose_secret()
            .to_string();
        Self::with_config(model, api_key, DEFAULT_BASE_URL.to_string(), clock)
    }

    /// Build a provider with an explicit key and base URL, for tests that stand up a local mock
    /// HTTP server (shaping tests use recorded JSON directly and never need this, but it's here
    /// for completeness / future integration tests outside this crate).
    pub fn with_config(
        model: ModelId,
        api_key: String,
        base_url: String,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Unavailable(format!("failed to build HTTP client: {e}")))?;
        Ok(AnthropicProvider {
            id: "anthropic".to_string(),
            model,
            base_url,
            api_key,
            http,
            clock,
            max_retries: DEFAULT_MAX_RETRIES,
        })
    }

    /// Maximum retry attempts on 429/5xx before giving up. Default is set in the constructors;
    /// exposed for tests that want to force exhaustion quickly.
    pub fn set_max_retries(&mut self, max_retries: u32) {
        self.max_retries = max_retries;
    }
}

// ---- wire shapes -----------------------------------------------------------------------------
//
// These mirror the Anthropic Messages API JSON exactly (snake_case field names, externally
// tagged content blocks) and exist only to (de)serialize at the HTTP boundary; `crate::types` is
// the provider-independent shape used everywhere else in the workspace.

/// The Messages API request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireRequest {
    /// The model id.
    pub model: String,
    /// System prompt, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
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
    pub stop_sequences: Vec<String>,
    /// Whether to stream the response over SSE.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub stream: bool,
}

/// One conversation turn on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMessage {
    /// `"user"` or `"assistant"` (the API has no wire `"system"` role; it's a top-level field).
    pub role: String,
    /// Content blocks for this turn.
    pub content: Vec<WireContentBlock>,
}

/// One content block on the wire. Externally tagged via `type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireContentBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// A tool call.
    ToolUse {
        /// The call's id.
        id: String,
        /// The tool's name.
        name: String,
        /// The call's input.
        input: serde_json::Value,
    },
    /// A tool result.
    ToolResult {
        /// The call this answers.
        tool_use_id: String,
        /// The result content.
        content: Vec<WireContentBlock>,
        /// Whether the tool call itself errored.
        #[serde(default)]
        is_error: bool,
    },
}

/// A tool definition on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireTool {
    /// The tool's name.
    pub name: String,
    /// The tool's description.
    pub description: String,
    /// The tool's JSON Schema input shape.
    pub input_schema: serde_json::Value,
    /// A prompt-cache breakpoint (`{"type": "ephemeral"}`), set only on the last tool in the
    /// request. Anthropic's Messages API caches everything up to and including the block that
    /// carries `cache_control`, so one breakpoint here covers the whole (typically stable, whole
    /// tool-schema) prefix ahead of the per-turn `messages` — see
    /// [`build_wire_request_with_names`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// An Anthropic prompt-cache breakpoint marker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    /// Always `"ephemeral"`, the only breakpoint type the Messages API currently defines.
    #[serde(rename = "type")]
    pub cache_type: String,
}

impl CacheControl {
    /// The one breakpoint type the Messages API defines today.
    pub fn ephemeral() -> Self {
        CacheControl {
            cache_type: "ephemeral".to_string(),
        }
    }
}

/// The Messages API response body (non-streaming).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResponse {
    /// The model that served the request.
    pub model: String,
    /// Generated content blocks.
    pub content: Vec<WireContentBlock>,
    /// Why generation stopped.
    pub stop_reason: String,
    /// Token usage.
    pub usage: WireUsage,
}

/// Usage on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireUsage {
    /// Input tokens.
    pub input_tokens: u32,
    /// Output tokens.
    pub output_tokens: u32,
    /// Cache-read input tokens.
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    /// Cache-creation (write) input tokens.
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
}

/// The Messages API's error envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireErrorEnvelope {
    /// The error detail.
    pub error: WireErrorDetail,
}

/// One error detail on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireErrorDetail {
    /// The Anthropic error type, e.g. `"rate_limit_error"`, `"overloaded_error"`.
    #[serde(rename = "type")]
    pub error_type: String,
    /// A human-readable message.
    pub message: String,
}

// ---- shaping (pure, unit-tested against recorded JSON) --------------------------------------

fn content_block_to_wire(block: &crate::types::ContentBlock) -> WireContentBlock {
    use crate::types::ContentBlock;
    match block {
        ContentBlock::Text { text } => WireContentBlock::Text { text: text.clone() },
        ContentBlock::ToolUse { id, name, input } => WireContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => WireContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.iter().map(content_block_to_wire).collect(),
            is_error: *is_error,
        },
    }
}

fn wire_to_content_block(block: &WireContentBlock) -> crate::types::ContentBlock {
    use crate::types::ContentBlock;
    match block {
        WireContentBlock::Text { text } => ContentBlock::Text { text: text.clone() },
        WireContentBlock::ToolUse { id, name, input } => ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        WireContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.iter().map(wire_to_content_block).collect(),
            is_error: *is_error,
        },
    }
}

/// Build the wire request body for `req` against `model`.
///
/// Maps [`crate::types::MessageRole::System`] messages' text content into
/// [`WireRequest::system`] (concatenated with `\n\n` if there is more than one, though in
/// practice callers send at most one); maps `User`/`Assistant` roles into [`WireMessage`] turns.
/// `req.n` has no direct Messages API equivalent (the API always returns one candidate); this
/// function always produces a single-candidate wire request, and callers wanting multiple
/// candidates must issue multiple requests.
///
/// Tool names go on the wire through [`WireNames::for_request`] (dotted tm names are not valid
/// Messages API tool names; see [`crate::wire_names`]).
pub fn build_wire_request(model: &ModelId, req: &CompletionRequest) -> WireRequest {
    build_wire_request_with_names(model, req, &WireNames::for_request(req))
}

/// [`build_wire_request`] with an explicit name map, so [`AnthropicProvider::complete`] can decode
/// the response with the same one.
pub fn build_wire_request_with_names(
    model: &ModelId,
    req: &CompletionRequest,
    names: &WireNames,
) -> WireRequest {
    use crate::types::MessageRole;

    let req = names.encode_request(req);
    let req = req.as_ref();

    let mut system_parts: Vec<String> = Vec::new();
    if let Some(system) = &req.system {
        system_parts.push(system.clone());
    }

    let mut messages = Vec::with_capacity(req.messages.len());
    for msg in &req.messages {
        match msg.role {
            MessageRole::System => {
                for block in &msg.content {
                    if let crate::types::ContentBlock::Text { text } = block {
                        system_parts.push(text.clone());
                    }
                }
            }
            MessageRole::User | MessageRole::Assistant => {
                let role = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => unreachable!("system handled above"),
                };
                messages.push(WireMessage {
                    role: role.to_string(),
                    content: msg.content.iter().map(content_block_to_wire).collect(),
                });
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    let tool_count = req.tools.len();
    let tools = req
        .tools
        .iter()
        .enumerate()
        .map(|(i, t)| WireTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
            // The prompt-caching-eligible prefix is everything up to and including this
            // block's position on the wire; one breakpoint on the last tool caches the whole
            // (usually per-turn-identical) tool-schema block without touching `system`'s shape.
            cache_control: if i + 1 == tool_count {
                Some(CacheControl::ephemeral())
            } else {
                None
            },
        })
        .collect();

    WireRequest {
        model: model.model.clone(),
        system,
        messages,
        tools,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        stop_sequences: req.stop_sequences.clone(),
        stream: req.stream,
    }
}

/// Build the prompt-cache-aware and auth headers for a request.
///
/// Always includes `x-api-key` and `anthropic-version`. System prompts and tool definitions are
/// sent with prompt caching enabled by convention, so the `anthropic-beta` header advertising
/// `prompt-caching-2024-07-31` is always included.
pub fn build_headers(api_key: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "x-api-key",
        reqwest::header::HeaderValue::from_str(api_key)
            .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
    );
    headers.insert(
        "anthropic-version",
        reqwest::header::HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    headers.insert(
        "anthropic-beta",
        reqwest::header::HeaderValue::from_static("prompt-caching-2024-07-31"),
    );
    headers
}

fn map_stop_reason(reason: &str) -> Result<crate::types::StopReason, ProviderError> {
    use crate::types::StopReason;
    match reason {
        "end_turn" => Ok(StopReason::EndTurn),
        "max_tokens" => Ok(StopReason::MaxTokens),
        "stop_sequence" => Ok(StopReason::StopSequence),
        "tool_use" => Ok(StopReason::ToolUse),
        other => Err(ProviderError::MalformedResponse(format!(
            "unknown stop_reason: {other}"
        ))),
    }
}

/// Parse a successful Messages API response body into a provider-independent [`Completion`].
pub fn parse_wire_response(
    body: &[u8],
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError> {
    use crate::types::{Candidate, Usage};

    let wire: WireResponse = serde_json::from_slice(body).map_err(|e| {
        ProviderError::MalformedResponse(format!("failed to parse Messages API response: {e}"))
    })?;

    let stop_reason = map_stop_reason(&wire.stop_reason)?;
    let content = wire.content.iter().map(wire_to_content_block).collect();

    Ok(Completion {
        model: ModelId::new("anthropic", wire.model),
        candidates: vec![Candidate {
            content,
            stop_reason,
        }],
        usage: Usage {
            input_tokens: wire.usage.input_tokens,
            output_tokens: wire.usage.output_tokens,
            cache_read_tokens: wire.usage.cache_read_input_tokens,
            cache_write_tokens: wire.usage.cache_creation_input_tokens,
        },
        latency,
        received_at,
    })
}

/// Parse a `Retry-After` header value as an integer number of seconds, the documented Anthropic
/// behavior. An HTTP-date value (RFC 7231's alternate form) is not handled, since the workspace
/// has no HTTP-date parsing dependency and Anthropic does not use that form in practice.
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

fn parse_error_message(body: &[u8], fallback: &str) -> String {
    serde_json::from_slice::<WireErrorEnvelope>(body)
        .map(|env| env.error.message)
        .unwrap_or_else(|_| fallback.to_string())
}

fn rate_limit_message(message: String, retry_after: Option<Duration>) -> String {
    match retry_after {
        Some(wait) => format!("{message} Retry after {} seconds.", wait.as_secs()),
        None => message,
    }
}

/// Classify an HTTP response as success, a retryable failure, or a terminal failure.
///
/// 200 -> `Ok(())`, caller parses body as success. 429 -> [`ProviderError::RateLimited`] with
/// `retry_after` parsed from the `Retry-After` header. 500-599 -> [`ProviderError::Unavailable`].
/// 401/403 -> [`ProviderError::AuthFailed`]. Any other 4xx -> [`ProviderError::InvalidRequest`].
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
        let message = rate_limit_message(parse_error_message(body, "rate limited"), retry_after);
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

    let message = parse_error_message(body, &format!("request failed with status {status}"));
    Err(ProviderError::InvalidRequest(message))
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    /// Send `req` to the Messages API, retrying on 429/5xx up to `max_retries` times, honoring
    /// `Retry-After`.
    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let model = req.model_or(&self.model);
        let names = WireNames::for_request(&req);
        let wire_request = build_wire_request_with_names(&model, &req, &names);
        let url = format!("{}/v1/messages", self.base_url);
        let headers = build_headers(&self.api_key);

        let mut attempt: u32 = 0;
        loop {
            let started = self.clock.now();
            let response = self
                .http
                .post(&url)
                .headers(headers.clone())
                .json(&wire_request)
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
                    if err.is_retryable() && attempt < self.max_retries {
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
            let body = response.bytes().await.map_err(|e| {
                ProviderError::Unavailable(format!("failed to read response body: {e}"))
            })?;

            match classify_status(status, retry_after_header.as_deref(), &body) {
                Ok(()) => {
                    let finished = self.clock.now();
                    let latency =
                        Duration::from_secs(finished.seconds_since(started).max(0) as u64);
                    return parse_wire_response(&body, latency, finished)
                        .map(|c| names.decode_completion(c));
                }
                Err(err) => {
                    if err.is_retryable() && attempt < self.max_retries {
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

    /// Send `req` to the Embeddings-equivalent endpoint.
    ///
    /// Anthropic does not (as of this writing) ship a first-party embeddings endpoint under the
    /// Messages API; this method exists only to satisfy the [`Provider`] trait for a provider
    /// slug that a `providers.toml` might still route `embedder` to via a future endpoint.
    async fn embed(&self, _req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        Err(ProviderError::InvalidRequest(
            "anthropic provider does not support embeddings".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Candidate, ContentBlock, Message, MessageRole, StopReason, ToolDef, Usage};

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
            model: None,
        }
    }

    #[test]
    fn build_wire_request_maps_system_and_user_messages() {
        let model = ModelId::new("anthropic", "claude-sonnet-5");
        let wire = build_wire_request(&model, &sample_request());

        assert_eq!(wire.model, "claude-sonnet-5");
        assert_eq!(wire.system.as_deref(), Some("You are a helpful assistant."));
        assert_eq!(wire.messages.len(), 1);
        assert_eq!(wire.messages[0].role, "user");
        assert_eq!(wire.max_tokens, 1024);
        assert_eq!(wire.temperature, Some(0.0));
        assert_eq!(wire.stop_sequences, vec!["STOP".to_string()]);
        assert!(!wire.stream);
        assert_eq!(wire.tools.len(), 1);
        assert_eq!(wire.tools[0].name, "get_weather");
    }

    #[test]
    fn build_wire_request_concatenates_multiple_system_messages() {
        let model = ModelId::new("anthropic", "claude-sonnet-5");
        let mut req = sample_request();
        req.messages.insert(
            0,
            Message {
                role: MessageRole::System,
                content: vec![ContentBlock::Text {
                    text: "Extra system note.".to_string(),
                }],
            },
        );
        let wire = build_wire_request(&model, &req);
        assert_eq!(
            wire.system.as_deref(),
            Some("You are a helpful assistant.\n\nExtra system note.")
        );
        // The system-role message should not appear in `messages`.
        assert_eq!(wire.messages.len(), 1);
    }

    #[test]
    fn build_wire_request_ignores_n_and_always_produces_single_candidate_shape() {
        let model = ModelId::new("anthropic", "claude-sonnet-5");
        let mut req = sample_request();
        req.n = 5;
        let wire = build_wire_request(&model, &req);
        // WireRequest has no `n` field at all; nothing to assert beyond "it compiles and maps".
        assert_eq!(wire.messages.len(), 1);
    }

    #[test]
    fn build_wire_request_recurses_into_tool_result_content() {
        let model = ModelId::new("anthropic", "claude-sonnet-5");
        let mut req = sample_request();
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: vec![ContentBlock::Text {
                    text: "72F".to_string(),
                }],
                is_error: false,
            }],
        });
        let wire = build_wire_request(&model, &req);
        let last = wire.messages.last().expect("has appended message");
        match &last.content[0] {
            WireContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert!(!is_error);
                match &content[0] {
                    WireContentBlock::Text { text } => assert_eq!(text, "72F"),
                    other => panic!("expected text block, got {other:?}"),
                }
            }
            other => panic!("expected tool result block, got {other:?}"),
        }
    }

    #[test]
    fn build_headers_includes_auth_version_and_cache_beta() {
        let headers = build_headers("sk-ant-test-key");
        assert_eq!(headers.get("x-api-key").unwrap(), "sk-ant-test-key");
        assert_eq!(headers.get("anthropic-version").unwrap(), ANTHROPIC_VERSION);
        assert_eq!(
            headers.get("anthropic-beta").unwrap(),
            "prompt-caching-2024-07-31"
        );
    }

    #[test]
    fn build_headers_never_leaks_a_key_it_cannot_encode_as_a_header_value() {
        // A key resolved through `EnvApiKey` that happens to contain a byte `HeaderValue`
        // rejects (a bare newline is the simplest one) must not have that value surface in the
        // resulting header map at all -- `build_headers` falls back to an empty header rather
        // than propagating an error, so there is no code path here that could format the
        // rejected key into an error message either.
        let var = "TM_ANTHROPIC_TEST_HEADER_LEAK_CANARY";
        std::env::set_var(var, "canary-value\nwith-a-newline");
        let cred = EnvApiKey::new(var)
            .resolve()
            .expect("var is set to an invalid-but-present value");
        std::env::remove_var(var);

        let headers = build_headers(cred.expose_secret());
        let value = headers
            .get("x-api-key")
            .expect("x-api-key is always inserted, even on the fallback path");
        assert_eq!(
            value, "",
            "invalid header bytes must fall back to empty, not leak"
        );
        assert!(!format!("{value:?}").contains("canary-value"));
    }

    const RECORDED_RESPONSE_TEXT: &str = r#"{
        "model": "claude-sonnet-5-20260101",
        "content": [{"type": "text", "text": "Hello there!"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 12,
            "output_tokens": 5,
            "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 0
        }
    }"#;

    const RECORDED_RESPONSE_TOOL_USE: &str = r#"{
        "model": "claude-sonnet-5-20260101",
        "content": [{"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {"city": "SF"}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 20, "output_tokens": 8}
    }"#;

    #[test]
    fn parse_wire_response_maps_text_content_and_usage() {
        let ts = tm_types::Timestamp::from_unix_seconds(1_700_000_000);
        let completion = parse_wire_response(
            RECORDED_RESPONSE_TEXT.as_bytes(),
            Duration::from_millis(250),
            ts,
        )
        .expect("parses recorded response");

        assert_eq!(
            completion.model,
            ModelId::new("anthropic", "claude-sonnet-5-20260101")
        );
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(
            completion.candidates[0],
            Candidate {
                content: vec![ContentBlock::Text {
                    text: "Hello there!".to_string()
                }],
                stop_reason: StopReason::EndTurn,
            }
        );
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

    #[test]
    fn parse_wire_response_maps_tool_use_stop_reason() {
        let ts = tm_types::Timestamp::EPOCH;
        let completion =
            parse_wire_response(RECORDED_RESPONSE_TOOL_USE.as_bytes(), Duration::ZERO, ts)
                .expect("parses recorded response");
        assert_eq!(completion.candidates[0].stop_reason, StopReason::ToolUse);
        match &completion.candidates[0].content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_01");
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("expected tool use block, got {other:?}"),
        }
    }

    #[test]
    fn parse_wire_response_rejects_invalid_json() {
        let ts = tm_types::Timestamp::EPOCH;
        let err = parse_wire_response(b"not json", Duration::ZERO, ts).unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn parse_wire_response_rejects_unknown_stop_reason() {
        let ts = tm_types::Timestamp::EPOCH;
        let body = r#"{
            "model": "claude-sonnet-5",
            "content": [],
            "stop_reason": "something_new",
            "usage": {"input_tokens": 0, "output_tokens": 0}
        }"#;
        let err = parse_wire_response(body.as_bytes(), Duration::ZERO, ts).unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn classify_status_ok_on_200() {
        assert!(classify_status(reqwest::StatusCode::OK, None, b"{}").is_ok());
    }

    #[test]
    fn classify_status_rate_limited_with_retry_after_seconds() {
        let err = classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some("30"),
            br#"{"error": {"type": "rate_limit_error", "message": "slow down"}}"#,
        )
        .unwrap_err();
        match err {
            ProviderError::RateLimited {
                message,
                retry_after,
            } => {
                assert_eq!(message, "slow down Retry after 30 seconds.");
                assert_eq!(retry_after, Some(Duration::from_secs(30)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_rate_limited_without_retry_after() {
        let err = classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, None, b"{}").unwrap_err();
        match err {
            ProviderError::RateLimited { retry_after, .. } => assert_eq!(retry_after, None),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_server_error_is_unavailable() {
        let err = classify_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            None,
            br#"{"error": {"type": "overloaded_error", "message": "overloaded"}}"#,
        )
        .unwrap_err();
        match err {
            ProviderError::Unavailable(message) => assert_eq!(message, "overloaded"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_401_is_auth_failed() {
        let err = classify_status(
            reqwest::StatusCode::UNAUTHORIZED,
            None,
            br#"{"error": {"type": "authentication_error", "message": "invalid key"}}"#,
        )
        .unwrap_err();
        match err {
            ProviderError::AuthFailed(message) => assert_eq!(message, "invalid key"),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_403_is_auth_failed() {
        let err = classify_status(reqwest::StatusCode::FORBIDDEN, None, b"{}").unwrap_err();
        assert!(matches!(err, ProviderError::AuthFailed(_)));
    }

    #[test]
    fn classify_status_generic_400_is_invalid_request() {
        let err = classify_status(
            reqwest::StatusCode::BAD_REQUEST,
            None,
            br#"{"error": {"type": "invalid_request_error", "message": "bad field"}}"#,
        )
        .unwrap_err();
        match err {
            ProviderError::InvalidRequest(message) => assert_eq!(message, "bad field"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_falls_back_to_status_text_on_unparseable_error_body() {
        let err = classify_status(reqwest::StatusCode::BAD_REQUEST, None, b"not json").unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn with_config_builds_a_provider_without_touching_the_environment() {
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(tm_types::SystemClock);
        let provider = AnthropicProvider::with_config(
            ModelId::new("anthropic", "claude-sonnet-5"),
            "test-key".to_string(),
            "https://example.invalid".to_string(),
            clock,
        )
        .expect("builds provider");
        assert_eq!(provider.id(), "anthropic");
        assert_eq!(provider.max_retries, DEFAULT_MAX_RETRIES);
    }

    #[test]
    fn set_max_retries_overrides_the_default() {
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(tm_types::SystemClock);
        let mut provider = AnthropicProvider::with_config(
            ModelId::new("anthropic", "claude-sonnet-5"),
            "test-key".to_string(),
            "https://example.invalid".to_string(),
            clock,
        )
        .expect("builds provider");
        provider.set_max_retries(7);
        assert_eq!(provider.max_retries, 7);
    }

    fn dotted_tool_request() -> CompletionRequest {
        let mut req = sample_request();
        req.tools = ["fs.read", "fs_read", "shell.run"]
            .iter()
            .map(|n| ToolDef {
                name: n.to_string(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            })
            .collect();
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_01".to_string(),
                name: "fs.read".to_string(),
                input: serde_json::json!({"path": "a"}),
            }],
        });
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_01".to_string(),
                content: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
                is_error: false,
            }],
        });
        req
    }

    #[test]
    fn build_wire_request_sends_only_valid_tool_names_consistent_with_history() {
        let model = ModelId::new("anthropic", "claude-sonnet-5");
        let wire = build_wire_request(&model, &dotted_tool_request());
        let tool_names: Vec<&str> = wire.tools.iter().map(|t| t.name.as_str()).collect();
        for name in &tool_names {
            assert!(crate::wire_names::is_valid_wire_name(name), "{name}");
        }
        assert_eq!(tool_names, vec!["fs_read_2", "fs_read", "shell_run"]);
        let history_name = wire
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                WireContentBlock::ToolUse { name, .. } => Some(name.clone()),
                _ => None,
            })
            .expect("history tool use");
        assert_eq!(history_name, "fs_read_2");
        let body = serde_json::to_string(&wire).expect("serializes");
        assert!(
            !body.contains("fs.read") && !body.contains("shell.run"),
            "{body}"
        );
    }

    #[test]
    fn build_wire_request_marks_only_the_last_tool_cacheable() {
        let model = ModelId::new("anthropic", "claude-sonnet-5");
        let wire = build_wire_request(&model, &dotted_tool_request());
        assert_eq!(wire.tools.len(), 3, "fixture has three tools");
        for tool in &wire.tools[..wire.tools.len() - 1] {
            assert!(
                tool.cache_control.is_none(),
                "only the last tool should carry a cache breakpoint, found one on {}",
                tool.name
            );
        }
        let last = wire.tools.last().expect("at least one tool");
        assert!(
            last.cache_control.is_some(),
            "the last tool should carry the prompt-cache breakpoint"
        );
        let body = serde_json::to_string(&wire).expect("serializes");
        assert_eq!(
            body.matches("cache_control").count(),
            1,
            "exactly one cache_control breakpoint should be on the wire: {body}"
        );
    }

    #[test]
    fn a_response_tool_call_maps_back_to_the_dotted_name() {
        let model = ModelId::new("anthropic", "claude-sonnet-5");
        let req = dotted_tool_request();
        let names = WireNames::for_request(&req);
        let _wire = build_wire_request_with_names(&model, &req, &names);
        let body = r#"{
            "model": "claude-sonnet-5",
            "content": [
                {"type": "tool_use", "id": "toolu_02", "name": "fs_read_2", "input": {}},
                {"type": "tool_use", "id": "toolu_03", "name": "fs_read", "input": {}},
                {"type": "tool_use", "id": "toolu_04", "name": "shell_run", "input": {}},
                {"type": "tool_use", "id": "toolu_05", "name": "invented", "input": {}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }"#;
        let completion = names.decode_completion(
            parse_wire_response(body.as_bytes(), Duration::ZERO, tm_types::Timestamp::EPOCH)
                .expect("parses"),
        );
        let called: Vec<&str> = completion.candidates[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(called, vec!["fs.read", "fs_read", "shell.run", "invented"]);
    }
}
