//! Google Gemini, over the `generativelanguage.googleapis.com` REST API — **not** built on
//! [`crate::providers::compat`], since Gemini's request/response shape is its own (`contents` /
//! `parts` / `functionCall` / `functionResponse`), not OpenAI Chat Completions. This module owns
//! its own wire shapes end to end, following the pattern [`crate::anthropic`] set: wire structs
//! private to this file, pure translation functions, its own retry/error-mapping copied in spirit
//! (not by importing `compat`'s private helpers) from `compat.rs`/`anthropic.rs`.
//!
//! Gemini also exposes an OpenAI-compatible shim (`/v1beta/openai/chat/completions`) that could in
//! principle reuse [`crate::providers::compat`] wholesale. This module deliberately speaks the
//! native API instead: the shim only surfaces a lossy subset of Gemini's actual feature set, most
//! notably it has no way to set a thinking-token budget, which the native `generationConfig`
//! exposes directly. Paying for the extra wire shapes here buys full feature access.
//!
//! Env vars read by [`GeminiProvider::from_env`] (each via [`tm_auth::EnvApiKey`], `SPEC.md`
//! §28.2's auth-adapter layer, rather than a bare `std::env::var` call):
//! - `GEMINI_API_KEY` (required unless `GOOGLE_API_KEY` is set instead — check `GEMINI_API_KEY`
//!   first, fall back to `GOOGLE_API_KEY`, since both names are in common use).
//! - `GEMINI_BASE_URL` (optional, default `"https://generativelanguage.googleapis.com/v1beta"`).
//!
//! Auth: send the key as the `x-goog-api-key` header (Gemini also accepts a `?key=` query
//! parameter; prefer the header so the key never lands in logged URLs).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_auth::EnvApiKey;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::{missing_env_var, Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Candidate, Completion, CompletionRequest, ContentBlock, EmbedRequest, Embeddings, MessageRole,
    ModelId, ProviderError, StopReason, Usage,
};
use crate::wire_names::WireNames;

/// Default Generative Language API base URL.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Default maximum retry attempts on 429/5xx before giving up.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Default request timeout for the underlying HTTP client.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Floor for exponential backoff when the provider gives no `Retry-After`.
const BACKOFF_FLOOR: Duration = Duration::from_millis(500);

/// Google Gemini, via the Generative Language API. See the module docs for the exact wire shapes
/// implemented (Gemini is not OpenAI-Chat-Completions-shaped, so this does not build on
/// [`crate::providers::compat`]).
pub struct GeminiProvider {
    id: String,
    model: ModelId,
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    max_retries: u32,
}

impl GeminiProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let api_key = EnvApiKey::new("GEMINI_API_KEY")
            .resolve()
            .or_else(|_| EnvApiKey::new("GOOGLE_API_KEY").resolve())
            .map_err(|_| missing_env_var("GEMINI_API_KEY"))?
            .expose_secret()
            .to_string();
        let base_url =
            std::env::var("GEMINI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self::with_config(model, api_key, base_url, clock)
    }

    /// Build a provider with an explicit key and base URL, for tests that stand up a local mock
    /// HTTP server (shaping tests use recorded JSON directly and never need this, but it's here
    /// for completeness / future integration tests outside this crate).
    pub fn with_config(
        model: ModelId,
        api_key: String,
        base_url: String,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Unavailable(format!("failed to build HTTP client: {e}")))?;
        Ok(GeminiProvider {
            id: "gemini".to_string(),
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

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "gemini",
            display_name: "Google Gemini",
            env_vars: &[
                EnvVarRequirement {
                    name: "GEMINI_API_KEY",
                    required: true,
                    description: "API key (falls back to GOOGLE_API_KEY if unset)",
                },
                EnvVarRequirement {
                    name: "GOOGLE_API_KEY",
                    required: false,
                    description: "Fallback name for GEMINI_API_KEY",
                },
                EnvVarRequirement {
                    name: "GEMINI_BASE_URL",
                    required: false,
                    description:
                        "Override the default https://generativelanguage.googleapis.com/v1beta",
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

// ---- wire shapes -----------------------------------------------------------------------------
//
// These mirror the Generative Language API JSON exactly (camelCase field names) and exist only
// to (de)serialize at the HTTP boundary; `crate::types` is the provider-independent shape used
// everywhere else in the workspace.

/// The `generateContent`/`streamGenerateContent` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireRequest {
    /// System prompt, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<WireContent>,
    /// Conversation turns.
    pub contents: Vec<WireContent>,
    /// Tool definitions available to the model.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<WireTool>,
    /// Generation parameters.
    pub generation_config: WireGenerationConfig,
}

/// One `generationConfig` block on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireGenerationConfig {
    /// Max tokens to generate.
    pub max_output_tokens: u32,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub stop_sequences: Vec<String>,
}

/// One conversation turn (or the system instruction) on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireContent {
    /// `"user"` or `"model"` (there is no wire `"system"` role; absent on `systemInstruction`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The turn's parts, in order.
    pub parts: Vec<WirePart>,
}

/// One content part on the wire. Untagged: Gemini distinguishes these by which field is present,
/// not by a `type` discriminator, so this deserializes/serializes as a flat struct with optional
/// fields rather than an externally-tagged enum.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WirePart {
    /// Plain text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A model-issued function call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<WireFunctionCall>,
    /// The caller's answer to a prior function call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<WireFunctionResponse>,
}

