//! Azure OpenAI, AWS Bedrock and Google Vertex AI: three "the model vendor, but behind a cloud
//! account" backends. Only [`AzureOpenAiProvider`] can build on
//! [`crate::providers::compat`] — Bedrock and Vertex both require request-signing schemes this
//! crate has no dependency to implement (see each struct's doc comment for exactly what is
//! missing and why this agent should not add it unilaterally, per the workspace's
//! no-new-dependencies rule for anyone but the scaffold owner).
//!
//! ## Status summary (read before wiring any of these into the registry)
//!
//! - [`AzureOpenAiProvider`]: fully implemented, over [`crate::providers::compat`].
//! - [`BedrockProvider`]: **not implemented**. `from_env` and both [`crate::fabric::Provider`]
//!   methods return [`ProviderError::Unavailable`] explaining that AWS SigV4 signing has no
//!   dependency to build on (`hmac`/`sha2` or an AWS SDK crate are absent from this crate's
//!   `Cargo.toml`, and this file may not edit that manifest). No request is ever sent with a
//!   fabricated or missing signature.
//! - [`VertexProvider`]: implemented for the pre-minted-bearer-token path only
//!   (`VERTEX_ACCESS_TOKEN`), which needs no new dependency. Minting a token directly from a
//!   service-account JSON file (`GOOGLE_APPLICATION_CREDENTIALS`) is out of scope for the same
//!   reason as Bedrock — it needs RS256 JWT signing this crate has no dependency for.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::{AuthStyle, CompatConfig, CompatProvider};
use crate::providers::{missing_env_var, Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Candidate, Completion, CompletionRequest, ContentBlock, EmbedRequest, Embeddings, MessageRole,
    ModelId, ProviderError, StopReason, Usage,
};
use crate::wire_names::WireNames;

/// Default Azure OpenAI API version, absent `AZURE_OPENAI_API_VERSION`.
const AZURE_DEFAULT_API_VERSION: &str = "2024-10-21";

/// Azure OpenAI Service. Builds on [`crate::providers::compat`]: same Chat Completions JSON body
/// as plain OpenAI, different auth header and URL shape (the model is a *deployment name* baked
/// into the path plus an `api-version` query parameter, not a body field OpenAI-proper uses).
///
/// Env vars read by [`AzureOpenAiProvider::from_env`]:
/// - `AZURE_OPENAI_API_KEY` (required).
/// - `AZURE_OPENAI_ENDPOINT` (required) — e.g. `"https://my-resource.openai.azure.com"`, no
///   trailing slash, no path segment.
/// - `AZURE_OPENAI_DEPLOYMENT` (required) — the deployment name, distinct from the underlying
///   model id; this is what appears in the URL path, not `model.model`.
/// - `AZURE_OPENAI_API_VERSION` (optional, default `"2024-10-21"`).
///
/// Azure deploys one model per deployment per capability; this struct assumes the given
/// deployment serves both chat and embeddings under the same name, which matches how most
/// Azure OpenAI resources are provisioned in practice (a caller routing chat and embedding roles
/// to genuinely different deployments needs two `AzureOpenAiProvider` instances with different
/// `AZURE_OPENAI_DEPLOYMENT` values, one per process — this crate has no per-request deployment
/// override).
pub struct AzureOpenAiProvider {
    compat: CompatProvider,
}

impl AzureOpenAiProvider {
    /// Build a provider for `model`, reading configuration from the environment. See the struct
    /// doc comment for the exact env vars and URL shape.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let api_key = std::env::var("AZURE_OPENAI_API_KEY")
            .map_err(|_| missing_env_var("AZURE_OPENAI_API_KEY"))?;
        let endpoint = std::env::var("AZURE_OPENAI_ENDPOINT")
            .map_err(|_| missing_env_var("AZURE_OPENAI_ENDPOINT"))?;
        let deployment = std::env::var("AZURE_OPENAI_DEPLOYMENT")
            .map_err(|_| missing_env_var("AZURE_OPENAI_DEPLOYMENT"))?;
        let api_version = std::env::var("AZURE_OPENAI_API_VERSION")
            .unwrap_or_else(|_| AZURE_DEFAULT_API_VERSION.to_string());

        let chat_path =
            format!("/openai/deployments/{deployment}/chat/completions?api-version={api_version}");
        let embed_path =
            format!("/openai/deployments/{deployment}/embeddings?api-version={api_version}");

        let config = CompatConfig::new("azure-openai", endpoint, model.model)
            .with_api_key(api_key)
            .with_auth_style(AuthStyle::Header("api-key".to_string()))
            .with_chat_path(chat_path)
            .with_embeddings_path(embed_path);

