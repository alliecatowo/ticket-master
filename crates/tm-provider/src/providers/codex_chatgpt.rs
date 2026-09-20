//! [`CodexChatGptProvider`]: a [`crate::fabric::Provider`] that calls the real OpenAI Codex CLI's
//! own ChatGPT-subscription backend, authenticated via [`tm_auth::CodexSubscriptionOAuth`]
//! (`crates/tm-auth/src/codex_subscription.rs`) instead of an `{PREFIX}_API_KEY` environment
//! variable — the twelfth provider module in this crate, and the first whose credential comes
//! from another program's (`codex`'s) own already-completed login rather than this workspace's
//! own OAuth flow or a bare API key.
//!
//! # Endpoint and wire shape
//!
//! `POST https://chatgpt.com/backend-api/codex/responses`, speaking the (undocumented, for this
//! specific host) Responses API wire shape — flat `tools`, an `input` array of typed items
//! (`message` / `function_call` / `function_call_output`), a top-level `instructions` field for
//! the system prompt, and `max_output_tokens` rather than Chat Completions' `max_tokens`. This is
//! **not** [`crate::providers::compat::CompatProvider`]'s Chat Completions dialect and does not
//! build on it — see `docs/decisions/D-015-codex-chatgpt-session-auth-adapter.md` for why a
//! second, self-contained wire module was the right call over stretching `compat.rs` to cover a
//! second protocol family.
//!
//! Determined by cross-referencing this machine's own real, decoded (never printed) access-token
//! JWT claims against public OpenAI documentation and third-party reverse-engineering writeups —
//! see the decision doc for the full source list and honest confidence level. **This was not
//! empirically verified against the live endpoint from inside this development session**: the
//! harness sandbox this change was built in blocks ad hoc raw network probes (even authenticated
//! ones a human had explicitly authorized), so the [`tests`] module below only covers the pure
//! wire-shape translation functions against literal recorded-shaped JSON, and the real end-to-end
//! proof is [`crate`]-external: `crates/tm-agent/src/agent_loop.rs`'s
//! `tests::live_codex_auth::real_agent_loop_turn_reaches_the_real_codex_backend_and_submits`
//! (gated behind `--features live-codex-auth`, `#[ignore]`, and `TM_LIVE_CODEX_AUTH=1` — see
//! `mise.toml`'s `test:live-codex-auth` task), which either confirms this shape against the real
//! backend or fails with the real server's own error message pointing at what's wrong.
//!
//! # Streaming, always
//!
//! [`build_wire_request`] hardcodes `stream: true` regardless of
//! [`crate::types::CompletionRequest::stream`]: every third-party source this module's docs cite
//! that calls this specific Codex-branded backend does so with `stream: true`, and none confirms
//! `stream: false` is accepted here (unlike the public `api.openai.com/v1/responses`, which
//! supports both) — forcing streaming is the conservative choice given that gap, not a
//! preference. [`assemble_streamed_completion`] then reassembles the single [`crate::types::Completion`]
//! this crate's [`crate::fabric::Provider::complete`] contract promises, the same non-incremental
//! contract [`crate::providers::compat`]'s own streaming path honors (see that module's docs).
//! Unlike Chat Completions SSE, the Responses API's streaming protocol emits the *entire* final
//! response object on its terminal `response.completed`/`response.incomplete`/`response.failed`
//! event, so reassembly here is "find that event and parse its embedded object", not delta
//! accumulation — see [`extract_final_response`].
//!
//! # Headers
//!
//! `Authorization: Bearer <access_token>` (from [`tm_auth::CodexSubscriptionOAuth::credential`],
//! refreshed transparently), `ChatGPT-Account-ID: <account_id>` (from
//! [`tm_auth::CodexSubscriptionOAuth::account_id`], read once at construction — non-secret, so it
//! does not need per-call refresh the way the bearer token does), and `originator: codex_cli_rs`.
//! The last of these is the least certain of the three: multiple sources describe the backend's
//! model catalog as gated on this header without a canonical reference for its exact required
//! value, so `codex_cli_rs` (the value consistent with those sources) is a best-effort match to
//! what the real `codex` binary itself sends, not a confirmed contract.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_auth::{AuthAdapter, CodexSubscriptionOAuth};
use tm_types::{Clock, Timestamp};