/// A function call part on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFunctionCall {
    /// The function's name.
    pub name: String,
    /// The call's arguments, as a raw JSON object (not a JSON-encoded string).
    #[serde(default)]
    pub args: serde_json::Value,
}

/// A function response part on the wire. Correlates to its call by `name` and turn order only —
/// Gemini has no call-id field here (unlike OpenAI's `tool_call_id` or Anthropic's
/// `tool_use_id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFunctionResponse {
    /// The function's name.
    pub name: String,
    /// The function's result, as a raw JSON object.
    pub response: serde_json::Value,
}

/// A tool definition on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTool {
    /// The function declarations this tool exposes.
    pub function_declarations: Vec<WireFunctionDeclaration>,
}

/// One function declaration on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFunctionDeclaration {
    /// The tool's name.
    pub name: String,
    /// The tool's description.
    pub description: String,
    /// The tool's JSON Schema input shape.
    pub parameters: serde_json::Value,
}

/// The `generateContent` response body (also one SSE chunk's shape for `streamGenerateContent`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireResponse {
    /// Generated candidates.
    #[serde(default)]
    pub candidates: Vec<WireCandidate>,
    /// Token usage, present on the final chunk of a stream and on non-streaming responses.
    #[serde(default)]
    pub usage_metadata: Option<WireUsageMetadata>,
    /// The model version that actually served the request, when Google echoes it back.
    #[serde(default)]
    pub model_version: Option<String>,
}

/// One candidate on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireCandidate {
    /// The candidate's content (or content delta, in a `streamGenerateContent` chunk).
    #[serde(default)]
    pub content: Option<WireContent>,
    /// Which candidate slot this is, for `n > 1`. Absent in practice for `n == 1` requests
    /// (every chunk in this crate's single-candidate requests omits it); treated as `0`.
    #[serde(default)]
    pub index: u32,
    /// Why generation stopped, e.g. `"STOP"`, `"MAX_TOKENS"`. Absent on intermediate stream
    /// chunks.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Usage on the wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireUsageMetadata {
    /// Input (prompt) tokens.
    #[serde(default)]
    pub prompt_token_count: u32,
    /// Output (candidates) tokens.
    #[serde(default)]
    pub candidates_token_count: u32,
    /// Input tokens served from the context cache.
    #[serde(default)]
    pub cached_content_token_count: u32,
}

/// The Generative Language API's error envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireErrorEnvelope {
    /// The error detail.
    pub error: WireErrorDetail,
}

/// One error detail on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireErrorDetail {
    /// The HTTP-status-shaped integer code Google duplicates into the body.
    #[serde(default)]
    pub code: i32,
    /// A human-readable message.
    pub message: String,
    /// The gRPC-style status string, e.g. `"RESOURCE_EXHAUSTED"`.
    #[serde(default)]
    pub status: String,
}

/// The `batchEmbedContents` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireEmbedBatchRequest {
    /// One request per input.
    pub requests: Vec<WireEmbedRequest>,
}

/// One entry in a [`WireEmbedBatchRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireEmbedRequest {
    /// The model, as `"models/{model}"`.
    pub model: String,
    /// The text to embed.
    pub content: WireContent,
}

/// The `batchEmbedContents` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireEmbedBatchResponse {
    /// One embedding per request, in order.
    pub embeddings: Vec<WireEmbedding>,
}

/// One embedding on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireEmbedding {
    /// The embedding vector.
    pub values: Vec<f32>,
}

// ---- shaping (pure, unit-tested against recorded JSON) --------------------------------------