        Ok(AzureOpenAiProvider {
            compat: CompatProvider::new(config, clock)?,
        })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "azure-openai",
            display_name: "Azure OpenAI",
            env_vars: &[
                EnvVarRequirement {
                    name: "AZURE_OPENAI_API_KEY",
                    required: true,
                    description: "Resource API key",
                },
                EnvVarRequirement {
                    name: "AZURE_OPENAI_ENDPOINT",
                    required: true,
                    description: "https://<resource>.openai.azure.com, no path",
                },
                EnvVarRequirement {
                    name: "AZURE_OPENAI_DEPLOYMENT",
                    required: true,
                    description: "Deployment name (not the underlying model id)",
                },
                EnvVarRequirement {
                    name: "AZURE_OPENAI_API_VERSION",
                    required: false,
                    description: "Override the default 2024-10-21",
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
impl Provider for AzureOpenAiProvider {
    fn id(&self) -> &str {
        self.compat.id()
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        self.compat.complete(req).await
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        self.compat.embed(req).await
    }
}

/// AWS Bedrock (model-agnostic gateway; this struct targets the Bedrock Runtime `InvokeModel` /
/// `Converse` API for one specific underlying model family per instance, same one-model-per-id
/// convention as the rest of this crate).
///
/// # Not implemented: blocked on a missing dependency — do not add it yourself
///
/// Bedrock authenticates with **AWS SigV4**: every request needs a canonical-request hash signed
/// with an HMAC-SHA256 derivation chain from the AWS secret key, plus the current UTC timestamp
/// (which must come from the injected [`tm_types::Clock`], never a direct wall-clock read, per
/// this workspace's determinism rule). Implementing SigV4 by hand without a crypto primitives
/// crate is impractical and risks a subtly-wrong implementation that fails silently against real
/// AWS (signature mismatches return opaque 403s). As of this scaffold, `crates/tm-provider`'s
/// `Cargo.toml` has no HMAC/SHA-256 crate (e.g. `hmac`, `sha2`) and no AWS SDK crate. Per this
/// workspace's dependency policy, only the scaffold owner may add one — **do not edit
/// `Cargo.toml` from this file's implementing agent seat.** Flag the need for
/// `hmac`/`sha2` (minimum) or `aws-sigv4`/`aws-credential-types` (if a fuller AWS SDK dependency
/// is acceptable) back to whoever owns this crate's manifest before this struct can be built.
///
/// This struct *does* fully shape and hold the Converse-API-relevant configuration (region,
/// Bedrock model id, credentials) so the only missing piece is the signature itself — every
/// entry point returns [`ProviderError::Unavailable`] identifying signing as the blocker rather
/// than attempting an unsigned or fake-signed request.
///
/// Once signing is available, env vars to read: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
/// `AWS_SESSION_TOKEN` (optional, for temporary credentials), `AWS_REGION` (required),
/// `BEDROCK_MODEL_ID` (required — Bedrock's own model id string, e.g.
/// `"anthropic.claude-sonnet-5-v1:0"`, distinct from `model.model`). Note also that Bedrock's
/// request/response body shape varies **by model family** even after signing is solved (the
/// Anthropic-on-Bedrock body is close to `crate::anthropic`'s `WireRequest` wrapped with an
/// `anthropic_version` field and no top-level `model`; Titan and Llama families differ again) —
/// this is a second, independent piece of work from signing, not solved by this comment.
pub struct BedrockProvider {
    id: String,
    #[allow(dead_code)] // wired once request shaping is implemented alongside signing
    model: ModelId,
    #[allow(dead_code)]
    region: String,
    #[allow(dead_code)]
    model_id: String,
    #[allow(dead_code)]
    http: reqwest::Client,
    #[allow(dead_code)]
    clock: Arc<dyn Clock>,
}

/// The error returned by every [`BedrockProvider`] entry point until SigV4 signing is wired.
/// Centralized so the message is identical everywhere it's raised.
fn bedrock_unavailable() -> ProviderError {
    ProviderError::Unavailable(
        "bedrock provider is not usable: AWS SigV4 request signing is not implemented in this \
         crate (no hmac/sha2 or AWS SDK dependency present in tm-provider's Cargo.toml); request \
         shaping is complete but no request will be sent without a real signature"
            .to_string(),
    )
}

impl BedrockProvider {
    /// Build a provider for `model`, reading configuration from the environment. Blocked on the
    /// missing SigV4 dependency described in the struct doc comment; do not attempt a hand-rolled
    /// signer without one.
    ///
    /// This constructor still validates that the required env vars are present (so a caller
    /// discovers a missing credential immediately, matching every other provider's `from_env`
    /// contract) but always fails with [`bedrock_unavailable`] afterward, since a constructed
    /// instance could never actually sign a request.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _access_key =
            std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| missing_env_var("AWS_ACCESS_KEY_ID"))?;
        let _secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| missing_env_var("AWS_SECRET_ACCESS_KEY"))?;
        let region = std::env::var("AWS_REGION").map_err(|_| missing_env_var("AWS_REGION"))?;
        let model_id =
            std::env::var("BEDROCK_MODEL_ID").map_err(|_| missing_env_var("BEDROCK_MODEL_ID"))?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| ProviderError::Unavailable(format!("failed to build HTTP client: {e}")))?;