use crate::fabric::Provider;
use crate::types::{
    Candidate, Completion, CompletionRequest, ContentBlock, EmbedRequest, Embeddings, Message,
    MessageRole, ModelId, ProviderError, StopReason, Usage,
};

/// The real Codex CLI ChatGPT-subscription backend — see this module's docs for how this was
/// determined.
const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// See this module's docs, "Headers" section, for this value's confidence level.
const ORIGINATOR: &str = "codex_cli_rs";

/// Default request timeout for the underlying HTTP client, matching
/// [`crate::providers::compat`]'s own default.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum retry attempts on 429/5xx before giving up, matching
/// [`crate::providers::compat::CompatConfig`]'s own default.
const MAX_RETRIES: u32 = 3;

/// Floor for exponential backoff when the backend gives no `Retry-After`.
const BACKOFF_FLOOR: Duration = Duration::from_millis(500);

/// Calls the real Codex CLI ChatGPT-subscription backend using a real, already-authenticated
/// [`tm_auth::CodexSubscriptionOAuth`] adapter. See this module's top docs for endpoint, wire
/// shape, streaming and header details.
pub struct CodexChatGptProvider {
    model: String,
    adapter: Arc<CodexSubscriptionOAuth>,
    /// [`tm_auth::CodexSubscriptionOAuth::account_id`], read once at construction. Non-secret
    /// (an opaque account/org identifier), so — unlike the bearer token — it does not need to be
    /// re-read per call; if the underlying Codex session ever changes account, a fresh
    /// [`CodexChatGptProvider`] should be constructed rather than expecting this one to notice.
    account_id: Option<String>,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
}

