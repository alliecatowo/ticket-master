//! Azure OpenAI, AWS Bedrock and Google Vertex AI: three "the model vendor, but behind a cloud
//! account" backends. Only [`AzureOpenAiProvider`] can build on
//! [`crate::providers::compat`] — Bedrock and Vertex both require request-signing schemes this
//! crate has no dependency to implement (see each struct's doc comment for exactly what is
//! missing and why this agent should not add it unilaterally, per the workspace's
//! no-new-dependencies rule for anyone but the scaffold owner).

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::CompatProvider;
use crate::providers::ProviderInfo;
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

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
/// # IMPL
/// 1. Read the four env vars above (three required, one optional with the given default);
///    missing required ones -> `Err(crate::providers::missing_env_var(name))`.
/// 2. `let chat_path = format!("/openai/deployments/{deployment}/chat/completions?api-version={version}");`
///    and similarly for embeddings:
///    `format!("/openai/deployments/{deployment}/embeddings?api-version={version}")` — Azure uses
///    one deployment per capability, so if this provider is only ever routed to a chat role,
///    `.without_embeddings()` is simpler than assuming the same deployment serves both; document
///    whichever choice is made in this doc comment when implemented.
/// 3. `compat::CompatConfig::new("azure-openai", endpoint, model.model)`
///    `.with_api_key(key).with_auth_style(AuthStyle::Header("api-key".to_string()))`
///    `.with_chat_path(chat_path)` (and `.with_embeddings_path(embed_path)` if not dropping
///    embeddings per the point above).
/// 4. `CompatProvider::new(config, clock)`.
pub struct AzureOpenAiProvider {
    compat: CompatProvider,
}

impl AzureOpenAiProvider {
    /// Build a provider for `model`, reading configuration from the environment. See the struct
    /// doc comment's `# IMPL` section for the exact steps.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire AZURE_OPENAI_* env vars and the deployment-scoped path, per the struct doc comment")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "azure-openai",
    //     display_name: "Azure OpenAI",
    //     env_vars: &[
    //         EnvVarRequirement { name: "AZURE_OPENAI_API_KEY", required: true, description: "Resource API key" },
    //         EnvVarRequirement { name: "AZURE_OPENAI_ENDPOINT", required: true, description: "https://<resource>.openai.azure.com, no path" },
    //         EnvVarRequirement { name: "AZURE_OPENAI_DEPLOYMENT", required: true, description: "Deployment name (not the underlying model id)" },
    //         EnvVarRequirement { name: "AZURE_OPENAI_API_VERSION", required: false, description: "Override the default 2024-10-21" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for AzureOpenAiProvider {
    fn id(&self) -> &str {
        todo!("delegate to self.compat.id()")
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let _ = req;
        todo!("delegate to self.compat.complete(req).await")
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let _ = req;
        todo!("delegate to self.compat.embed(req).await")
    }
}

/// AWS Bedrock (model-agnostic gateway; this struct targets the Bedrock Runtime `InvokeModel` /
/// `Converse` API for one specific underlying model family per instance, same one-model-per-id
/// convention as the rest of this crate).
///
/// # IMPL: blocked on a missing dependency — do not add it yourself
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
    model: ModelId,
    region: String,
    model_id: String,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
}

