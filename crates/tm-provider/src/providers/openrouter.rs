//! OpenRouter and GitHub Models: two independently-billed gateways that both speak the OpenAI
//! Chat Completions dialect closely enough to build on [`crate::providers::compat`].
//!
//! [`OpenRouterProvider`] env vars:
//! - `OPENROUTER_API_KEY` (required) — Bearer key.
//! - `OPENROUTER_BASE_URL` (optional, default `"https://openrouter.ai/api/v1"`).
//! - `OPENROUTER_HTTP_REFERER` (optional) — sent as `HTTP-Referer`; OpenRouter uses this and
//!   `X-Title` for its public leaderboard attribution, not for auth.
//! - `OPENROUTER_APP_TITLE` (optional) — sent as `X-Title`.
//!
//! OpenRouter model ids are vendor-prefixed (e.g. `"anthropic/claude-sonnet-4.5"`,
//! `"openai/gpt-4o"`) — `model.model` is passed through to the wire verbatim; `providers.toml`
//! candidates routed here must already spell the model that way.
//!
//! [`GithubModelsProvider`] env vars:
//! - `GITHUB_TOKEN` (required) — a GitHub PAT or `GITHUB_TOKEN` from Actions, scoped for GitHub
//!   Models access. Sent as `Authorization: Bearer <token>`.
//! - `GITHUB_MODELS_BASE_URL` (optional, default `"https://models.github.ai/inference"`).

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::CompatProvider;
use crate::providers::ProviderInfo;
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// OpenRouter — a single API in front of dozens of upstream model vendors.
pub struct OpenRouterProvider {
    compat: CompatProvider,
}

impl OpenRouterProvider {
    // IMPL:
    // 1. Read OPENROUTER_API_KEY; missing -> `Err(crate::providers::missing_env_var(...))`.
    // 2. Read OPENROUTER_BASE_URL, defaulting to "https://openrouter.ai/api/v1".
    // 3. `compat::CompatConfig::new("openrouter", base_url, model.model).with_api_key(key)`.
    // 4. OpenRouter does not serve an OpenAI-compatible embeddings endpoint today ->
    //    `.without_embeddings()`.
    // 5. If OPENROUTER_HTTP_REFERER is set, `.with_extra_header("HTTP-Referer", value)`; likewise
    //    OPENROUTER_APP_TITLE -> `.with_extra_header("X-Title", value)`.
    // 6. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire OPENROUTER_API_KEY / OPENROUTER_BASE_URL / HTTP-Referer / X-Title, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "openrouter",
    //     display_name: "OpenRouter",
    //     env_vars: &[
    //         EnvVarRequirement { name: "OPENROUTER_API_KEY", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "OPENROUTER_BASE_URL", required: false, description: "Override the default https://openrouter.ai/api/v1" },
    //         EnvVarRequirement { name: "OPENROUTER_HTTP_REFERER", required: false, description: "Sent as the HTTP-Referer header" },
    //         EnvVarRequirement { name: "OPENROUTER_APP_TITLE", required: false, description: "Sent as the X-Title header" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: false, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    fn id(&self) -> &str {
        todo!("delegate to self.compat.id()")
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let _ = req;
        todo!("delegate to self.compat.complete(req).await")
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let _ = req;
        todo!("delegate to self.compat.embed(req).await (will return InvalidRequest since embeddings_path is None)")
    }
}

/// GitHub Models — GitHub's own model-hosting gateway, OpenAI-Chat-Completions-shaped.
pub struct GithubModelsProvider {
    compat: CompatProvider,
}

impl GithubModelsProvider {
    // IMPL:
    // 1. Read GITHUB_TOKEN; missing -> `Err(crate::providers::missing_env_var("GITHUB_TOKEN"))`.
    // 2. Read GITHUB_MODELS_BASE_URL, defaulting to "https://models.github.ai/inference".
    // 3. `compat::CompatConfig::new("github-models", base_url, model.model).with_api_key(token)`
    //    `.without_embeddings()` (GitHub Models does not serve embeddings).
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire GITHUB_TOKEN / GITHUB_MODELS_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "github-models",
    //     display_name: "GitHub Models",
    //     env_vars: &[
    //         EnvVarRequirement { name: "GITHUB_TOKEN", required: true, description: "GitHub PAT or Actions token scoped for GitHub Models" },
    //         EnvVarRequirement { name: "GITHUB_MODELS_BASE_URL", required: false, description: "Override the default https://models.github.ai/inference" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: false, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for GithubModelsProvider {
    fn id(&self) -> &str {
        todo!("delegate to self.compat.id()")
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let _ = req;
        todo!("delegate to self.compat.complete(req).await")
    }

    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let _ = req;
        todo!("delegate to self.compat.embed(req).await (will return InvalidRequest since embeddings_path is None)")
    }
}