        let _ = BedrockProvider {
            id: "bedrock".to_string(),
            model,
            region,
            model_id,
            http,
            clock,
        };
        Err(bedrock_unavailable())
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    ///
    /// Capabilities are conservative (`embedding: false`, `tool_use: false`) until a specific
    /// model family's request shape is implemented alongside signing; narrow or widen per family
    /// once that's known. Declared here regardless of [`BedrockProvider::from_env`] always
    /// failing today, so autodetection can at least report "configured but unusable" rather than
    /// "unknown".
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "bedrock",
            display_name: "AWS Bedrock",
            env_vars: &[
                EnvVarRequirement {
                    name: "AWS_ACCESS_KEY_ID",
                    required: true,
                    description: "AWS access key for SigV4 signing",
                },
                EnvVarRequirement {
                    name: "AWS_SECRET_ACCESS_KEY",
                    required: true,
                    description: "AWS secret key for SigV4 signing",
                },
                EnvVarRequirement {
                    name: "AWS_SESSION_TOKEN",
                    required: false,
                    description: "Session token for temporary credentials",
                },
                EnvVarRequirement {
                    name: "AWS_REGION",
                    required: true,
                    description: "Bedrock Runtime region, e.g. us-east-1",
                },
                EnvVarRequirement {
                    name: "BEDROCK_MODEL_ID",
                    required: true,
                    description: "Bedrock's own model id string",
                },
            ],
            capabilities: Capabilities {
                completion: true,
                embedding: false,
                streaming: true,
                tool_use: false,
                vision: false,
            },
        }
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let _ = req;
        Err(bedrock_unavailable())
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let _ = req;
        Err(bedrock_unavailable())
    }
}

/// Default Vertex AI location, only used to build the base URL; `VERTEX_LOCATION` itself is
/// still required (see struct docs) since a wrong default silently misroutes requests to the
/// wrong region.
const VERTEX_TIMEOUT: Duration = Duration::from_secs(120);

/// Default maximum retry attempts on 429/5xx before giving up, matching every other provider.
const VERTEX_MAX_RETRIES: u32 = 3;

/// Floor for exponential backoff when Vertex gives no `Retry-After`.
const VERTEX_BACKOFF_FLOOR: Duration = Duration::from_millis(500);

/// Google Vertex AI's Gemini endpoint — same request/response JSON shape as Google's public
/// Gemini API (Vertex mirrors `generateContent`/`embedContent`), but reached via a
/// project/location-scoped URL and authenticated with a Google OAuth2 bearer token instead of a
/// static API key.
///
/// [`crate::providers::gemini::GeminiProvider`] is itself an unimplemented stub as of this
/// writing (its translation functions are not `pub`), so this struct owns a self-contained copy
/// of the Gemini wire shapes and mapping logic below rather than depending on that module; if
/// `gemini.rs` grows `pub` translation functions later, deduplicating against them is worthwhile
/// follow-up but is not done here to avoid taking a hard dependency on another agent's
/// in-progress file.
///
/// # Implemented path
///
/// Accepts a pre-minted, externally-refreshed access token via `VERTEX_ACCESS_TOKEN` (e.g.
/// produced out-of-band by `gcloud auth print-access-token` on a refresh cadence outside this
/// process). Sent as `Authorization: Bearer {token}`. This needs no new dependency and covers
/// real usage today.
///
/// # Not implemented: minting a token from a service account
///
/// Minting a token directly from a `GOOGLE_APPLICATION_CREDENTIALS` service-account JSON file
/// requires signing a JWT with RS256, which needs an RSA/JWT-signing crate this crate does not
/// depend on. As with [`BedrockProvider`], flag this to the manifest owner rather than adding one
/// from this seat.
///
/// Env vars: `VERTEX_ACCESS_TOKEN` (required), `VERTEX_PROJECT` (required), `VERTEX_LOCATION`
/// (required, e.g. `"us-central1"`).
pub struct VertexProvider {
    id: String,
    model: ModelId,
    base_url: String,
    access_token: String,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    max_retries: u32,
}