impl BedrockProvider {
    /// Build a provider for `model`, reading configuration from the environment. Blocked on the
    /// missing SigV4 dependency described in the struct doc comment; do not attempt a hand-rolled
    /// signer without one.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("blocked on a SigV4-capable dependency (hmac/sha2 or an AWS SDK crate) not present in Cargo.toml; see the struct doc comment")
    }

    // IMPL: return the literal ProviderInfo below (capabilities are conservative until a specific
    // model family's request shape is implemented; narrow `tool_use`/`streaming` per family once
    // that's known).
    // ProviderInfo {
    //     id: "bedrock",
    //     display_name: "AWS Bedrock",
    //     env_vars: &[
    //         EnvVarRequirement { name: "AWS_ACCESS_KEY_ID", required: true, description: "AWS access key for SigV4 signing" },
    //         EnvVarRequirement { name: "AWS_SECRET_ACCESS_KEY", required: true, description: "AWS secret key for SigV4 signing" },
    //         EnvVarRequirement { name: "AWS_SESSION_TOKEN", required: false, description: "Session token for temporary credentials" },
    //         EnvVarRequirement { name: "AWS_REGION", required: true, description: "Bedrock Runtime region, e.g. us-east-1" },
    //         EnvVarRequirement { name: "BEDROCK_MODEL_ID", required: true, description: "Bedrock's own model id string" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: false, streaming: true, tool_use: false, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let _ = req;
        todo!("blocked on SigV4 signing; see the struct doc comment")
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let _ = req;
        todo!("blocked on SigV4 signing; see the struct doc comment")
    }
}

/// Google Vertex AI's Gemini endpoint — same request/response JSON shape as
/// [`crate::providers::gemini::GeminiProvider`] (Vertex mirrors the public Gemini API for the
/// `generateContent`/`embedContent` routes), but reached via a project/location-scoped URL and
/// authenticated with a Google OAuth2 bearer token instead of a static API key.
///
/// # IMPL: two paths, one blocked on a missing dependency
///
/// - **Unblocked path (implement this first):** accept a pre-minted, externally-refreshed access
///   token via `VERTEX_ACCESS_TOKEN` (e.g. produced out-of-band by `gcloud auth
///   print-access-token` on a refresh cadence outside this process). Send it as
///   `Authorization: Bearer {token}`. This needs no new dependency and covers real usage today.
/// - **Blocked path (do not attempt without a new dependency):** minting a token directly from a
///   `GOOGLE_APPLICATION_CREDENTIALS` service-account JSON file requires signing a JWT with
///   RS256, which needs an RSA/JWT-signing crate this crate does not depend on. As with
///   [`BedrockProvider`], flag this to the manifest owner rather than adding one from this seat.
///
/// Env vars for the unblocked path: `VERTEX_ACCESS_TOKEN` (required), `VERTEX_PROJECT_ID`
/// (required), `VERTEX_LOCATION` (required, e.g. `"us-central1"`).
/// Base URL: `format!("https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models")`,
/// then `{base}/{model}:generateContent` / `:embedContent`, same body shapes as
/// [`crate::providers::gemini`] documents — reuse that module's pure translation functions if they
/// are `pub` by the time this is implemented, rather than re-deriving the same mapping here.
pub struct VertexProvider {
    id: String,
    model: ModelId,
    project_id: String,
    location: String,
    access_token: String,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
}

impl VertexProvider {
    /// Build a provider for `model`, reading configuration from the environment (the
    /// pre-minted-token path only; see the struct doc comment).
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire VERTEX_ACCESS_TOKEN / VERTEX_PROJECT_ID / VERTEX_LOCATION, per the struct doc comment")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "vertex",
    //     display_name: "Google Vertex AI",
    //     env_vars: &[
    //         EnvVarRequirement { name: "VERTEX_ACCESS_TOKEN", required: true, description: "Pre-minted OAuth2 bearer token (see struct docs for the service-account path, currently blocked)" },
    //         EnvVarRequirement { name: "VERTEX_PROJECT_ID", required: true, description: "GCP project id" },
    //         EnvVarRequirement { name: "VERTEX_LOCATION", required: true, description: "Vertex AI region, e.g. us-central1" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for VertexProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let _ = req;
        todo!("implement per crate::providers::gemini's generateContent wire shape, over the Vertex-scoped URL described in the struct doc comment")
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let _ = req;
        todo!("implement per crate::providers::gemini's embedContent wire shape, over the Vertex-scoped URL described in the struct doc comment")
    }
}