/// Turn a single [`crate::types::ContentBlock`] into its wire [`WirePart`], resolving a
/// [`ContentBlock::ToolResult::tool_use_id`] to a function name via `tool_names` (a map built by
/// the caller from every [`ContentBlock::ToolUse`] seen so far in the conversation, since Gemial's
/// `functionResponse` has no id field to carry the correlation itself).
fn content_block_to_wire_part(
    block: &ContentBlock,
    tool_names: &std::collections::HashMap<String, String>,
) -> WirePart {
    match block {
        ContentBlock::Text { text } => WirePart {
            text: Some(text.clone()),
            ..Default::default()
        },
        ContentBlock::ToolUse { name, input, .. } => WirePart {
            function_call: Some(WireFunctionCall {
                name: name.clone(),
                args: input.clone(),
            }),
            ..Default::default()
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            let name = tool_names
                .get(tool_use_id)
                .cloned()
                .unwrap_or_else(|| tool_use_id.clone());
            // functionResponse.response is a single JSON object; concatenate any text blocks
            // (the common case is exactly one) since there is no wire slot for a list of parts.
            let text: String = content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            WirePart {
                function_response: Some(WireFunctionResponse {
                    name,
                    response: serde_json::json!({ "result": text }),
                }),
                ..Default::default()
            }
        }
    }
}

fn wire_part_to_content_block(part: &WirePart) -> Option<ContentBlock> {
    if let Some(text) = &part.text {
        return Some(ContentBlock::Text { text: text.clone() });
    }
    if let Some(call) = &part.function_call {
        // Gemini issues no call id on the wire; synthesize one so downstream `ContentBlock`
        // consumers (which key tool results off `id`) have something stable to correlate on.
        return Some(ContentBlock::ToolUse {
            id: format!("call_{}", call.name),
            name: call.name.clone(),
            input: call.args.clone(),
        });
    }
    None
}

/// Build the wire request body for `req` against `model`.
///
/// Maps [`MessageRole::System`] text into [`WireRequest::system_instruction`] (concatenated with
/// `\n\n` if more than one, matching `crate::anthropic`'s handling); maps `User` -> `role: "user"`
/// and `Assistant` -> `role: "model"`. Walks messages in order to build the `tool_use_id -> name`
/// map a following `ToolResult` needs, since Gemini's `functionResponse` correlates by name only.
///
/// Tool names go on the wire through [`WireNames::for_request`], like every other real provider.
/// Gemini itself would accept tm's dotted names (its rule allows `.` and `:`), but it also
/// requires a leading letter or underscore and caps the length, and one mapping rule for every
/// provider is simpler to reason about than a per-provider exception: already-valid names pass
/// through unchanged, so the map only ever touches names some provider would reject. The name
/// map is applied before the `tool_use_id -> name` walk below, so a `functionResponse` carries
/// the same wire name as the `functionCall` it answers.
pub fn build_wire_request(model: &ModelId, req: &CompletionRequest) -> WireRequest {
    build_wire_request_with_names(model, req, &WireNames::for_request(req))
}

/// [`build_wire_request`] with an explicit name map, so [`GeminiProvider::complete`] can decode
/// the response (streamed or not) with the same one.
pub fn build_wire_request_with_names(
    model: &ModelId,
    req: &CompletionRequest,
    names: &WireNames,
) -> WireRequest {
    let req = names.encode_request(req);
    let req = req.as_ref();
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(system) = &req.system {
        system_parts.push(system.clone());
    }

    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut contents = Vec::with_capacity(req.messages.len());
    for msg in &req.messages {
        match msg.role {
            MessageRole::System => {
                for block in &msg.content {
                    if let ContentBlock::Text { text } = block {
                        system_parts.push(text.clone());
                    }
                }
            }
            MessageRole::User | MessageRole::Assistant => {
                for block in &msg.content {
                    if let ContentBlock::ToolUse { id, name, .. } = block {
                        tool_names.insert(id.clone(), name.clone());
                    }
                }
                let role = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "model",
                    MessageRole::System => unreachable!("system handled above"),
                };
                contents.push(WireContent {
                    role: Some(role.to_string()),
                    parts: msg
                        .content
                        .iter()
                        .map(|b| content_block_to_wire_part(b, &tool_names))
                        .collect(),
                });
            }
        }
    }

    let system_instruction = if system_parts.is_empty() {
        None
    } else {
        Some(WireContent {
            role: None,
            parts: vec![WirePart {
                text: Some(system_parts.join("\n\n")),
                ..Default::default()
            }],
        })
    };

    let tools = if req.tools.is_empty() {
        Vec::new()
    } else {
        vec![WireTool {
            function_declarations: req
                .tools
                .iter()
                .map(|t| WireFunctionDeclaration {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                })
                .collect(),
        }]
    };

    WireRequest {
        system_instruction,
        contents,
        tools,
        generation_config: WireGenerationConfig {
            max_output_tokens: req.max_tokens,
            temperature: req.temperature,
            stop_sequences: req.stop_sequences.clone(),
        },
    }
    .tap_model(model)
}