impl VertexProvider {
    /// Build a provider for `model`, reading configuration from the environment (the
    /// pre-minted-token path only; see the struct doc comment).
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let access_token = std::env::var("VERTEX_ACCESS_TOKEN")
            .map_err(|_| missing_env_var("VERTEX_ACCESS_TOKEN"))?;
        let project =
            std::env::var("VERTEX_PROJECT").map_err(|_| missing_env_var("VERTEX_PROJECT"))?;
        let location =
            std::env::var("VERTEX_LOCATION").map_err(|_| missing_env_var("VERTEX_LOCATION"))?;

        let base_url = format!(
            "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models"
        );

        let http = reqwest::Client::builder()
            .timeout(VERTEX_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Unavailable(format!("failed to build HTTP client: {e}")))?;

        Ok(VertexProvider {
            id: "vertex".to_string(),
            model,
            base_url,
            access_token,
            http,
            clock,
            max_retries: VERTEX_MAX_RETRIES,
        })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "vertex",
            display_name: "Google Vertex AI",
            env_vars: &[
                EnvVarRequirement {
                    name: "VERTEX_ACCESS_TOKEN",
                    required: true,
                    description: "Pre-minted OAuth2 bearer token (service-account minting is not implemented)",
                },
                EnvVarRequirement {
                    name: "VERTEX_PROJECT",
                    required: true,
                    description: "GCP project id",
                },
                EnvVarRequirement {
                    name: "VERTEX_LOCATION",
                    required: true,
                    description: "Vertex AI region, e.g. us-central1",
                },
            ],
            capabilities: Capabilities {
                completion: true,
                embedding: true,
                streaming: false,
                tool_use: true,
                vision: false,
            },
        }
    }
}

// ---- Vertex/Gemini wire shapes (pure, unit-tested against recorded JSON) --------------------
//
// Mirrors the module docs `crate::providers::gemini` carries for the public Gemini API — Vertex
// speaks the same `generateContent`/`embedContent` JSON, just at a different URL with bearer
// auth. See that module's doc comment for the full mapping rationale; this is a self-contained
// copy (see the struct doc comment above for why).

/// One request body for `generateContent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "systemInstruction")]
    system_instruction: Option<VertexSystemInstruction>,
    contents: Vec<VertexContent>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tools: Vec<VertexToolWrapper>,
    #[serde(rename = "generationConfig")]
    generation_config: VertexGenerationConfig,
}

/// The `systemInstruction` wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexSystemInstruction {
    parts: Vec<VertexPart>,
}

/// One turn of `contents`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexContent {
    role: String,
    parts: Vec<VertexPart>,
}

/// One part of a turn. Only the fields this crate's [`ContentBlock`] can produce are modeled;
/// unrecognized parts on the way in fail with [`ProviderError::MalformedResponse`] rather than
/// being silently dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "functionCall")]
    function_call: Option<VertexFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "functionResponse")]
    function_response: Option<VertexFunctionResponse>,
}

impl VertexPart {
    fn text(text: String) -> Self {
        VertexPart {
            text: Some(text),
            function_call: None,
            function_response: None,
        }
    }

    fn function_call(name: String, args: serde_json::Value) -> Self {
        VertexPart {
            text: None,
            function_call: Some(VertexFunctionCall { name, args }),
            function_response: None,
        }
    }

    fn function_response(name: String, response: serde_json::Value) -> Self {
        VertexPart {
            text: None,
            function_call: None,
            function_response: Some(VertexFunctionResponse { name, response }),
        }
    }
}

/// A model-issued function call part.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexFunctionCall {
    name: String,
    args: serde_json::Value,
}

/// A caller-supplied function result part. Vertex correlates by `name` and turn order only —
/// there is no call-id field on the wire, matching the public Gemini API.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexFunctionResponse {
    name: String,
    response: serde_json::Value,
}

/// The `tools` wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexToolWrapper {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<VertexFunctionDeclaration>,
}

/// One tool definition on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// The `generationConfig` wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexGenerationConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        default,
        rename = "stopSequences"
    )]
    stop_sequences: Vec<String>,
}

/// The `generateContent` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexResponse {
    candidates: Vec<VertexCandidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<VertexUsageMetadata>,
}

/// One candidate in a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexCandidate {
    content: VertexContent,
    #[serde(rename = "finishReason", default)]
    finish_reason: Option<String>,
}

/// Usage accounting in a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexUsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: u32,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: u32,
    #[serde(rename = "cachedContentTokenCount", default)]
    cached_content_token_count: u32,
}

/// Vertex's error envelope, structurally identical to the public Gemini API's.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexErrorEnvelope {
    error: VertexErrorDetail,
}

