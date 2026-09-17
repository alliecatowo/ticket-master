//! Ollama, LM Studio and llama.cpp: three local inference servers, all speaking (a subset of) the
//! OpenAI Chat Completions dialect on `localhost`, built on [`crate::providers::compat`] with
//! [`crate::providers::compat::AuthStyle::None`] (no gateway sits in front of them, so there is
//! nothing to authenticate to by default).
//!
//! [`OllamaProvider`] env vars:
//! - `OLLAMA_HOST` (optional, default `"http://localhost:11434"`) — Ollama's OpenAI-compatible
//!   routes are mounted under `{OLLAMA_HOST}/v1`.
//!
//! [`LmStudioProvider`] env vars:
//! - `LM_STUDIO_HOST` (optional, default `"http://localhost:1234"`) — LM Studio's local server
//!   mounts its OpenAI-compatible routes under `{LM_STUDIO_HOST}/v1`.
//!
//! [`LlamaCppProvider`] env vars:
//! - `LLAMA_CPP_HOST` (optional, default `"http://localhost:8080"`) — `llama-server`'s
//!   OpenAI-compatible routes are mounted directly under `{LLAMA_CPP_HOST}/v1` when started with
//!   `--api-key`-less defaults.
//! - `LLAMA_CPP_API_KEY` (optional) — only set if `llama-server` was started with `--api-key`; if
//!   present, use [`crate::providers::compat::AuthStyle::Bearer`] instead of `None`.
//!
//! # IMPL: `ProviderInfo::is_configured` cannot detect these
//!
//! Every env var above is optional with a working default, which means
//! [`crate::providers::ProviderInfo::is_configured`] — `env_vars.iter().filter(required).all(...)`
//! over an **empty** filtered set — is vacuously `true` for all three structs here regardless of
//! whether the local server is actually running. [`crate::providers::registry::Registry`]
//! autodetection must not treat "no required env vars" as "always available" for this module: it
//! needs a real reachability probe (e.g. a short-timeout `GET {host}/v1/models`) before offering
//! any of these three as configured, unlike every other module in this crate where `is_configured`
//! alone is sufficient. This is called out here because it is the one module where the shared
//! contract's env-var-only detection genuinely does not work, not because the contract is wrong
//! for the other ten.

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::CompatProvider;
use crate::providers::ProviderInfo;
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// Ollama.
pub struct OllamaProvider {
    compat: CompatProvider,
}

impl OllamaProvider {
    // IMPL:
    // 1. Read OLLAMA_HOST, defaulting to "http://localhost:11434"; append "/v1" for the base URL.
    // 2. `compat::CompatConfig::new("ollama", base_url, model.model)`
    //    `.with_auth_style(AuthStyle::None)` (no `api_key` needed at all — do not call
    //    `.with_api_key`). Ollama serves `/v1/embeddings` for embedding-capable models pulled
    //    locally; leave `embeddings_path` at its default.
    // 3. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire OLLAMA_HOST with AuthStyle::None, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "ollama",
    //     display_name: "Ollama",
    //     env_vars: &[
    //         EnvVarRequirement { name: "OLLAMA_HOST", required: false, description: "Override the default http://localhost:11434" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract
    /// (and this module's docs for why `is_configured` alone is not enough here).
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for OllamaProvider {
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

/// LM Studio.
pub struct LmStudioProvider {
    compat: CompatProvider,
}

impl LmStudioProvider {
    // IMPL:
    // 1. Read LM_STUDIO_HOST, defaulting to "http://localhost:1234"; append "/v1" for base URL.
    // 2. `compat::CompatConfig::new("lm-studio", base_url, model.model)`
    //    `.with_auth_style(AuthStyle::None)`. LM Studio's embeddings support depends on the
    //    loaded model; leave `embeddings_path` at its default and let a failed call surface as a
    //    normal `ProviderError` rather than guessing `.without_embeddings()` up front.
    // 3. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire LM_STUDIO_HOST with AuthStyle::None, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "lm-studio",
    //     display_name: "LM Studio",
    //     env_vars: &[
    //         EnvVarRequirement { name: "LM_STUDIO_HOST", required: false, description: "Override the default http://localhost:1234" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract
    /// (and this module's docs for why `is_configured` alone is not enough here).
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for LmStudioProvider {
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

/// llama.cpp's `llama-server`.
pub struct LlamaCppProvider {
    compat: CompatProvider,
}

impl LlamaCppProvider {
    // IMPL:
    // 1. Read LLAMA_CPP_HOST, defaulting to "http://localhost:8080"; append "/v1" for base URL.
    // 2. Read optional LLAMA_CPP_API_KEY.
    // 3. `compat::CompatConfig::new("llama-cpp", base_url, model.model)`; if the API key is
    //    present, `.with_api_key(key)` (default AuthStyle::Bearer is then correct); if absent,
    //    `.with_auth_style(AuthStyle::None)`. `llama-server`'s embeddings route requires starting
    //    it with `--embedding`, which this module cannot detect — leave `embeddings_path` at its
    //    default and let a failed call surface normally, same reasoning as LM Studio above.
    // 4. `CompatProvider::new(config, clock)`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire LLAMA_CPP_HOST / LLAMA_CPP_API_KEY, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "llama-cpp",
    //     display_name: "llama.cpp",
    //     env_vars: &[
    //         EnvVarRequirement { name: "LLAMA_CPP_HOST", required: false, description: "Override the default http://localhost:8080" },
    //         EnvVarRequirement { name: "LLAMA_CPP_API_KEY", required: false, description: "Only needed if llama-server was started with --api-key" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract
    /// (and this module's docs for why `is_configured` alone is not enough here).
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for LlamaCppProvider {
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