// `WireRequest` carries no `model` field of its own (Gemini puts the model in the URL path, not
// the body); this no-op adapter exists only so `build_wire_request`'s signature stays symmetric
// with `crate::anthropic::build_wire_request`, which callers may reasonably expect.
impl WireRequest {
    fn tap_model(self, _model: &ModelId) -> Self {
        self
    }
}

fn map_finish_reason(
    reason: &str,
    content: &Option<WireContent>,
) -> Result<StopReason, ProviderError> {
    let is_tool_call = content
        .as_ref()
        .map(|c| !c.parts.is_empty() && c.parts.iter().all(|p| p.function_call.is_some()))
        .unwrap_or(false);
    if is_tool_call {
        return Ok(StopReason::ToolUse);
    }
    match reason {
        "STOP" => Ok(StopReason::EndTurn),
        "MAX_TOKENS" => Ok(StopReason::MaxTokens),
        other => Err(ProviderError::MalformedResponse(format!(
            "unsupported finishReason: {other}"
        ))),
    }
}

/// Parse a successful `generateContent` response body into a provider-independent [`Completion`].
pub fn parse_wire_response(
    body: &[u8],
    model: &ModelId,
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError> {
    let wire: WireResponse = serde_json::from_slice(body).map_err(|e| {
        ProviderError::MalformedResponse(format!("failed to parse Gemini response: {e}"))
    })?;

    let candidate = wire.candidates.first().ok_or_else(|| {
        ProviderError::MalformedResponse("Gemini response had no candidates".to_string())
    })?;

    let finish_reason = candidate.finish_reason.as_deref().ok_or_else(|| {
        ProviderError::MalformedResponse("candidate had no finishReason".to_string())
    })?;
    let stop_reason = map_finish_reason(finish_reason, &candidate.content)?;

    let content = candidate
        .content
        .as_ref()
        .map(|c| {
            c.parts
                .iter()
                .filter_map(wire_part_to_content_block)
                .collect()
        })
        .unwrap_or_default();

    let usage = wire.usage_metadata.unwrap_or_default();

    Ok(Completion {
        model: ModelId::new(
            &model.provider,
            wire.model_version.unwrap_or_else(|| model.model.clone()),
        ),
        candidates: vec![Candidate {
            content,
            stop_reason,
        }],
        usage: Usage {
            input_tokens: usage.prompt_token_count,
            output_tokens: usage.candidates_token_count,
            cache_read_tokens: usage.cached_content_token_count,
            cache_write_tokens: 0,
        },
        latency,
        received_at,
    })
}

// ---- streaming: SSE parsing + reassembly ------------------------------------------------------

/// Split a fully-buffered `streamGenerateContent?alt=sse` response body into its `data:` events,
/// each parsed as a [`WireResponse`] (Gemini's streaming chunks reuse the same top-level shape as
/// the non-streaming response, unlike `compat`'s delta-shaped `WireStreamChunk`). Follows
/// [`crate::providers::compat::parse_sse_body`]'s pattern of splitting on blank lines and
/// stripping the `"data:"` prefix; Gemini has no `[DONE]` sentinel, so there is nothing to skip
/// besides blank keep-alive lines.
pub fn parse_sse_body(body: &[u8]) -> Result<Vec<WireResponse>, ProviderError> {
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
            if data.is_empty() {
                continue;
            }
            let chunk: WireResponse = serde_json::from_str(data).map_err(|e| {
                ProviderError::MalformedResponse(format!("failed to parse SSE chunk: {e}"))
            })?;
            chunks.push(chunk);
        }
    }
    Ok(chunks)
}