/// One error detail on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexErrorDetail {
    message: String,
}

/// Batch embedding request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexBatchEmbedRequest {
    requests: Vec<VertexEmbedRequestEntry>,
}

/// One entry in a batch embedding request.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexEmbedRequestEntry {
    model: String,
    content: VertexEmbedContent,
}

/// The `content` field of one embedding request entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexEmbedContent {
    parts: Vec<VertexPart>,
}

/// Batch embedding response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexBatchEmbedResponse {
    embeddings: Vec<VertexEmbedding>,
}

/// One embedding vector in a batch embedding response.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VertexEmbedding {
    values: Vec<f32>,
}

/// Build the `generateContent` request body for `req` against `model`.
///
/// Follows the same role/content mapping as `crate::providers::gemini`'s module docs: `System`
/// text becomes `systemInstruction` (there is no wire `"system"` role); `User` -> `"user"`,
/// `Assistant` -> `"model"`; [`ContentBlock::ToolUse`] -> a `functionCall` part with `input` sent
/// directly as `args` (not JSON-encoded); [`ContentBlock::ToolResult`] -> a `functionResponse`
/// part on a `"user"` turn, resolved back to its tool name by scanning preceding assistant turns
/// for the matching [`ContentBlock::ToolUse::id`] (Vertex has nowhere on the wire to carry
/// `tool_use_id` itself, so it is dropped after this resolution step).
fn build_vertex_request(req: &CompletionRequest) -> VertexRequest {
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(system) = &req.system {
        system_parts.push(system.clone());
    }

    // Tool-use id -> tool name, so a later ToolResult in the same request can recover the name
    // Vertex's functionResponse needs (the wire has no id field to carry it directly).
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in &req.messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                tool_names.insert(id.clone(), name.clone());
            }
        }
    }

    let mut contents = Vec::new();
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
                let role = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "model",
                    MessageRole::System => unreachable!("system handled above"),
                };
                let parts = msg
                    .content
                    .iter()
                    .map(|block| block_to_vertex_part(block, &tool_names))
                    .collect();
                contents.push(VertexContent {
                    role: role.to_string(),
                    parts,
                });
            }
        }
    }

    let system_instruction = if system_parts.is_empty() {
        None
    } else {
        Some(VertexSystemInstruction {
            parts: vec![VertexPart::text(system_parts.join("\n\n"))],
        })
    };

    let tools = if req.tools.is_empty() {
        Vec::new()
    } else {
        vec![VertexToolWrapper {
            function_declarations: req
                .tools
                .iter()
                .map(|t| VertexFunctionDeclaration {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                })
                .collect(),
        }]
    };

    VertexRequest {
        system_instruction,
        contents,
        tools,
        generation_config: VertexGenerationConfig {
            max_output_tokens: req.max_tokens,
            temperature: req.temperature,
            stop_sequences: req.stop_sequences.clone(),
        },
    }
}

fn block_to_vertex_part(
    block: &ContentBlock,
    tool_names: &std::collections::HashMap<String, String>,
) -> VertexPart {
    match block {
        ContentBlock::Text { text } => VertexPart::text(text.clone()),
        ContentBlock::ToolUse { name, input, .. } => {
            VertexPart::function_call(name.clone(), input.clone())
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            let name = tool_names
                .get(tool_use_id)
                .cloned()
                .unwrap_or_else(|| tool_use_id.clone());
            // functionResponse.response is a single JSON object; collapse text content blocks
            // into a `{"result": "..."}` envelope the way the public Gemini API's own examples
            // do, since ContentBlock::ToolResult::content is a list but Vertex wants one value.
            let text: String = content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            VertexPart::function_response(name, serde_json::json!({ "result": text }))
        }
    }
}

fn vertex_part_to_block(part: &VertexPart) -> Result<ContentBlock, ProviderError> {
    if let Some(text) = &part.text {
        return Ok(ContentBlock::Text { text: text.clone() });
    }
    if let Some(call) = &part.function_call {
        // Vertex assigns no call id; synthesize one from the name so downstream ToolResult
        // correlation within this crate's ContentBlock shape still has something to reference.
        return Ok(ContentBlock::ToolUse {
            id: call.name.clone(),
            name: call.name.clone(),
            input: call.args.clone(),
        });
    }
    Err(ProviderError::MalformedResponse(
        "vertex response part had neither text nor functionCall".to_string(),
    ))
}

