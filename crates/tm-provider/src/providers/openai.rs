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
use tm_auth::EnvApiKey;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::{CompatConfig, CompatProvider};
use crate::providers::{Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// OpenAI's Chat Completions API. See the module docs for env vars and the `max_tokens` /
/// `max_completion_tokens` caveat.
pub struct OpenAiProvider {
    compat: CompatProvider,
}

impl OpenAiProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        // The bearer API key goes through `tm_auth::EnvApiKey` (`SPEC.md` §28.2's auth-adapter
        // layer). `OPENAI_ORGANIZATION`/`OPENAI_PROJECT` stay plain `std::env::var` reads below:
        // they're routing identifiers OpenAI's API happens to read from headers, not secrets.
        let api_key = EnvApiKey::new("OPENAI_API_KEY")
            .resolve()
            .map_err(|_| crate::providers::missing_env_var("OPENAI_API_KEY"))?
            .expose_secret()
            .to_string();
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

        let mut config = CompatConfig::new("openai", base_url, model.model).with_api_key(api_key);
        if let Ok(org) = std::env::var("OPENAI_ORGANIZATION") {
            config = config.with_extra_header("OpenAI-Organization", org);
        }
        if let Ok(project) = std::env::var("OPENAI_PROJECT") {
            config = config.with_extra_header("OpenAI-Project", project);
        }

        let compat = CompatProvider::new(config, clock)?;
        Ok(OpenAiProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "openai",
            display_name: "OpenAI",
            env_vars: &[
                EnvVarRequirement {
                    name: "OPENAI_API_KEY",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "OPENAI_BASE_URL",
                    required: false,
                    description: "Override the default https://api.openai.com/v1",
                },
                EnvVarRequirement {
                    name: "OPENAI_ORGANIZATION",
                    required: false,
                    description: "Sent as the OpenAI-Organization header",
                },
                EnvVarRequirement {
                    name: "OPENAI_PROJECT",
                    required: false,
                    description: "Sent as the OpenAI-Project header",
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
impl Provider for OpenAiProvider {
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