impl CodexChatGptProvider {
    /// Build a provider for `model` (e.g. `"gpt-5.6-terra"` — whatever `codex doctor`/
    /// `~/.codex/config.toml`'s own `model` field names on this machine; this provider does not
    /// guess a default, since the real Codex CLI session is itself the source of truth for which
    /// models the account can reach) over `adapter`.
    pub fn new(
        model: impl Into<String>,
        adapter: Arc<CodexSubscriptionOAuth>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Unavailable(format!("failed to build HTTP client: {e}")))?;
        let account_id = adapter
            .account_id()
            .map_err(|e| ProviderError::AuthFailed(e.to_string()))?;
        Ok(CodexChatGptProvider {
            model: model.into(),
            adapter,
            account_id,
            http,
            clock,
        })
    }

    /// [`CodexChatGptProvider::new`], building a fresh [`tm_auth::CodexSubscriptionOAuth`] over
    /// the real `$CODEX_HOME`/`$HOME/.codex` ([`tm_auth::default_codex_home`]) rather than
    /// requiring the caller to construct the adapter separately.
    pub fn from_local_session(
        model: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ProviderError> {
        let adapter = CodexSubscriptionOAuth::new(clock.clone())
            .map_err(|e| ProviderError::AuthFailed(e.to_string()))?;
        Self::new(model, Arc::new(adapter), clock)
    }

    fn build_headers(
        &self,
        access_token: &str,
    ) -> Result<reqwest::header::HeaderMap, ProviderError> {
        let mut headers = reqwest::header::HeaderMap::new();
        let auth_value = reqwest::header::HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|e| {
                ProviderError::InvalidRequest(format!(
                    "access token is not a valid header value: {e}"
                ))
            })?;
        headers.insert(reqwest::header::AUTHORIZATION, auth_value);
        if let Some(account_id) = &self.account_id {
            let value = reqwest::header::HeaderValue::from_str(account_id).map_err(|e| {
                ProviderError::InvalidRequest(format!(
                    "account id is not a valid header value: {e}"
                ))
            })?;
            headers.insert("chatgpt-account-id", value);
        }
        headers.insert(
            "originator",
            reqwest::header::HeaderValue::from_static(ORIGINATOR),
        );
        Ok(headers)
    }

    /// Send one JSON-bodied POST, retrying on retryable [`ProviderError`]s up to [`MAX_RETRIES`]
    /// times, honoring a `Retry-After` response header — mirrors
    /// [`crate::providers::compat::CompatProvider`]'s own retry loop, reusing
    /// [`crate::providers::compat::classify_status`] for status/error-envelope classification
    /// (the Responses API's `{"error": {"message": ...}}` envelope matches Chat Completions'
    /// closely enough that re-deriving a second classifier would be pure duplication).
    async fn send_with_retry(
        &self,
        headers: &reqwest::header::HeaderMap,
        body: &WireRequest,
    ) -> Result<(Vec<u8>, Duration, Timestamp), ProviderError> {
        let mut attempt: u32 = 0;
        loop {
            let started = self.clock.now();
            let response = self
                .http
                .post(RESPONSES_URL)
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
                    if err.is_retryable() && attempt < MAX_RETRIES {
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

            match crate::providers::compat::classify_status(
                status,
                retry_after_header.as_deref(),
                &body_bytes,
            ) {
                Ok(()) => {
                    let finished = self.clock.now();
                    let latency =
                        Duration::from_millis(finished.millis_since(started).max(0) as u64);
                    return Ok((body_bytes, latency, finished));
                }
                Err(err) => {
                    if err.is_retryable() && attempt < MAX_RETRIES {
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
impl Provider for CodexChatGptProvider {
    fn id(&self) -> &str {
        "codex-chatgpt"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let credential = self
            .adapter
            .credential()
            .await
            .map_err(|e| ProviderError::AuthFailed(e.to_string()))?;
        let wire_request = build_wire_request(&self.model, &req);
        let headers = self.build_headers(credential.expose_secret())?;
        let (body, latency, received_at) = self.send_with_retry(&headers, &wire_request).await?;
        assemble_streamed_completion(&body, &self.model, self.id(), latency, received_at)
    }

    async fn embed(&self, _req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        Err(ProviderError::InvalidRequest(format!(
            "{} does not support embeddings",
            self.id()
        )))
    }
}

// ---- wire shapes: Responses API ----------------------------------------------------------------

/// The Responses API request body, as this backend expects it. See this module's top docs.
#[derive(Debug, Clone, Serialize)]
struct WireRequest {
    model: String,
    input: Vec<WireInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tools: Vec<WireTool>,
    max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// Always `true` — see this module's top docs, "Streaming, always".
    stream: bool,
}

/// One item of [`WireRequest::input`], internally tagged on `"type"`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type")]
enum WireInputItem {
    #[serde(rename = "message")]
    Message {
        role: String,
        content: Vec<WireContentPart>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        /// JSON-encoded arguments string, matching the Responses API convention (not a nested
        /// JSON object) — same convention [`crate::providers::compat::WireFunctionCall::arguments`]
        /// follows for Chat Completions.
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: String },
}

/// One content part of a [`WireInputItem::Message`], internally tagged on `"type"`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type")]
enum WireContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
}

/// A tool definition on the wire — flat (`type`/`name`/`description`/`parameters` as siblings),
/// unlike Chat Completions' `{"type":"function","function":{...}}` nesting.
#[derive(Debug, Clone, Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// The Responses API response body shape — used both for the object embedded in a terminal SSE
/// event ([`extract_final_response`]) and (defensively) for a plain non-streaming body, should
/// this backend ever be confirmed to accept `stream: false`.
#[derive(Debug, Clone, Deserialize)]
struct WireResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    output: Vec<WireOutputItem>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    incomplete_details: Option<WireIncompleteDetails>,
    #[serde(default)]
    error: Option<WireError>,
}

/// One item of [`WireResponse::output`]. `#[serde(other)]`'s `Other` variant absorbs item types
/// this provider does not need to understand (e.g. `"reasoning"` summary items some models emit)
/// rather than failing the whole parse over them.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
enum WireOutputItem {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        content: Vec<WireOutputContentPart>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Other,
}

/// One content part of a [`WireOutputItem::Message`].
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
enum WireOutputContentPart {
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        #[serde(default)]
        #[allow(dead_code)] // parsed for shape-completeness; never read
        annotations: Vec<serde_json::Value>,
    },
    #[serde(rename = "refusal")]
    Refusal {
        #[serde(default)]
        refusal: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// `WireResponse::incomplete_details`.
#[derive(Debug, Clone, Deserialize)]
struct WireIncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

/// The `{"error": {...}}` envelope, matching [`crate::providers::compat::WireErrorDetail`]'s
/// shape closely enough to describe independently rather than importing it across an unrelated
/// wire-shape boundary.
#[derive(Debug, Clone, Deserialize)]
struct WireError {
    #[serde(default)]
    message: Option<String>,
}

/// Usage on the wire.
#[derive(Debug, Clone, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<WireInputTokensDetails>,
}

/// `WireUsage::input_tokens_details`.
#[derive(Debug, Clone, Deserialize)]
struct WireInputTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

/// One event of the streamed response — see this module's docs, "Streaming, always". Only the
/// handful of top-level fields this provider needs are modeled; every other event field is
/// ignored by `serde`'s default "unknown fields are dropped" behavior (no `#[serde(deny_unknown_fields)]`
/// here).
#[derive(Debug, Clone, Deserialize)]
struct WireStreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    response: Option<WireResponse>,
    /// A defensive fallback for a top-level `{"type":"error","message":"..."}`-shaped event, in
    /// case the real stream ever emits one outside a `response.failed`-carried
    /// `WireResponse::error`. See this module's top docs on wire-shape confidence.
    #[serde(default)]
    message: Option<String>,
}

// ---- shaping: crate::types -> wire (pure, unit-tested against recorded JSON) -----------------

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

fn content_part(role: MessageRole, text: String) -> WireContentPart {
    match role {
        MessageRole::Assistant => WireContentPart::OutputText { text },
        MessageRole::System | MessageRole::User => WireContentPart::InputText { text },
    }
}

fn role_str(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
}

/// Translate one crate-internal [`Message`] into zero or more [`WireInputItem`]s, appending them
/// to `out`. Mirrors [`crate::providers::compat::push_wire_messages`]'s per-role text/tool-use/
/// tool-result splitting, but a [`ContentBlock::ToolUse`]/[`ContentBlock::ToolResult`] becomes
/// its own top-level input item ([`WireInputItem::FunctionCall`]/
/// [`WireInputItem::FunctionCallOutput`]) rather than a field nested inside a message item, per
/// the Responses API's item shape.
fn push_wire_input_items(msg: &Message, out: &mut Vec<WireInputItem>) {
    let mut buffer = String::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => {
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(text);
            }
            ContentBlock::ToolUse { id, name, input } => {
                if !buffer.is_empty() {
                    out.push(WireInputItem::Message {
                        role: role_str(msg.role).to_string(),
                        content: vec![content_part(msg.role, std::mem::take(&mut buffer))],
                    });
                }
                out.push(WireInputItem::FunctionCall {
                    call_id: id.clone(),
                    name: name.clone(),
                    arguments: serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()),
                });
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                if !buffer.is_empty() {
                    out.push(WireInputItem::Message {
                        role: role_str(msg.role).to_string(),
                        content: vec![content_part(msg.role, std::mem::take(&mut buffer))],
                    });
                }
                out.push(WireInputItem::FunctionCallOutput {
                    call_id: tool_use_id.clone(),
                    output: render_text_blocks(content),
                });
            }
        }
    }
    if !buffer.is_empty() {
        out.push(WireInputItem::Message {
            role: role_str(msg.role).to_string(),
            content: vec![content_part(msg.role, buffer)],
        });
    }
}