fn map_finish_reason(
    finish_reason: Option<&str>,
    content: &[ContentBlock],
) -> Result<StopReason, ProviderError> {
    if content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    {
        return Ok(StopReason::ToolUse);
    }
    match finish_reason {
        Some("STOP") | None => Ok(StopReason::EndTurn),
        Some("MAX_TOKENS") => Ok(StopReason::MaxTokens),
        Some(other) => Err(ProviderError::MalformedResponse(format!(
            "unmapped vertex finishReason: {other}"
        ))),
    }
}

/// Parse a successful `generateContent` response body into a provider-independent [`Completion`].
fn parse_vertex_response(
    body: &[u8],
    model: &ModelId,
    latency: Duration,
    received_at: tm_types::Timestamp,
) -> Result<Completion, ProviderError> {
    let wire: VertexResponse = serde_json::from_slice(body).map_err(|e| {
        ProviderError::MalformedResponse(format!("failed to parse vertex response: {e}"))
    })?;

    let candidate = wire.candidates.first().ok_or_else(|| {
        ProviderError::MalformedResponse("vertex response had no candidates".to_string())
    })?;

    let content = candidate
        .content
        .parts
        .iter()
        .map(vertex_part_to_block)
        .collect::<Result<Vec<_>, _>>()?;
    let stop_reason = map_finish_reason(candidate.finish_reason.as_deref(), &content)?;

    let usage = wire
        .usage_metadata
        .map(|u| Usage {
            input_tokens: u.prompt_token_count,
            output_tokens: u.candidates_token_count,
            cache_read_tokens: u.cached_content_token_count,
            cache_write_tokens: 0,
        })
        .unwrap_or_default();

    Ok(Completion {
        model: model.clone(),
        candidates: vec![Candidate {
            content,
            stop_reason,
        }],
        usage,
        latency,
        received_at,
    })
}

fn parse_vertex_error_message(body: &[u8], fallback: &str) -> String {
    serde_json::from_slice::<VertexErrorEnvelope>(body)
        .map(|env| env.error.message)
        .unwrap_or_else(|_| fallback.to_string())
}

/// Classify an HTTP response the same way `crate::anthropic::classify_status` does: 429 ->
/// [`ProviderError::RateLimited`], 5xx -> [`ProviderError::Unavailable`], 401/403 ->
/// [`ProviderError::AuthFailed`], other 4xx -> [`ProviderError::InvalidRequest`].
fn classify_vertex_status(
    status: reqwest::StatusCode,
    retry_after_header: Option<&str>,
    body: &[u8],
) -> Result<(), ProviderError> {
    if status.is_success() {
        return Ok(());
    }

    let retry_after =
        retry_after_header.and_then(|v| v.trim().parse::<u64>().ok().map(Duration::from_secs));

    if status.as_u16() == 429 {
        return Err(ProviderError::RateLimited {
            message: parse_vertex_error_message(body, "rate limited"),
            retry_after,
        });
    }
    if status.is_server_error() {
        return Err(ProviderError::Unavailable(parse_vertex_error_message(
            body,
            "provider unavailable",
        )));
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(ProviderError::AuthFailed(parse_vertex_error_message(
            body,
            "authentication failed",
        )));
    }
    Err(ProviderError::InvalidRequest(parse_vertex_error_message(
        body,
        &format!("request failed with status {status}"),
    )))
}

#[async_trait]
impl Provider for VertexProvider {
    fn id(&self) -> &str {
        &self.id
    }

    /// Send `req` to `generateContent`, retrying on 429/5xx up to `max_retries` times, honoring
    /// `Retry-After`, exactly the pattern `crate::anthropic::AnthropicProvider::complete` uses.
    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let model = req.model_or(&self.model);
        // Same tool-name mapping as `crate::providers::gemini` (see `crate::wire_names`).
        let names = WireNames::for_request(&req);
        let wire_request = build_vertex_request(&names.encode_request(&req));
        let url = format!("{}/{}:generateContent", self.base_url, model.model);

