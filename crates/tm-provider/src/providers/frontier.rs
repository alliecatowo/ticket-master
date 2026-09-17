//! DeepSeek, Mistral and xAI: three independent frontier-model vendors, all close enough to the
//! OpenAI Chat Completions dialect to build on [`crate::providers::compat`]. Grouped together as
//! alternative `coder.deep` / `planner.frontier` candidates, not because they share
//! infrastructure.
//!
//! [`DeepSeekProvider`] env vars:
//! - `DEEPSEEK_API_KEY` (required) — Bearer key.
//! - `DEEPSEEK_BASE_URL` (optional, default `"https://api.deepseek.com"`) — note: no `/v1`
//!   segment; DeepSeek's own docs mount `/chat/completions` directly under the bare host.
//!
//! [`MistralProvider`] env vars:
//! - `MISTRAL_API_KEY` (required) — Bearer key.
//! - `MISTRAL_BASE_URL` (optional, default `"https://api.mistral.ai/v1"`).
//!
//! [`XaiProvider`] env vars:
//! - `XAI_API_KEY` (required) — Bearer key.
//! - `XAI_BASE_URL` (optional, default `"https://api.x.ai/v1"`).

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::CompatProvider;
use crate::providers::ProviderInfo;
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// DeepSeek.
pub struct DeepSeekProvider {
    compat: CompatProvider,
}

impl DeepSeekProvider {
    // IMPL:
    // 1. Read DEEPSEEK_API_KEY; missing -> `Err(crate::providers::missing_env_var(...))`.
    // 2. Read DEEPSEEK_BASE_URL, defaulting to "https://api.deepseek.com" (no `/v1`).
    // 3. `compat::CompatConfig::new("deepseek", base_url, model.model).with_api_key(key)`
    //    `.without_embeddings()` (DeepSeek does not serve an OpenAI-compatible embeddings route).
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire DEEPSEEK_API_KEY / DEEPSEEK_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "deepseek",
    //     display_name: "DeepSeek",
    //     env_vars: &[
    //         EnvVarRequirement { name: "DEEPSEEK_API_KEY", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "DEEPSEEK_BASE_URL", required: false, description: "Override the default https://api.deepseek.com" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: false, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
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

/// Mistral.
pub struct MistralProvider {
    compat: CompatProvider,
}

impl MistralProvider {
    // IMPL:
    // 1. Read MISTRAL_API_KEY; missing -> `Err(crate::providers::missing_env_var(...))`.
    // 2. Read MISTRAL_BASE_URL, defaulting to "https://api.mistral.ai/v1".
    // 3. `compat::CompatConfig::new("mistral", base_url, model.model).with_api_key(key)`
    //    (Mistral does serve `/v1/embeddings`; leave `embeddings_path` at its default).
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire MISTRAL_API_KEY / MISTRAL_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "mistral",
    //     display_name: "Mistral",
    //     env_vars: &[
    //         EnvVarRequirement { name: "MISTRAL_API_KEY", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "MISTRAL_BASE_URL", required: false, description: "Override the default https://api.mistral.ai/v1" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for MistralProvider {
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

/// xAI (Grok).
pub struct XaiProvider {
    compat: CompatProvider,
}

impl XaiProvider {
    // IMPL:
    // 1. Read XAI_API_KEY; missing -> `Err(crate::providers::missing_env_var("XAI_API_KEY"))`.
    // 2. Read XAI_BASE_URL, defaulting to "https://api.x.ai/v1".
    // 3. `compat::CompatConfig::new("xai", base_url, model.model).with_api_key(key)`
    //    `.without_embeddings()` (xAI does not serve embeddings as of this writing).
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire XAI_API_KEY / XAI_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "xai",
    //     display_name: "xAI",
    //     env_vars: &[
    //         EnvVarRequirement { name: "XAI_API_KEY", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "XAI_BASE_URL", required: false, description: "Override the default https://api.x.ai/v1" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: false, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for XaiProvider {
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
