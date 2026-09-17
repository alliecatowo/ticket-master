//! Groq and Cerebras: two inference-speed-optimized OpenAI-Chat-Completions-shaped backends,
//! built on [`crate::providers::compat`]. Grouped together because they exist in `providers.toml`
//! for the same reason — low-latency `coder.fast` / `summarizer.cheap` candidates — not because
//! they share infrastructure.
//!
//! [`GroqProvider`] env vars:
//! - `GROQ_API_KEY` (required) — Bearer key.
//! - `GROQ_BASE_URL` (optional, default `"https://api.groq.com/openai/v1"`).
//!
//! [`CerebrasProvider`] env vars:
//! - `CEREBRAS_API_KEY` (required) — Bearer key.
//! - `CEREBRAS_BASE_URL` (optional, default `"https://api.cerebras.ai/v1"`).
//!
//! Neither backend serves an OpenAI-compatible embeddings endpoint as of this writing.

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::CompatProvider;
use crate::providers::ProviderInfo;
use crate::types::{Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError};

/// Groq — LPU-backed low-latency inference.
pub struct GroqProvider {
    compat: CompatProvider,
}

impl GroqProvider {
    // IMPL:
    // 1. Read GROQ_API_KEY; missing -> `Err(crate::providers::missing_env_var("GROQ_API_KEY"))`.
    // 2. Read GROQ_BASE_URL, defaulting to "https://api.groq.com/openai/v1".
    // 3. `compat::CompatConfig::new("groq", base_url, model.model).with_api_key(key)`
    //    `.without_embeddings()`.
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire GROQ_API_KEY / GROQ_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "groq",
    //     display_name: "Groq",
    //     env_vars: &[
    //         EnvVarRequirement { name: "GROQ_API_KEY", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "GROQ_BASE_URL", required: false, description: "Override the default https://api.groq.com/openai/v1" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: false, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for GroqProvider {
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

/// Cerebras — wafer-scale-engine-backed low-latency inference.
pub struct CerebrasProvider {
    compat: CompatProvider,
}

impl CerebrasProvider {
    // IMPL:
    // 1. Read CEREBRAS_API_KEY; missing -> `Err(crate::providers::missing_env_var(...))`.
    // 2. Read CEREBRAS_BASE_URL, defaulting to "https://api.cerebras.ai/v1".
    // 3. `compat::CompatConfig::new("cerebras", base_url, model.model).with_api_key(key)`
    //    `.without_embeddings()`.
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire CEREBRAS_API_KEY / CEREBRAS_BASE_URL, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "cerebras",
    //     display_name: "Cerebras",
    //     env_vars: &[
    //         EnvVarRequirement { name: "CEREBRAS_API_KEY", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "CEREBRAS_BASE_URL", required: false, description: "Override the default https://api.cerebras.ai/v1" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: false, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for CerebrasProvider {
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