/// Translate a full message history into wire input items. See [`push_wire_input_items`] for the
/// per-role mapping.
fn build_wire_input(messages: &[Message]) -> Vec<WireInputItem> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        push_wire_input_items(msg, &mut out);
    }
    out
}

/// Build the wire request body for `req` against `model`. `req.system` maps to the top-level
/// `instructions` field (empty is dropped, matching
/// [`crate::providers::compat::build_wire_request`]'s empty-system handling). `req.stream` is
/// deliberately **not** consulted — see this module's top docs, "Streaming, always".
fn build_wire_request(model: &str, req: &CompletionRequest) -> WireRequest {
    WireRequest {
        model: model.to_string(),
        input: build_wire_input(&req.messages),
        instructions: req.system.clone().filter(|s| !s.is_empty()),
        tools: req
            .tools
            .iter()
            .map(|t| WireTool {
                kind: "function".to_string(),
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            })
            .collect(),
        max_output_tokens: req.max_tokens,
        temperature: req.temperature,
        stream: true,
    }
}

// ---- shaping: wire -> crate::types -------------------------------------------------------------

fn map_incomplete_reason(reason: &str) -> Result<StopReason, ProviderError> {
    match reason {
        "max_output_tokens" => Ok(StopReason::MaxTokens),
        other => Err(ProviderError::MalformedResponse(format!(
            "response incomplete: {other}"
        ))),
    }
}

