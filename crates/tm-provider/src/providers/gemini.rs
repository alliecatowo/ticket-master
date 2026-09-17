//! Google Gemini, over the `generativelanguage.googleapis.com` REST API — **not** built on
//! [`crate::providers::compat`], since Gemini's request/response shape is its own (`contents` /
//! `parts` / `functionCall` / `functionResponse`), not OpenAI Chat Completions. This module owns
//! its own wire shapes end to end, following the pattern [`crate::anthropic`] set: wire structs
//! private to this file, pure translation functions, its own retry/error-mapping copied in spirit
//! (not by importing `compat`'s private helpers) from `compat.rs`/`anthropic.rs`.
//!
//! Env vars read by [`GeminiProvider::from_env`]:
//! - `GEMINI_API_KEY` (required unless `GOOGLE_API_KEY` is set instead — check `GEMINI_API_KEY`
//!   first, fall back to `GOOGLE_API_KEY`, since both names are in common use).
//! - `GEMINI_BASE_URL` (optional, default `"https://generativelanguage.googleapis.com/v1beta"`).
//!
//! Auth: send the key as the `x-goog-api-key` header (Gemini also accepts a `?key=` query
//! parameter; prefer the header so the key never lands in logged URLs).
//!
//! # IMPL: exact wire shapes to implement
//!
//! **`generateContent`** (`POST {base_url}/models/{model}:generateContent`; use
//! `:streamGenerateContent?alt=sse` for `req.stream == true`, and reuse
//! [`crate::providers::compat::parse_sse_body`]'s *pattern* — split on blank lines, strip
//! `"data:"` — but with Gemini's own per-chunk JSON shape below, not `compat`'s):
//!
//! Request body:
//! ```json
//! {
//!   "systemInstruction": {"parts": [{"text": "..."}]},
//!   "contents": [
//!     {"role": "user", "parts": [{"text": "..."}]},
//!     {"role": "model", "parts": [{"functionCall": {"name": "...", "args": {}}}]},
//!     {"role": "user", "parts": [{"functionResponse": {"name": "...", "response": {}}}]}
//!   ],
//!   "tools": [{"functionDeclarations": [{"name": "...", "description": "...", "parameters": {}}]}],
//!   "generationConfig": {
//!     "maxOutputTokens": 1024,
//!     "temperature": 0.0,
//!     "stopSequences": ["STOP"]
//!   }
//! }
//! ```
//! Mapping from [`crate::types::CompletionRequest`]:
//! - [`crate::types::MessageRole::System`] text -> `systemInstruction` (there is no wire
//!   `"system"` role in `contents`, matching how `crate::anthropic` handles it).
//! - [`crate::types::MessageRole::User`] -> `role: "user"`; [`crate::types::MessageRole::Assistant`]
//!   -> `role: "model"` (Gemini's name for the assistant turn — do not send `"assistant"`).
//! - [`crate::types::ContentBlock::Text`] -> `{"text": ...}` part.
//! - [`crate::types::ContentBlock::ToolUse`] -> `{"functionCall": {"name", "args": input}}` part
//!   (note: `args` is the input object directly, not a JSON-encoded string like `compat`'s
//!   `WireFunctionCall::arguments`).
//! - [`crate::types::ContentBlock::ToolResult`] -> `{"functionResponse": {"name", "response":
//!   content}}` part on a `role: "user"` turn. Gemini's `functionResponse` has no call-id
//!   correlation field at all (unlike OpenAI's `tool_call_id` or Anthropic's `tool_use_id`); it
//!   correlates by `name` and turn order only, so `crate::types::ContentBlock::ToolResult::tool_use_id`
//!   is dropped on the way out (there's nowhere on the wire to put it) and `name` must come from
//!   matching this result back to its preceding `ToolUse` block by id, resolved on this module's
//!   side before serializing.
//!
//! Response body:
//! ```json
//! {
//!   "candidates": [{
//!     "content": {"role": "model", "parts": [{"text": "..."}]},
//!     "finishReason": "STOP"
//!   }],
//!   "usageMetadata": {
//!     "promptTokenCount": 12,
//!     "candidatesTokenCount": 5,
//!     "cachedContentTokenCount": 0
//!   }
//! }
//! ```
//! `finishReason` values to map to [`crate::types::StopReason`]: `"STOP"` -> `EndTurn`,
//! `"MAX_TOKENS"` -> `MaxTokens`, a candidate whose only part is a `functionCall` -> `ToolUse`
//! (Gemini does not have a distinct finish-reason string for this the way OpenAI's `"tool_calls"`
//! does — detect it from content shape instead). Anything else (`"SAFETY"`, `"RECITATION"`, ...)
//! -> [`crate::types::ProviderError::MalformedResponse`], matching this crate's existing
//! strictness convention for finish reasons with no `StopReason` equivalent.
//!
//! **`embedContent`** (`POST {base_url}/models/{model}:embedContent` for one input, or
//! `:batchEmbedContents` for [`crate::types::EmbedRequest::inputs`] with more than one entry —
//! prefer the batch endpoint unconditionally to keep this module's request path uniform):
//! ```json
//! {"requests": [{"model": "models/{model}", "content": {"parts": [{"text": "..."}]}}]}
//! ```
//! Response: `{"embeddings": [{"values": [0.1, 0.2, ...]}, ...]}`, one entry per request, in
//! order — no explicit index field here (unlike `compat`'s `WireEmbedDatum::index`), so this
//! endpoint's ordering guarantee from Google's docs must be trusted directly.
//!
//! **Error mapping**: Gemini's error envelope is `{"error": {"code": 429, "message": "...",
//! "status": "RESOURCE_EXHAUSTED"}}` — `code` is an HTTP-status-shaped integer duplicating the
//! actual HTTP status, `status` is a gRPC-style string. Classify by HTTP status exactly like
//! [`crate::anthropic::classify_status`] (429 -> `RateLimited`, 5xx -> `Unavailable`, 401/403 ->
//! `AuthFailed`, other 4xx -> `InvalidRequest`); use `error.message` for the message text.

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::ProviderInfo;
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// Google Gemini, via the Generative Language API. See the module docs for the exact wire shapes
/// to implement (Gemini is not OpenAI-Chat-Completions-shaped, so this does not build on
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
    // IMPL:
    // 1. Read GEMINI_API_KEY; if absent, fall back to GOOGLE_API_KEY; if both absent ->
    //    `Err(crate::providers::missing_env_var("GEMINI_API_KEY"))`.
    // 2. Read GEMINI_BASE_URL, defaulting to
    //    "https://generativelanguage.googleapis.com/v1beta".
    // 3. Build a `reqwest::Client` with a timeout (120s matches every other provider in this
    //    crate) the same way `crate::anthropic::AnthropicProvider::with_config` does.
    // 4. Store `id: "gemini".to_string()`, the given `model`, `max_retries: 3` (matches
    //    `crate::anthropic::DEFAULT_MAX_RETRIES`).
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire GEMINI_API_KEY (falling back to GOOGLE_API_KEY) / GEMINI_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "gemini",
    //     display_name: "Google Gemini",
    //     env_vars: &[
    //         EnvVarRequirement { name: "GEMINI_API_KEY", required: true, description: "API key (falls back to GOOGLE_API_KEY if unset)" },
    //         EnvVarRequirement { name: "GOOGLE_API_KEY", required: false, description: "Fallback name for GEMINI_API_KEY" },
    //         EnvVarRequirement { name: "GEMINI_BASE_URL", required: false, description: "Override the default https://generativelanguage.googleapis.com/v1beta" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    // Note: `ProviderInfo::is_configured` requires every `required: true` var to be present, so
    // do not mark GOOGLE_API_KEY required too — that would demand *both* names be set at once.
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    // IMPL: build the `generateContent`/`streamGenerateContent` request per the module docs,
    // POST to `{base_url}/models/{model}:generateContent` with header `x-goog-api-key`, retry on
    // 429/5xx honoring `Retry-After` exactly like `crate::anthropic::AnthropicProvider::complete`,
    // then parse the response per the module docs' shape.
    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let _ = req;
        todo!("implement per the generateContent wire shape documented in the module docs above")
    }

    // IMPL: build the `batchEmbedContents` request per the module docs, POST to
    // `{base_url}/models/{model}:batchEmbedContents`, parse the response per the module docs.
    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let _ = req;
        todo!("implement per the batchEmbedContents wire shape documented in the module docs above")
    }
}