/// Reassemble a sequence of [`WireResponse`] SSE chunks (in arrival order) into the same
/// [`Completion`] shape [`parse_wire_response`] produces from a non-streaming response: text
/// parts are concatenated per candidate index, `functionCall`/`functionResponse` parts arrive
/// whole in a single chunk (Gemini does not fragment them the way OpenAI fragments tool-call
/// argument strings) so they are appended as-is, and the last chunk carrying a `finishReason` or
/// `usageMetadata` wins.
pub fn assemble_streamed_completion(
    chunks: &[WireResponse],
    model: &ModelId,
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct CandidateAccumulator {
        text: String,
        non_text_parts: Vec<WirePart>,
        finish_reason: Option<String>,
    }

    let mut candidates: BTreeMap<u32, CandidateAccumulator> = BTreeMap::new();
    let mut usage: Option<WireUsageMetadata> = None;
    let mut model_version: Option<String> = None;

    for chunk in chunks {
        if chunk.usage_metadata.is_some() {
            usage = chunk.usage_metadata.clone();
        }
        if chunk.model_version.is_some() {
            model_version = chunk.model_version.clone();
        }
        for wire_candidate in &chunk.candidates {
            let accum = candidates.entry(wire_candidate.index).or_default();
            if let Some(content) = &wire_candidate.content {
                for part in &content.parts {
                    if let Some(text) = &part.text {
                        accum.text.push_str(text);
                    } else {
                        accum.non_text_parts.push(part.clone());
                    }
                }
            }
            if let Some(reason) = &wire_candidate.finish_reason {
                accum.finish_reason = Some(reason.clone());
            }
        }
    }

    if candidates.is_empty() {
        return Err(ProviderError::MalformedResponse(
            "stream produced no candidates".to_string(),
        ));
    }

    let mut out_candidates = Vec::with_capacity(candidates.len());
    for (_, accum) in candidates {
        let mut parts: Vec<WirePart> = Vec::new();
        if !accum.text.is_empty() {
            parts.push(WirePart {
                text: Some(accum.text),
                ..Default::default()
            });
        }
        parts.extend(accum.non_text_parts);
        let content_for_reason = Some(WireContent {
            role: Some("model".to_string()),
            parts: parts.clone(),
        });
        let finish_reason = accum.finish_reason.ok_or_else(|| {
            ProviderError::MalformedResponse("stream ended without a finishReason".to_string())
        })?;
        let stop_reason = map_finish_reason(&finish_reason, &content_for_reason)?;
        out_candidates.push(Candidate {
            content: parts
                .iter()
                .filter_map(wire_part_to_content_block)
                .collect(),
            stop_reason,
        });
    }

    let usage = usage.unwrap_or_default();

    Ok(Completion {
        model: ModelId::new(
            &model.provider,
            model_version.unwrap_or_else(|| model.model.clone()),
        ),
        candidates: out_candidates,
        usage: Usage {
            input_tokens: usage.prompt_token_count,
            output_tokens: usage.candidates_token_count,
            cache_read_tokens: usage.cached_content_token_count,
            cache_write_tokens: 0,
        },
        latency,
        received_at,
    })
}

/// Parse a successful body (SSE chunks when `stream`, one JSON response otherwise) and map every
/// tool call's wire name back through `names`, the map [`build_wire_request_with_names`] sent the
/// request with. [`GeminiProvider::complete`]'s whole response path, kept pure so both branches
/// are tested without a network.
pub fn parse_completion_body(
    body: &[u8],
    stream: bool,
    model: &ModelId,
    latency: Duration,
    received_at: tm_types::Timestamp,
    names: &WireNames,
) -> Result<Completion, ProviderError> {
    let completion = if stream {
        let chunks = parse_sse_body(body)?;
        assemble_streamed_completion(&chunks, model, latency, received_at)
    } else {
        parse_wire_response(body, model, latency, received_at)
    }?;
    Ok(names.decode_completion(completion))
}

/// Parse a `Retry-After` header value as an integer number of seconds. An HTTP-date value (RFC
/// 7231's alternate form) is not handled, matching `crate::anthropic::parse_retry_after`.
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
/// 200 -> `Ok(())`. 429 -> [`ProviderError::RateLimited`] with `retry_after` parsed from the
/// `Retry-After` header. 500-599 -> [`ProviderError::Unavailable`]. 401/403 ->
/// [`ProviderError::AuthFailed`]. Any other 4xx -> [`ProviderError::InvalidRequest`]. `code`/
/// `status` in the JSON body duplicate the real HTTP status per Google's convention, so this
/// classifies by the actual HTTP status, matching [`crate::anthropic::classify_status`].
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

    let message = parse_error_message(body, &format!("request failed with status {status}"));
    Err(ProviderError::InvalidRequest(message))
}

