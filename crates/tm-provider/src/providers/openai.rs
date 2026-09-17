//! OpenAI, over the Chat Completions API via [`crate::providers::compat`].
//!
//! Env vars read by [`OpenAiProvider::from_env`]:
//! - `OPENAI_API_KEY` (required) — Bearer key.
//! - `OPENAI_BASE_URL` (optional, default `"https://api.openai.com/v1"`) — override for
//!   Azure-adjacent proxies or local OpenAI-shaped gateways that are *not* Azure OpenAI itself
//!   (Azure OpenAI proper is [`crate::providers::cloud::AzureOpenAiProvider`], which needs a
//!   different auth header and URL shape compat's simple base-URL override can't express).
//! - `OPENAI_ORGANIZATION` (optional) — sent as the `OpenAI-Organization` header.
//! - `OPENAI_PROJECT` (optional) — sent as the `OpenAI-Project` header.
//!
//! `model` (which concrete model, e.g. `"gpt-4o"`) is not read from the environment: it comes in
//! as the `model: ModelId` constructor argument, exactly like
//! [`crate::anthropic::AnthropicProvider::from_env`], because one `providers.toml` role table can
//! route different roles to different OpenAI models while all of them share one API key.
//!
//! # IMPL: a known gap in the shared compat core
//!
//! OpenAI's reasoning-model families (the `o1`/`o3`/`gpt-5`-style lines) reject `max_tokens` and
//! require `max_completion_tokens` instead; [`compat::WireRequest`] always serializes the field as
//! `max_tokens`. `compat.rs` does not special-case this (it is meant to work across eight
//! different backends' shared dialect, and only OpenAI itself draws this particular line). Two
//! ways to handle it, neither of which is a design decision left open by accident — pick based on
//! how many reasoning-model roles actually get routed here:
//! 1. If only classic chat models are ever routed to this provider, do nothing; `max_tokens` is
//!    correct for them.
//! 2. If a reasoning model needs to be reachable too, serialize [`compat::build_wire_request`]'s
//!    output to a [`serde_json::Value`], rename the `max_tokens` key to `max_completion_tokens`
//!    when `self.model.model` matches a reasoning-family prefix, and send that `Value` instead of
//!    the typed struct (`reqwest::RequestBuilder::json` accepts any `Serialize`, including
//!    `serde_json::Value`).

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::CompatProvider;
use crate::providers::ProviderInfo;
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// OpenAI's Chat Completions API. See the module docs for env vars and the `max_tokens` /
/// `max_completion_tokens` caveat.
pub struct OpenAiProvider {
    compat: CompatProvider,
}

impl OpenAiProvider {
    // IMPL:
    // 1. Read OPENAI_API_KEY; missing -> `Err(crate::providers::missing_env_var("OPENAI_API_KEY"))`.
    // 2. Read OPENAI_BASE_URL, defaulting to "https://api.openai.com/v1".
    // 3. Build `compat::CompatConfig::new("openai", base_url, model.model)`
    //    `.with_api_key(key)` (default AuthStyle::Bearer is correct — leave as-is).
    // 4. If OPENAI_ORGANIZATION is set, `.with_extra_header("OpenAI-Organization", value)`;
    //    likewise OPENAI_PROJECT -> `.with_extra_header("OpenAI-Project", value)`.
    // 5. `CompatProvider::new(config, clock)` and wrap it in `OpenAiProvider { compat }`.
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let _ = (model, clock);
        todo!("read OPENAI_API_KEY / OPENAI_BASE_URL / OPENAI_ORGANIZATION / OPENAI_PROJECT and build a CompatProvider, per the IMPL comment above")
    }

    // IMPL: return the literal ProviderInfo below (already fully specified — this is metadata,
    // not provider logic, so there is no remaining design decision):
    // ProviderInfo {
    //     id: "openai",
    //     display_name: "OpenAI",
    //     env_vars: &[
    //         EnvVarRequirement { name: "OPENAI_API_KEY", required: true, description: "Bearer API key" },
    //         EnvVarRequirement { name: "OPENAI_BASE_URL", required: false, description: "Override the default https://api.openai.com/v1" },
    //         EnvVarRequirement { name: "OPENAI_ORGANIZATION", required: false, description: "Sent as the OpenAI-Organization header" },
    //         EnvVarRequirement { name: "OPENAI_PROJECT", required: false, description: "Sent as the OpenAI-Project header" },
    //     ],
    //     capabilities: Capabilities { completion: true, embedding: true, streaming: true, tool_use: true, vision: false },
    // }
    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        todo!("return the literal ProviderInfo from the IMPL comment above")
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    // IMPL: `self.compat.id()`.
    fn id(&self) -> &str {
        todo!("delegate to self.compat.id()")
    }

    // IMPL: `self.compat.complete(req).await`.
    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let _ = req;
        todo!("delegate to self.compat.complete(req).await")
    }

    // IMPL: `self.compat.embed(req).await`.
    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError> {
        let _ = req;
        todo!("delegate to self.compat.embed(req).await")
    }
}
