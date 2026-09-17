//! Together AI, Fireworks AI and Hugging Face's inference router: three serverless
//! multi-model-vendor hosts, all OpenAI-Chat-Completions-shaped enough to build on
//! [`crate::providers::compat`]. Grouped as alternative `explorer.cheap` / `serverless` capacity,
//! not because they share infrastructure.
//!
//! [`TogetherProvider`] env vars:
//! - `TOGETHER_API_KEY` (required) — Bearer key.
//! - `TOGETHER_BASE_URL` (optional, default `"https://api.together.xyz/v1"`).
//!
//! [`FireworksProvider`] env vars:
//! - `FIREWORKS_API_KEY` (required) — Bearer key.
//! - `FIREWORKS_BASE_URL` (optional, default `"https://api.fireworks.ai/inference/v1"`).
//!
//! [`HuggingFaceProvider`] env vars:
//! - `HF_TOKEN` (required) — Bearer key.
//! - `HF_BASE_URL` (optional, default `"https://router.huggingface.co/v1"` — HF's unified
//!   OpenAI-compatible router in front of many underlying inference providers).
//!
//! # IMPL: capability variance is real here, not sloppiness
//!
//! Together and Fireworks both host arbitrary open-weight models with wildly different tool-use
//! and embedding support depending on *which* model a `providers.toml` candidate names — the
//! [`crate::providers::ProviderInfo::capabilities`] this module reports is necessarily an
//! optimistic default for "a reasonably capable instruct model on this host", not a guarantee.
//! Hugging Face's router is the most heterogeneous of the three (it fans out to many distinct
//! backend runtimes); [`HuggingFaceProvider::info`] reports `tool_use: false` for that reason —
//! flip it per-candidate only once a specific routed model's support is confirmed.

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::CompatProvider;
use crate::providers::ProviderInfo;
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// Together AI.
pub struct TogetherProvider {
    compat: CompatProvider,
}

impl TogetherProvider {
    // IMPL:
    // 1. Read TOGETHER_API_KEY; missing -> `Err(crate::providers::missing_env_var(...))`.
    // 2. Read TOGETHER_BASE_URL, defaulting to "https://api.together.xyz/v1".
    // 3. `compat::CompatConfig::new("together", base_url, model.model).with_api_key(key)`
    //    (Together serves `/v1/embeddings`; leave `embeddings_path` at its default).
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire TOGETHER_API_KEY / TOGETHER_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "together",
    //     display_name: "Together AI",
    //     env_vars: &[
    //         EnvVarRequirement { name: "TOGETHER_API_KEY", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "TOGETHER_BASE_URL", required: false, description: "Override the default https://api.together.xyz/v1" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for TogetherProvider {
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

/// Fireworks AI.
pub struct FireworksProvider {
    compat: CompatProvider,
}

impl FireworksProvider {
    // IMPL:
    // 1. Read FIREWORKS_API_KEY; missing -> `Err(crate::providers::missing_env_var(...))`.
    // 2. Read FIREWORKS_BASE_URL, defaulting to "https://api.fireworks.ai/inference/v1".
    // 3. `compat::CompatConfig::new("fireworks", base_url, model.model).with_api_key(key)`
    //    (Fireworks serves `/inference/v1/embeddings`; leave `embeddings_path` at its default).
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire FIREWORKS_API_KEY / FIREWORKS_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "fireworks",
    //     display_name: "Fireworks AI",
    //     env_vars: &[
    //         EnvVarRequirement { name: "FIREWORKS_API_KEY", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "FIREWORKS_BASE_URL", required: false, description: "Override the default https://api.fireworks.ai/inference/v1" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for FireworksProvider {
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

/// Hugging Face's unified inference router.
pub struct HuggingFaceProvider {
    compat: CompatProvider,
}

impl HuggingFaceProvider {
    // IMPL:
    // 1. Read HF_TOKEN; missing -> `Err(crate::providers::missing_env_var("HF_TOKEN"))`.
    // 2. Read HF_BASE_URL, defaulting to "https://router.huggingface.co/v1".
    // 3. `compat::CompatConfig::new("huggingface", base_url, model.model).with_api_key(token)`
    //    `.without_embeddings()` (the router's embeddings support is inconsistent across backing
    //    runtimes; do not claim it generically — a specific known-good model can override this
    //    per instance if needed later).
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire HF_TOKEN / HF_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "huggingface",
    //     display_name: "Hugging Face",
    //     env_vars: &[
    //         EnvVarRequirement { name: "HF_TOKEN", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "HF_BASE_URL", required: false, description: "Override the default https://router.huggingface.co/v1" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: false, streaming: true, tool_use: false, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for HuggingFaceProvider {
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