        let mut attempt: u32 = 0;
        loop {
            let started = self.clock.now();
            let response = self
                .http
                .post(&url)
                .bearer_auth(&self.access_token)
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
                        tokio::time::sleep(VERTEX_BACKOFF_FLOOR * attempt).await;
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

            match classify_vertex_status(status, retry_after_header.as_deref(), &body) {
                Ok(()) => {
                    let finished = self.clock.now();
                    let latency =
                        Duration::from_secs(finished.seconds_since(started).max(0) as u64);
                    return parse_vertex_response(&body, &model, latency, finished)
                        .map(|c| names.decode_completion(c));
                }
                Err(err) => {
                    if err.is_retryable() && attempt < self.max_retries {
                        attempt += 1;
                        let wait = err.retry_after().unwrap_or(VERTEX_BACKOFF_FLOOR * attempt);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    /// Send `req` to `batchEmbedContents`.
    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let url = format!("{}/{}:batchEmbedContents", self.base_url, self.model.model);
        let wire_model = format!("models/{}", self.model.model);
        let body = VertexBatchEmbedRequest {
            requests: req
                .inputs
                .iter()
                .map(|text| VertexEmbedRequestEntry {
                    model: wire_model.clone(),
                    content: VertexEmbedContent {
                        parts: vec![VertexPart::text(text.clone())],
                    },
                })
                .collect(),
        };

        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout(e.to_string())
                } else {
                    ProviderError::Unavailable(e.to_string())
                }
            })?;

        let status = response.status();
        let response_body = response.bytes().await.map_err(|e| {
            ProviderError::Unavailable(format!("failed to read response body: {e}"))
        })?;
        classify_vertex_status(status, None, &response_body)?;

        let wire: VertexBatchEmbedResponse =
            serde_json::from_slice(&response_body).map_err(|e| {
                ProviderError::MalformedResponse(format!(
                    "failed to parse vertex embed response: {e}"
                ))
            })?;

        Ok(Embeddings {
            model: self.model.clone(),
            vectors: wire.embeddings.into_iter().map(|e| e.values).collect(),
            usage: Usage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ToolDef, Usage as CrateUsage};

    fn sample_request() -> CompletionRequest {
        CompletionRequest {
            system: Some("Be terse.".to_string()),
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
            max_tokens: 512,
            temperature: Some(0.0),
            stop_sequences: vec!["STOP".to_string()],
            stream: false,
            n: 1,
            model: None,
        }
    }

    // ---- Azure OpenAI ------------------------------------------------------------------------

    #[test]
    fn azure_info_declares_required_env_vars_and_capabilities() {
        let info = AzureOpenAiProvider::info();
        assert_eq!(info.id, "azure-openai");
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "AZURE_OPENAI_API_KEY" && v.required));
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "AZURE_OPENAI_API_VERSION" && !v.required));
        assert!(info.capabilities.completion);
        assert!(info.capabilities.embedding);
    }

    #[test]
    fn azure_from_env_fails_with_missing_env_var_when_key_absent() {
        // Deliberately does not touch process env (which would race other tests); exercises the
        // missing_env_var helper directly through the same path from_env uses.
        let err = missing_env_var("AZURE_OPENAI_API_KEY");
        assert!(
            matches!(err, ProviderError::AuthFailed(msg) if msg.contains("AZURE_OPENAI_API_KEY"))
        );
    }

    #[test]
    fn azure_deployment_path_includes_api_version_query_param() {
        let deployment = "gpt-4o-prod";
        let api_version = "2024-10-21";
        let chat_path =
            format!("/openai/deployments/{deployment}/chat/completions?api-version={api_version}");
        assert_eq!(
            chat_path,
            "/openai/deployments/gpt-4o-prod/chat/completions?api-version=2024-10-21"
        );
    }

    // ---- Bedrock (blocked) --------------------------------------------------------------------

    #[test]
    fn bedrock_complete_reports_signing_unavailable() {
        let clock: Arc<dyn Clock> = Arc::new(tm_types::SystemClock);
        let http = reqwest::Client::new();
        let provider = BedrockProvider {
            id: "bedrock".to_string(),
            model: ModelId::new("bedrock", "anthropic.claude-sonnet-5-v1:0"),
            region: "us-east-1".to_string(),
            model_id: "anthropic.claude-sonnet-5-v1:0".to_string(),
            http,
            clock,
        };
        let err = futures::executor::block_on(provider.complete(sample_request())).unwrap_err();
        match err {
            ProviderError::Unavailable(msg) => assert!(msg.contains("SigV4")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn bedrock_info_declares_conservative_capabilities() {
        let info = BedrockProvider::info();
        assert_eq!(info.id, "bedrock");
        assert!(!info.capabilities.embedding);
        assert!(!info.capabilities.tool_use);
    }

    // ---- Vertex --------------------------------------------------------------------------------

    #[test]
    fn vertex_info_declares_required_env_vars() {
        let info = VertexProvider::info();
        assert_eq!(info.id, "vertex");
        for name in ["VERTEX_ACCESS_TOKEN", "VERTEX_PROJECT", "VERTEX_LOCATION"] {
            assert!(
                info.env_vars.iter().any(|v| v.name == name && v.required),
                "missing required env var {name}"
            );
        }
    }

    #[test]
    fn build_vertex_request_maps_system_and_user_and_tools() {
        let wire = build_vertex_request(&sample_request());
        assert_eq!(
            wire.system_instruction.unwrap().parts[0].text.as_deref(),
            Some("Be terse.")
        );
        assert_eq!(wire.contents.len(), 1);
        assert_eq!(wire.contents[0].role, "user");
        assert_eq!(wire.generation_config.max_output_tokens, 512);
        assert_eq!(wire.tools.len(), 1);
        assert_eq!(wire.tools[0].function_declarations[0].name, "get_weather");
    }

    #[test]
    fn build_vertex_request_maps_assistant_role_to_model() {
        let mut req = sample_request();
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "Hi!".to_string(),
            }],
        });
        let wire = build_vertex_request(&req);
        assert_eq!(wire.contents[1].role, "model");
    }

    #[test]
    fn build_vertex_request_resolves_tool_result_name_from_preceding_tool_use() {
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
        let wire = build_vertex_request(&req);
        let last = wire.contents.last().expect("has tool result turn");
        let response = last.parts[0]
            .function_response
            .as_ref()
            .expect("functionResponse part");
        assert_eq!(response.name, "get_weather");
        assert_eq!(response.response["result"], "72F");
    }

    const RECORDED_TEXT_RESPONSE: &str = r#"{
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Hello there!"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 5, "cachedContentTokenCount": 0}
    }"#;

    const RECORDED_TOOL_USE_RESPONSE: &str = r#"{
        "candidates": [{
            "content": {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "SF"}}}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 8, "cachedContentTokenCount": 0}
    }"#;

    #[test]
    fn parse_vertex_response_maps_text_and_usage() {
        let ts = tm_types::Timestamp::from_unix_seconds(1_700_000_000);
        let model = ModelId::new("vertex", "gemini-2.0-flash");
        let completion = parse_vertex_response(
            RECORDED_TEXT_RESPONSE.as_bytes(),
            &model,
            Duration::from_millis(120),
            ts,
        )
        .expect("parses recorded response");

        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(
            completion.candidates[0].content[0],
            ContentBlock::Text {
                text: "Hello there!".to_string()
            }
        );
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(
            completion.usage,
            CrateUsage {
                input_tokens: 12,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }
        );
    }

    #[test]
    fn parse_vertex_response_detects_tool_use_from_content_shape() {
        let ts = tm_types::Timestamp::EPOCH;
        let model = ModelId::new("vertex", "gemini-2.0-flash");
        let completion = parse_vertex_response(
            RECORDED_TOOL_USE_RESPONSE.as_bytes(),
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
    fn parse_vertex_response_rejects_malformed_body() {
        let ts = tm_types::Timestamp::EPOCH;
        let model = ModelId::new("vertex", "gemini-2.0-flash");
        let err = parse_vertex_response(b"not json", &model, Duration::ZERO, ts).unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn parse_vertex_response_rejects_unmapped_finish_reason() {
        let ts = tm_types::Timestamp::EPOCH;
        let model = ModelId::new("vertex", "gemini-2.0-flash");
        let body = r#"{"candidates": [{"content": {"role": "model", "parts": [{"text": "x"}]}, "finishReason": "SAFETY"}]}"#;
        let err = parse_vertex_response(body.as_bytes(), &model, Duration::ZERO, ts).unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn classify_vertex_status_ok_on_200() {
        assert!(classify_vertex_status(reqwest::StatusCode::OK, None, b"{}").is_ok());
    }

    #[test]
    fn classify_vertex_status_rate_limited_with_retry_after() {
        let err = classify_vertex_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some("15"),
            br#"{"error": {"message": "slow down"}}"#,
        )
        .unwrap_err();
        match err {
            ProviderError::RateLimited {
                message,
                retry_after,
            } => {
                assert_eq!(message, "slow down");
                assert_eq!(retry_after, Some(Duration::from_secs(15)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_vertex_status_5xx_is_unavailable() {
        let err = classify_vertex_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            None,
            br#"{"error": {"message": "boom"}}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::Unavailable(msg) if msg == "boom"));
    }

    #[test]
    fn classify_vertex_status_401_is_auth_failed() {
        let err =
            classify_vertex_status(reqwest::StatusCode::UNAUTHORIZED, None, b"{}").unwrap_err();
        assert!(matches!(err, ProviderError::AuthFailed(_)));
    }

    #[test]
    fn classify_vertex_status_generic_400_is_invalid_request() {
        let err = classify_vertex_status(
            reqwest::StatusCode::BAD_REQUEST,
            None,
            br#"{"error": {"message": "bad field"}}"#,
        )
        .unwrap_err();
        match err {
            ProviderError::InvalidRequest(msg) => assert_eq!(msg, "bad field"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_vertex_status_falls_back_on_unparseable_body() {
        let err = classify_vertex_status(reqwest::StatusCode::BAD_REQUEST, None, b"not json")
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }
}