fn build_headers(api_key: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "x-goog-api-key",
        reqwest::header::HeaderValue::from_str(api_key)
            .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
    );
    headers
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    /// Send `req` to `generateContent`, or to `streamGenerateContent?alt=sse` and reassemble the
    /// SSE chunks when `req.stream` is set — matching every other provider in this crate (see
    /// `compat.rs` module docs), [`Provider::complete`] always returns one fully-assembled
    /// [`Completion`] regardless of which endpoint served it. Retries on 429/5xx up to
    /// `max_retries` times, honoring `Retry-After`.
    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let streaming = req.stream;
        let model = req.model_or(&self.model);
        let names = WireNames::for_request(&req);
        let wire_request = build_wire_request_with_names(&model, &req, &names);
        let url = if streaming {
            format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.base_url, model.model
            )
        } else {
            format!("{}/models/{}:generateContent", self.base_url, model.model)
        };
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
                    return parse_completion_body(
                        &body, streaming, &model, latency, finished, &names,
                    );
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

    /// Send `req` to `batchEmbedContents`, used unconditionally (even for a single input) to keep
    /// this module's request path uniform.
    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let wire_request = WireEmbedBatchRequest {
            requests: req
                .inputs
                .iter()
                .map(|text| WireEmbedRequest {
                    model: format!("models/{}", self.model.model),
                    content: WireContent {
                        role: None,
                        parts: vec![WirePart {
                            text: Some(text.clone()),
                            ..Default::default()
                        }],
                    },
                })
                .collect(),
        };

        let url = format!(
            "{}/models/{}:batchEmbedContents",
            self.base_url, self.model.model
        );
        let headers = build_headers(&self.api_key);

        let mut attempt: u32 = 0;
        loop {
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
                    let wire: WireEmbedBatchResponse =
                        serde_json::from_slice(&body).map_err(|e| {
                            ProviderError::MalformedResponse(format!(
                                "failed to parse Gemini embed response: {e}"
                            ))
                        })?;
                    return Ok(Embeddings {
                        model: self.model.clone(),
                        vectors: wire.embeddings.into_iter().map(|e| e.values).collect(),
                        usage: Usage::default(),
                    });
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, MessageRole, ToolDef};

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
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let wire = build_wire_request(&model, &sample_request());

        let system = wire.system_instruction.expect("has system instruction");
        assert_eq!(
            system.parts[0].text.as_deref(),
            Some("You are a helpful assistant.")
        );
        assert_eq!(wire.contents.len(), 1);
        assert_eq!(wire.contents[0].role.as_deref(), Some("user"));
        assert_eq!(wire.generation_config.max_output_tokens, 1024);
        assert_eq!(wire.generation_config.temperature, Some(0.0));
        assert_eq!(
            wire.generation_config.stop_sequences,
            vec!["STOP".to_string()]
        );
        assert_eq!(wire.tools.len(), 1);
        assert_eq!(wire.tools[0].function_declarations[0].name, "get_weather");
    }

    #[test]
    fn build_wire_request_maps_assistant_role_to_model() {
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let mut req = sample_request();
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "Hi there".to_string(),
            }],
        });
        let wire = build_wire_request(&model, &req);
        assert_eq!(wire.contents[1].role.as_deref(), Some("model"));
    }

    #[test]
    fn build_wire_request_concatenates_multiple_system_messages() {
        let model = ModelId::new("gemini", "gemini-2.5-pro");
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
            wire.system_instruction.unwrap().parts[0].text.as_deref(),
            Some("You are a helpful assistant.\n\nExtra system note.")
        );
        assert_eq!(wire.contents.len(), 1);
    }

    #[test]
    fn build_wire_request_maps_tool_use_to_function_call() {
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let mut req = sample_request();
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
            }],
        });
        let wire = build_wire_request(&model, &req);
        let call = wire.contents[1].parts[0]
            .function_call
            .as_ref()
            .expect("has function call");
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.args["city"], "SF");
    }

    #[test]
    fn build_wire_request_resolves_tool_result_name_from_preceding_tool_use() {
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let mut req = sample_request();
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({"city": "SF"}),
            }],
        });
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
        let response = wire.contents[2].parts[0]
            .function_response
            .as_ref()
            .expect("has function response");
        assert_eq!(response.name, "get_weather");
        assert_eq!(response.response["result"], "72F");
    }

    const RECORDED_RESPONSE_TEXT: &str = r#"{
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Hello there!"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 12,
            "candidatesTokenCount": 5,
            "cachedContentTokenCount": 3
        },
        "modelVersion": "gemini-2.5-pro-001"
    }"#;

    const RECORDED_RESPONSE_TOOL_USE: &str = r#"{
        "candidates": [{
            "content": {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "SF"}}}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 8}
    }"#;

    #[test]
    fn parse_wire_response_maps_text_content_and_usage() {
        let ts = tm_types::Timestamp::from_unix_seconds(1_700_000_000);
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let completion = parse_wire_response(
            RECORDED_RESPONSE_TEXT.as_bytes(),
            &model,
            Duration::from_millis(250),
            ts,
        )
        .expect("parses recorded response");

        assert_eq!(
            completion.model,
            ModelId::new("gemini", "gemini-2.5-pro-001")
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
    fn parse_wire_response_detects_tool_use_from_content_shape() {
        let ts = tm_types::Timestamp::EPOCH;
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let completion = parse_wire_response(
            RECORDED_RESPONSE_TOOL_USE.as_bytes(),
            &model,
            Duration::ZERO,
            ts,
        )
        .expect("parses recorded response");
        assert_eq!(completion.candidates[0].stop_reason, StopReason::ToolUse);
        match &completion.candidates[0].content[0] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("expected tool use block, got {other:?}"),
        }
    }

    #[test]
    fn parse_wire_response_rejects_invalid_json() {
        let ts = tm_types::Timestamp::EPOCH;
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let err = parse_wire_response(b"not json", &model, Duration::ZERO, ts).unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn parse_wire_response_rejects_unsupported_finish_reason() {
        let ts = tm_types::Timestamp::EPOCH;
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let body = r#"{
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "..."}]},
                "finishReason": "SAFETY"
            }]
        }"#;
        let err = parse_wire_response(body.as_bytes(), &model, Duration::ZERO, ts).unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn parse_wire_response_rejects_empty_candidates() {
        let ts = tm_types::Timestamp::EPOCH;
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let err =
            parse_wire_response(br#"{"candidates": []}"#, &model, Duration::ZERO, ts).unwrap_err();
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
            br#"{"error": {"code": 429, "message": "slow down", "status": "RESOURCE_EXHAUSTED"}}"#,
        )
        .unwrap_err();
        match err {
            ProviderError::RateLimited {
                message,
                retry_after,
            } => {
                assert_eq!(message, "slow down");
                assert_eq!(retry_after, Some(Duration::from_secs(30)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_server_error_is_unavailable() {
        let err = classify_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            None,
            br#"{"error": {"code": 500, "message": "overloaded", "status": "INTERNAL"}}"#,
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
            br#"{"error": {"code": 401, "message": "invalid key", "status": "UNAUTHENTICATED"}}"#,
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
            br#"{"error": {"code": 400, "message": "bad field", "status": "INVALID_ARGUMENT"}}"#,
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
    fn parse_sse_body_parses_multiple_data_events() {
        let body = concat!(
            "data: {\"candidates\": [{\"content\": {\"role\": \"model\", \"parts\": [{\"text\": \"Hel\"}]}}]}\n\n",
            "data: {\"candidates\": [{\"content\": {\"role\": \"model\", \"parts\": [{\"text\": \"lo\"}]}, \"finishReason\": \"STOP\"}], \"usageMetadata\": {\"promptTokenCount\": 4, \"candidatesTokenCount\": 2}}\n\n",
        );
        let chunks = parse_sse_body(body.as_bytes()).expect("parses");
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn assemble_streamed_completion_merges_text_deltas_across_chunks() {
        let chunks = vec![
            serde_json::from_str::<WireResponse>(
                r#"{"candidates": [{"content": {"role": "model", "parts": [{"text": "Hel"}]}, "index": 0}]}"#,
            )
            .expect("parses chunk"),
            serde_json::from_str::<WireResponse>(
                r#"{"candidates": [{"content": {"role": "model", "parts": [{"text": "lo!"}]}, "index": 0, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 2}}"#,
            )
            .expect("parses chunk"),
        ];
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let completion = assemble_streamed_completion(
            &chunks,
            &model,
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .expect("assembles");
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(
            completion.candidates[0],
            Candidate {
                content: vec![ContentBlock::Text {
                    text: "Hello!".to_string()
                }],
                stop_reason: StopReason::EndTurn,
            }
        );
        assert_eq!(completion.usage.input_tokens, 4);
        assert_eq!(completion.usage.output_tokens, 2);
    }

    #[test]
    fn assemble_streamed_completion_rejects_empty_stream() {
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let err =
            assemble_streamed_completion(&[], &model, Duration::ZERO, tm_types::Timestamp::EPOCH)
                .unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn parse_embed_batch_response_maps_vectors_in_order() {
        let body = br#"{"embeddings": [{"values": [0.1, 0.2]}, {"values": [0.3, 0.4]}]}"#;
        let wire: WireEmbedBatchResponse = serde_json::from_slice(body).expect("parses");
        assert_eq!(wire.embeddings.len(), 2);
        assert_eq!(wire.embeddings[0].values, vec![0.1, 0.2]);
        assert_eq!(wire.embeddings[1].values, vec![0.3, 0.4]);
    }

    #[test]
    fn build_headers_sets_goog_api_key() {
        let headers = build_headers("test-key");
        assert_eq!(headers.get("x-goog-api-key").unwrap(), "test-key");
    }

    #[test]
    fn with_config_builds_a_provider_without_touching_the_environment() {
        let clock: Arc<dyn Clock> = Arc::new(tm_types::SystemClock);
        let provider = GeminiProvider::with_config(
            ModelId::new("gemini", "gemini-2.5-pro"),
            "test-key".to_string(),
            "https://example.invalid".to_string(),
            clock,
        )
        .expect("builds provider");
        assert_eq!(provider.id(), "gemini");
        assert_eq!(provider.max_retries, DEFAULT_MAX_RETRIES);
    }

    #[test]
    fn set_max_retries_overrides_the_default() {
        let clock: Arc<dyn Clock> = Arc::new(tm_types::SystemClock);
        let mut provider = GeminiProvider::with_config(
            ModelId::new("gemini", "gemini-2.5-pro"),
            "test-key".to_string(),
            "https://example.invalid".to_string(),
            clock,
        )
        .expect("builds provider");
        provider.set_max_retries(7);
        assert_eq!(provider.max_retries, 7);
    }

    #[test]
    fn info_declares_env_vars_and_capabilities() {
        let info = GeminiProvider::info();
        assert_eq!(info.id, "gemini");
        assert!(info.capabilities.completion);
        assert!(info.capabilities.embedding);
        assert!(info.capabilities.streaming);
        assert!(info.capabilities.tool_use);
        assert!(!info.capabilities.vision);
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "GEMINI_API_KEY" && v.required));
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "GOOGLE_API_KEY" && !v.required));
    }

    // ---- tool names on the wire ----

    fn dotted_tool_request(stream: bool) -> CompletionRequest {
        let mut req = sample_request();
        req.stream = stream;
        req.tools[0].name = "fs.read".to_string();
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_fs_read".to_string(),
                name: "fs.read".to_string(),
                input: serde_json::json!({"path": "a"}),
            }],
        });
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_fs_read".to_string(),
                content: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
                is_error: false,
            }],
        });
        req
    }

    #[test]
    fn build_wire_request_maps_declarations_calls_and_responses_to_one_wire_name() {
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let wire = build_wire_request(&model, &dotted_tool_request(false));
        assert_eq!(wire.tools[0].function_declarations[0].name, "fs_read");
        let call = wire.contents[1].parts[0]
            .function_call
            .as_ref()
            .expect("has function call");
        assert_eq!(call.name, "fs_read");
        // The functionResponse name is resolved from the (already renamed) preceding call.
        let response = wire.contents[2].parts[0]
            .function_response
            .as_ref()
            .expect("has function response");
        assert_eq!(response.name, "fs_read");
    }

    fn called_names(completion: &Completion) -> Vec<String> {
        completion.candidates[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parse_completion_body_maps_calls_back_streamed_and_not() {
        let model = ModelId::new("gemini", "gemini-2.5-pro");
        let req = dotted_tool_request(false);
        let names = WireNames::for_request(&req);
        let whole = r#"{"candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "fs_read", "args": {}}},
            {"functionCall": {"name": "invented", "args": {}}}
        ]}, "finishReason": "STOP"}]}"#;
        let completion = parse_completion_body(
            whole.as_bytes(),
            false,
            &model,
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
            &names,
        )
        .expect("parses");
        assert_eq!(called_names(&completion), vec!["fs.read", "invented"]);

        let streamed = concat!(
            "data: {\"candidates\": [{\"content\": {\"role\": \"model\", \"parts\": [{\"text\": \"Reading.\"}]}}]}\n\n",
            "data: {\"candidates\": [{\"content\": {\"role\": \"model\", \"parts\": [{\"functionCall\": {\"name\": \"fs_read\", \"args\": {}}}]}, \"finishReason\": \"STOP\"}]}\n\n",
        );
        let completion = parse_completion_body(
            streamed.as_bytes(),
            true,
            &model,
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
            &names,
        )
        .expect("assembles");
        assert_eq!(called_names(&completion), vec!["fs.read"]);
    }
}