/// Translate a fully-decoded [`WireResponse`] (either a plain response body or the object
/// embedded in a terminal streaming event) into a provider-independent [`Completion`].
fn completion_from_wire(
    wire: WireResponse,
    requested_model: &str,
    provider_id: &str,
    latency: Duration,
    received_at: Timestamp,
) -> Result<Completion, ProviderError> {
    if let Some(err) = &wire.error {
        return Err(ProviderError::InvalidRequest(
            err.message
                .clone()
                .unwrap_or_else(|| "codex backend reported an error with no message".to_string()),
        ));
    }

    let mut content = Vec::new();
    let mut saw_function_call = false;
    for item in &wire.output {
        match item {
            WireOutputItem::Message { content: parts } => {
                for part in parts {
                    match part {
                        WireOutputContentPart::OutputText { text, .. } => {
                            if !text.is_empty() {
                                content.push(ContentBlock::Text { text: text.clone() });
                            }
                        }
                        WireOutputContentPart::Refusal { refusal } => {
                            if let Some(r) = refusal {
                                return Err(ProviderError::InvalidRequest(format!(
                                    "codex backend refused: {r}"
                                )));
                            }
                        }
                        WireOutputContentPart::Other => {}
                    }
                }
            }
            WireOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                saw_function_call = true;
                let input: serde_json::Value = if arguments.is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(arguments).map_err(|e| {
                        ProviderError::MalformedResponse(format!(
                            "function_call arguments not valid JSON: {e}"
                        ))
                    })?
                };
                content.push(ContentBlock::ToolUse {
                    id: call_id.clone(),
                    name: name.clone(),
                    input,
                });
            }
            WireOutputItem::Other => {}
        }
    }

    let stop_reason = match wire.status.as_deref() {
        Some("completed") | None => {
            if saw_function_call {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
        Some("incomplete") => {
            let reason = wire
                .incomplete_details
                .as_ref()
                .and_then(|d| d.reason.as_deref())
                .unwrap_or("unknown");
            map_incomplete_reason(reason)?
        }
        Some("failed") => {
            return Err(ProviderError::Unavailable(
                "codex backend reported response status: failed".to_string(),
            ))
        }
        Some(other) => {
            return Err(ProviderError::MalformedResponse(format!(
                "unknown response status: {other}"
            )))
        }
    };

    let usage = wire
        .usage
        .map(|u| Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.input_tokens_details.map(|d| d.cached_tokens).unwrap_or(0),
            cache_write_tokens: 0,
        })
        .unwrap_or_default();

    Ok(Completion {
        model: ModelId::new(
            provider_id,
            wire.model.unwrap_or_else(|| requested_model.to_string()),
        ),
        candidates: vec![Candidate {
            content,
            stop_reason,
        }],
        usage,
        latency,
        received_at,
    })
}

/// Parse a plain (non-streaming) response body — [`assemble_streamed_completion`]'s fallback when
/// a response body has no SSE `data:` framing at all, and kept independently unit-testable for
/// the day this backend is confirmed to accept `stream: false` as a first-class mode rather than
/// a fallback.
fn parse_wire_response(
    body: &[u8],
    requested_model: &str,
    provider_id: &str,
    latency: Duration,
    received_at: Timestamp,
) -> Result<Completion, ProviderError> {
    let wire: WireResponse = serde_json::from_slice(body).map_err(|e| {
        ProviderError::MalformedResponse(format!("failed to parse codex responses payload: {e}"))
    })?;
    completion_from_wire(wire, requested_model, provider_id, latency, received_at)
}

