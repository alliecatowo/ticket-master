//! Cloudflare Workers AI, over its OpenAI-compatible endpoint, built on
//! [`crate::providers::compat`].
//!
//! Env vars read by [`CloudflareWorkersAiProvider::from_env`]:
//! - `CF_API_TOKEN` (required) — Bearer key (a Cloudflare API token scoped for Workers AI, not
//!   the account's Global API Key).
//! - `CF_ACCOUNT_ID` (required) — the account id is part of the URL path, not a header.
//!
//! # IMPL
//! 1. Read `CF_API_TOKEN` and `CF_ACCOUNT_ID`; either missing ->
//!    `Err(crate::providers::missing_env_var(name))`.
//! 2. `let base_url = format!("https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1");`
//!    — this is Cloudflare's OpenAI-compatible surface (added 2024), which serves both
//!    `/chat/completions` and `/embeddings` for the models that support each. There is also an
//!    older native `/ai/run/{model}` endpoint with a different, Cloudflare-specific response
//!    shape (`{"result": {"response": "..."}}`); prefer the `/ai/v1` compat surface above so this
//!    can build on `compat.rs` at all — do not fall back to `/ai/run` unless a specific model is
//!    confirmed missing from the compat surface, and if so, that fallback needs its own wire
//!    shape outside `compat`, not a `CompatConfig` tweak.
//! 3. `compat::CompatConfig::new("cloudflare", base_url, model.model).with_api_key(token)`
//!    (default `AuthStyle::Bearer` is correct; leave `embeddings_path` at its default — Workers AI
//!    serves `@cf/baai/bge-*` embedding models through this same compat surface).
//! 4. `CompatProvider::new(config, clock)`.

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::CompatProvider;
use crate::providers::ProviderInfo;
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// Cloudflare Workers AI. See the module doc comment's `# IMPL` section for the exact steps.
pub struct CloudflareWorkersAiProvider {
    compat: CompatProvider,
}

impl CloudflareWorkersAiProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("wire CF_API_TOKEN / CF_ACCOUNT_ID into the /ai/v1 compat surface, per the module doc comment")
    }

    // IMPL: return the literal ProviderInfo below.
    // ProviderInfo {
    //     id: "cloudflare",
    //     display_name: "Cloudflare Workers AI",
    //     env_vars: &[
    //         EnvVarRequirement { name: "CF_API_TOKEN", required: true, description: "Cloudflare API token scoped for Workers AI" },
    //         EnvVarRequirement { name: "CF_ACCOUNT_ID", required: true, description: "Cloudflare account id (part of the URL path)" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for CloudflareWorkersAiProvider {
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