// ---- streaming: SSE parsing + terminal-event extraction ----------------------------------------

/// Split a fully-buffered SSE response body into its `data:` events and parse each as a
/// [`WireStreamEvent`], skipping blank keep-alive lines — mirrors
/// [`crate::providers::compat::parse_sse_body`]'s framing (this crate reads the whole response
/// body before parsing rather than streaming incrementally; see that function's docs for why).
fn parse_sse_body(body: &[u8]) -> Result<Vec<WireStreamEvent>, ProviderError> {
    let text = std::str::from_utf8(body).map_err(|e| {
        ProviderError::MalformedResponse(format!("SSE body was not valid UTF-8: {e}"))
    })?;

    let mut events = Vec::new();
    for event in text.split("\n\n") {
        for line in event.lines() {
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let parsed: WireStreamEvent = serde_json::from_str(data).map_err(|e| {
                ProviderError::MalformedResponse(format!("failed to parse codex SSE event: {e}"))
            })?;
            events.push(parsed);
        }
    }
    Ok(events)
}

/// Find the terminal event (`response.completed` / `response.incomplete` / `response.failed`)
/// among `events` and return its embedded [`WireResponse`] — see this module's top docs,
/// "Streaming, always", for why this is reassembly-by-extraction rather than delta accumulation.
fn extract_final_response(events: &[WireStreamEvent]) -> Result<WireResponse, ProviderError> {
    for event in events {
        match event.kind.as_str() {
            "response.completed" | "response.incomplete" | "response.failed" => {
                return event.response.clone().ok_or_else(|| {
                    ProviderError::MalformedResponse(format!(
                        "{} event carried no response object",
                        event.kind
                    ))
                });
            }
            "error" => {
                return Err(ProviderError::Unavailable(
                    event
                        .message
                        .clone()
                        .unwrap_or_else(|| "codex backend streamed an error event".to_string()),
                ));
            }
            _ => continue,
        }
    }
    Err(ProviderError::MalformedResponse(
        "SSE stream ended without a terminal response.* event".to_string(),
    ))
}

/// Reassemble a response body into the same [`Completion`] shape [`parse_wire_response`] produces
/// from a plain body. `body` is expected to be SSE-framed (this provider always requests
/// `stream: true` — see this module's top docs); [`parse_sse_body`] finding zero `data:` lines at
/// all (rather than finding some but never a terminal `response.*` one — a genuine protocol
/// error, left alone below) is treated as "this wasn't actually SSE" and falls back to
/// [`parse_wire_response`], a defensive allowance for a body that arrives as plain JSON despite
/// the request — real-world slack given this module's own documented uncertainty about the exact
/// wire contract, not an expected steady-state path.
fn assemble_streamed_completion(
    body: &[u8],
    requested_model: &str,
    provider_id: &str,
    latency: Duration,
    received_at: Timestamp,
) -> Result<Completion, ProviderError> {
    let events = parse_sse_body(body)?;
    if events.is_empty() {
        return parse_wire_response(body, requested_model, provider_id, latency, received_at);
    }
    let wire = extract_final_response(&events)?;
    completion_from_wire(wire, requested_model, provider_id, latency, received_at)
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

    // ---- build_wire_request / build_wire_input ----

    #[test]
    fn build_wire_request_maps_instructions_and_user_input() {
        let wire = build_wire_request("gpt-test", &sample_request());
        assert_eq!(wire.model, "gpt-test");
        assert_eq!(
            wire.instructions.as_deref(),
            Some("You are a helpful assistant.")
        );
        assert_eq!(wire.input.len(), 1);
        assert_eq!(
            wire.input[0],
            WireInputItem::Message {
                role: "user".to_string(),
                content: vec![WireContentPart::InputText {
                    text: "Hello".to_string()
                }],
            }
        );
        assert_eq!(wire.max_output_tokens, 1024);
        assert_eq!(wire.temperature, Some(0.0));
        assert_eq!(wire.tools.len(), 1);
        assert_eq!(wire.tools[0].kind, "function");
        assert_eq!(wire.tools[0].name, "get_weather");
    }

    #[test]
    fn build_wire_request_always_streams_regardless_of_request_stream_flag() {
        let mut req = sample_request();
        req.stream = false;
        assert!(build_wire_request("gpt-test", &req).stream);

        req.stream = true;
        assert!(build_wire_request("gpt-test", &req).stream);
    }

    #[test]
    fn build_wire_request_drops_empty_instructions() {
        let mut req = sample_request();
        req.system = Some(String::new());
        assert_eq!(build_wire_request("gpt-test", &req).instructions, None);

        req.system = None;
        assert_eq!(build_wire_request("gpt-test", &req).instructions, None);
    }

    #[test]
    fn build_wire_input_splits_tool_use_and_tool_result_into_their_own_items() {
        let messages = vec![
            Message {
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "checking".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "get_weather".to_string(),
                        input: serde_json::json!({"city": "SF"}),
                    },
                ],
            },
            Message {
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "72F".to_string(),
                    }],
                    is_error: false,
                }],
            },
        ];
        let items = build_wire_input(&messages);
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0],
            WireInputItem::Message {
                role: "assistant".to_string(),
                content: vec![WireContentPart::OutputText {
                    text: "checking".to_string()
                }],
            }
        );
        match &items[1] {
            WireInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(name, "get_weather");
                let parsed: serde_json::Value =
                    serde_json::from_str(arguments).expect("valid JSON");
                assert_eq!(parsed["city"], "SF");
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        }
        assert_eq!(
            items[2],
            WireInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "72F".to_string(),
            }
        );
    }

    // ---- parse_wire_response / completion_from_wire ----

    const RECORDED_TEXT_RESPONSE: &str = r#"{
        "id": "resp_test",
        "model": "gpt-test-2026",
        "status": "completed",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "authloopworks", "annotations": []}]
            }
        ],
        "usage": {"input_tokens": 12, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 3}}
    }"#;

    #[test]
    fn parse_wire_response_maps_text_content_and_usage() {
        let ts = tm_types::Timestamp::from_unix_seconds(1_700_000_000);
        let completion = parse_wire_response(
            RECORDED_TEXT_RESPONSE.as_bytes(),
            "gpt-test",
            "codex-chatgpt",
            Duration::from_millis(250),
            ts,
        )
        .expect("parses recorded response");
        assert_eq!(
            completion.model,
            ModelId::new("codex-chatgpt", "gpt-test-2026")
        );
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "authloopworks".to_string()
            }]
        );
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage.input_tokens, 12);
        assert_eq!(completion.usage.output_tokens, 5);
        assert_eq!(completion.usage.cache_read_tokens, 3);
    }

    const RECORDED_TOOL_CALL_RESPONSE: &str = r#"{
        "id": "resp_test2",
        "model": "gpt-test-2026",
        "status": "completed",
        "output": [
            {
                "type": "function_call",
                "call_id": "call_abc",
                "name": "get_weather",
                "arguments": "{\"city\":\"SF\"}"
            }
        ],
        "usage": {"input_tokens": 20, "output_tokens": 8}
    }"#;

    #[test]
    fn parse_wire_response_maps_a_function_call_to_tool_use_with_tool_use_stop_reason() {
        let ts = Timestamp::from_unix_seconds(1_700_000_000);
        let completion = parse_wire_response(
            RECORDED_TOOL_CALL_RESPONSE.as_bytes(),
            "gpt-test",
            "codex-chatgpt",
            Duration::from_millis(100),
            ts,
        )
        .expect("parses recorded response");
        assert_eq!(completion.candidates[0].stop_reason, StopReason::ToolUse);
        match &completion.candidates[0].content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    const RECORDED_INCOMPLETE_RESPONSE: &str = r#"{
        "status": "incomplete",
        "output": [],
        "incomplete_details": {"reason": "max_output_tokens"}
    }"#;

    #[test]
    fn parse_wire_response_maps_incomplete_max_output_tokens_to_max_tokens_stop_reason() {
        let ts = Timestamp::from_unix_seconds(1_700_000_000);
        let completion = parse_wire_response(
            RECORDED_INCOMPLETE_RESPONSE.as_bytes(),
            "gpt-test",
            "codex-chatgpt",
            Duration::from_millis(100),
            ts,
        )
        .expect("parses recorded response");
        assert_eq!(completion.candidates[0].stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn parse_wire_response_surfaces_an_embedded_error_object() {
        let body = br#"{"error": {"message": "model not found"}}"#;
        let err = parse_wire_response(
            body,
            "gpt-test",
            "codex-chatgpt",
            Duration::ZERO,
            Timestamp::EPOCH,
        )
        .expect_err("embedded error object");
        match err {
            ProviderError::InvalidRequest(msg) => assert!(msg.contains("model not found")),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_wire_response_ignores_unknown_output_item_types() {
        let body = br#"{
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "content": [{"type": "output_text", "text": "hi", "annotations": []}]}
            ]
        }"#;
        let completion = parse_wire_response(
            body,
            "gpt-test",
            "codex-chatgpt",
            Duration::ZERO,
            Timestamp::EPOCH,
        )
        .expect("parses despite an unrecognized item type");
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "hi".to_string()
            }]
        );
    }

    // ---- SSE parsing / extract_final_response / assemble_streamed_completion ----

    #[test]
    fn extract_final_response_finds_the_response_completed_event() {
        // SSE `data:` lines cannot contain literal newlines -- `RECORDED_TEXT_RESPONSE` is
        // pretty-printed for readability as a Rust source literal, so it has to be minified
        // before it can stand in for a real server's single-line `data: {...}` payload.
        let minified: serde_json::Value =
            serde_json::from_str(RECORDED_TEXT_RESPONSE).expect("valid JSON fixture");
        let minified = serde_json::to_string(&minified).expect("re-serializes compactly");
        let sse = format!(
            "data: {{\"type\":\"response.created\"}}\n\n\
             data: {{\"type\":\"response.output_text.delta\",\"delta\":\"auth\"}}\n\n\
             data: {{\"type\":\"response.completed\",\"response\":{minified}}}\n\n\
             data: [DONE]\n\n"
        );
        let completion = assemble_streamed_completion(
            sse.as_bytes(),
            "gpt-test",
            "codex-chatgpt",
            Duration::from_millis(300),
            Timestamp::from_unix_seconds(1_700_000_000),
        )
        .expect("assembles from the terminal event");
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "authloopworks".to_string()
            }]
        );
    }

    #[test]
    fn extract_final_response_surfaces_a_top_level_error_event() {
        let sse = "data: {\"type\":\"error\",\"message\":\"invalid api key\"}\n\n";
        let err = assemble_streamed_completion(
            sse.as_bytes(),
            "gpt-test",
            "codex-chatgpt",
            Duration::ZERO,
            Timestamp::EPOCH,
        )
        .expect_err("top-level error event");
        match err {
            ProviderError::Unavailable(msg) => assert!(msg.contains("invalid api key")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn extract_final_response_errors_when_no_terminal_event_ever_arrives() {
        let sse = "data: {\"type\":\"response.created\"}\n\n";
        let err = assemble_streamed_completion(
            sse.as_bytes(),
            "gpt-test",
            "codex-chatgpt",
            Duration::ZERO,
            Timestamp::EPOCH,
        )
        .expect_err("stream never reached a terminal event");
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn assemble_streamed_completion_falls_back_to_a_plain_json_body_with_no_sse_framing() {
        // No "data:" lines at all -- `parse_sse_body` finds zero events, which
        // `assemble_streamed_completion` treats as "this wasn't SSE", not as a protocol error.
        let completion = assemble_streamed_completion(
            RECORDED_TEXT_RESPONSE.as_bytes(),
            "gpt-test",
            "codex-chatgpt",
            Duration::from_millis(50),
            Timestamp::from_unix_seconds(1_700_000_000),
        )
        .expect("falls back to plain JSON parsing");
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "authloopworks".to_string()
            }]
        );
    }

    #[test]
    fn parse_sse_body_skips_keep_alive_and_done_sentinel() {
        let sse = "\n\ndata: {\"type\":\"response.created\"}\n\ndata: [DONE]\n\n";
        let events = parse_sse_body(sse.as_bytes()).expect("parses");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "response.created");
    }
}
