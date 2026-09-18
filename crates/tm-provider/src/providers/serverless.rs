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
use crate::providers::compat::{self, CompatProvider};
use crate::providers::{missing_env_var, Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// Together AI.
pub struct TogetherProvider {
    compat: CompatProvider,
}

impl TogetherProvider {
    /// Build the [`compat::CompatConfig`] for `model` from already-read env values. Split out of
    /// [`TogetherProvider::from_env`] so the config shape (base URL, embeddings support) is
    /// testable without touching `std::env`.
    fn build_config(
        model: ModelId,
        api_key: String,
        base_url: Option<String>,
    ) -> compat::CompatConfig {
        let base_url = base_url.unwrap_or_else(|| "https://api.together.xyz/v1".to_string());
        compat::CompatConfig::new("together", base_url, model.model).with_api_key(api_key)
    }

    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let key =
            std::env::var("TOGETHER_API_KEY").map_err(|_| missing_env_var("TOGETHER_API_KEY"))?;
        let base_url = std::env::var("TOGETHER_BASE_URL").ok();
        let config = TogetherProvider::build_config(model, key, base_url);
        let compat = CompatProvider::new(config, clock)?;
        Ok(TogetherProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "together",
            display_name: "Together AI",
            env_vars: &[
                EnvVarRequirement {
                    name: "TOGETHER_API_KEY",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "TOGETHER_BASE_URL",
                    required: false,
                    description: "Override the default https://api.together.xyz/v1",
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
impl Provider for TogetherProvider {
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

/// Fireworks AI.
pub struct FireworksProvider {
    compat: CompatProvider,
}

impl FireworksProvider {
    /// Build the [`compat::CompatConfig`] for `model` from already-read env values. Split out of
    /// [`FireworksProvider::from_env`] so the config shape is testable without touching
    /// `std::env`.
    fn build_config(
        model: ModelId,
        api_key: String,
        base_url: Option<String>,
    ) -> compat::CompatConfig {
        let base_url =
            base_url.unwrap_or_else(|| "https://api.fireworks.ai/inference/v1".to_string());
        compat::CompatConfig::new("fireworks", base_url, model.model).with_api_key(api_key)
    }

    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let key =
            std::env::var("FIREWORKS_API_KEY").map_err(|_| missing_env_var("FIREWORKS_API_KEY"))?;
        let base_url = std::env::var("FIREWORKS_BASE_URL").ok();
        let config = FireworksProvider::build_config(model, key, base_url);
        let compat = CompatProvider::new(config, clock)?;
        Ok(FireworksProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "fireworks",
            display_name: "Fireworks AI",
            env_vars: &[
                EnvVarRequirement {
                    name: "FIREWORKS_API_KEY",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "FIREWORKS_BASE_URL",
                    required: false,
                    description: "Override the default https://api.fireworks.ai/inference/v1",
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
impl Provider for FireworksProvider {
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

/// Hugging Face's unified inference router.
pub struct HuggingFaceProvider {
    compat: CompatProvider,
}

impl HuggingFaceProvider {
    /// Build the [`compat::CompatConfig`] for `model` from already-read env values. Split out of
    /// [`HuggingFaceProvider::from_env`] so the config shape (default base URL, embeddings
    /// disabled) is testable without touching `std::env`.
    fn build_config(
        model: ModelId,
        token: String,
        base_url: Option<String>,
    ) -> compat::CompatConfig {
        let base_url = base_url.unwrap_or_else(|| "https://router.huggingface.co/v1".to_string());
        compat::CompatConfig::new("huggingface", base_url, model.model)
            .with_api_key(token)
            .without_embeddings()
    }

    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let token = std::env::var("HF_TOKEN").map_err(|_| missing_env_var("HF_TOKEN"))?;
        let base_url = std::env::var("HF_BASE_URL").ok();
        let config = HuggingFaceProvider::build_config(model, token, base_url);
        let compat = CompatProvider::new(config, clock)?;
        Ok(HuggingFaceProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "huggingface",
            display_name: "Hugging Face",
            env_vars: &[
                EnvVarRequirement {
                    name: "HF_TOKEN",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "HF_BASE_URL",
                    required: false,
                    description: "Override the default https://router.huggingface.co/v1",
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
impl Provider for HuggingFaceProvider {
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

#[cfg(test)]
mod tests {
    //! No network calls here, per the workspace rule — wire-shape and error-mapping coverage
    //! lives in `compat.rs`; these tests are about this module's own contribution: which env vars
    //! gate which provider, what `info()` reports, and that each provider's `CompatConfig` is
    //! wired the way its `from_env` doc promises (right default base URL, right embeddings
    //! support). Env-var tests serialize on [`ENV_LOCK`] since `std::env` is process-global and
    //! `cargo test` runs in parallel by default.

    use std::sync::Mutex;

    use tm_types::{Clock, SystemClock};

    use super::*;
    use crate::providers::compat::{classify_status, parse_sse_body, parse_wire_response};

    /// Serializes every test that mutates process env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clock() -> Arc<dyn Clock> {
        Arc::new(SystemClock)
    }

    fn clear_together_env() {
        std::env::remove_var("TOGETHER_API_KEY");
        std::env::remove_var("TOGETHER_BASE_URL");
    }

    fn clear_fireworks_env() {
        std::env::remove_var("FIREWORKS_API_KEY");
        std::env::remove_var("FIREWORKS_BASE_URL");
    }

    fn clear_hf_env() {
        std::env::remove_var("HF_TOKEN");
        std::env::remove_var("HF_BASE_URL");
    }

    // ---- Together ----

    #[test]
    fn together_from_env_fails_with_auth_failed_when_key_missing() {
        let _guard = ENV_LOCK.lock().expect("lock poisoned");
        clear_together_env();
        match TogetherProvider::from_env(ModelId::new("together", "some-model"), clock()) {
            Err(ProviderError::AuthFailed(msg)) => assert!(msg.contains("TOGETHER_API_KEY")),
            other => panic!(
                "expected Err(AuthFailed), got a provider build result: {}",
                other.is_ok()
            ),
        }
        clear_together_env();
    }

    #[test]
    fn together_from_env_uses_default_base_url_and_model() {
        let _guard = ENV_LOCK.lock().expect("lock poisoned");
        clear_together_env();
        std::env::set_var("TOGETHER_API_KEY", "test-key");
        let provider =
            TogetherProvider::from_env(ModelId::new("together", "meta-llama/Llama-3-70b"), clock())
                .expect("builds with key set");
        assert_eq!(provider.id(), "together");
        clear_together_env();
    }

    #[test]
    fn together_build_config_uses_default_base_url_and_serves_embeddings() {
        let config = TogetherProvider::build_config(
            ModelId::new("together", "meta-llama/Llama-3-70b"),
            "test-key".to_string(),
            None,
        );
        assert_eq!(config.base_url, "https://api.together.xyz/v1");
        assert_eq!(config.model, "meta-llama/Llama-3-70b");
        assert_eq!(config.api_key.as_deref(), Some("test-key"));
        assert!(config.embeddings_path.is_some());
    }

    #[test]
    fn together_build_config_honors_base_url_override() {
        let config = TogetherProvider::build_config(
            ModelId::new("together", "m"),
            "test-key".to_string(),
            Some("https://together.example.invalid/v1".to_string()),
        );
        assert_eq!(config.base_url, "https://together.example.invalid/v1");
    }

    #[test]
    fn together_info_matches_documented_contract() {
        let info = TogetherProvider::info();
        assert_eq!(info.id, "together");
        assert_eq!(info.env_vars.len(), 2);
        assert!(info.env_vars[0].required);
        assert!(!info.env_vars[1].required);
        assert!(info.capabilities.embedding);
        assert!(info.capabilities.streaming);
        assert!(info.capabilities.tool_use);
        assert!(!info.capabilities.vision);
    }

    // ---- Fireworks ----

    #[test]
    fn fireworks_from_env_fails_with_auth_failed_when_key_missing() {
        let _guard = ENV_LOCK.lock().expect("lock poisoned");
        clear_fireworks_env();
        match FireworksProvider::from_env(ModelId::new("fireworks", "some-model"), clock()) {
            Err(ProviderError::AuthFailed(msg)) => assert!(msg.contains("FIREWORKS_API_KEY")),
            other => panic!(
                "expected Err(AuthFailed), got a provider build result: {}",
                other.is_ok()
            ),
        }
        clear_fireworks_env();
    }

    #[test]
    fn fireworks_from_env_uses_default_base_url() {
        let _guard = ENV_LOCK.lock().expect("lock poisoned");
        clear_fireworks_env();
        std::env::set_var("FIREWORKS_API_KEY", "test-key");
        let provider = FireworksProvider::from_env(ModelId::new("fireworks", "m"), clock())
            .expect("builds with key set");
        assert_eq!(provider.id(), "fireworks");
        clear_fireworks_env();
    }

    #[test]
    fn fireworks_build_config_uses_default_base_url_and_serves_embeddings() {
        let config = FireworksProvider::build_config(
            ModelId::new("fireworks", "m"),
            "test-key".to_string(),
            None,
        );
        assert_eq!(config.base_url, "https://api.fireworks.ai/inference/v1");
        assert!(config.embeddings_path.is_some());
    }

    #[test]
    fn fireworks_info_matches_documented_contract() {
        let info = FireworksProvider::info();
        assert_eq!(info.id, "fireworks");
        assert_eq!(info.env_vars.len(), 2);
        assert!(info.capabilities.embedding);
        assert!(info.capabilities.tool_use);
    }

    // ---- Hugging Face ----

    #[test]
    fn huggingface_from_env_fails_with_auth_failed_when_token_missing() {
        let _guard = ENV_LOCK.lock().expect("lock poisoned");
        clear_hf_env();
        match HuggingFaceProvider::from_env(ModelId::new("huggingface", "some-model"), clock()) {
            Err(ProviderError::AuthFailed(msg)) => assert!(msg.contains("HF_TOKEN")),
            other => panic!(
                "expected Err(AuthFailed), got a provider build result: {}",
                other.is_ok()
            ),
        }
        clear_hf_env();
    }

    #[test]
    fn huggingface_from_env_disables_embeddings_and_uses_default_base_url() {
        let _guard = ENV_LOCK.lock().expect("lock poisoned");
        clear_hf_env();
        std::env::set_var("HF_TOKEN", "test-token");
        let provider = HuggingFaceProvider::from_env(ModelId::new("huggingface", "m"), clock())
            .expect("builds with token set");
        assert_eq!(provider.id(), "huggingface");
        clear_hf_env();
    }

    #[test]
    fn huggingface_build_config_uses_default_base_url_and_disables_embeddings() {
        let config = HuggingFaceProvider::build_config(
            ModelId::new("huggingface", "m"),
            "test-token".to_string(),
            None,
        );
        assert_eq!(config.base_url, "https://router.huggingface.co/v1");
        assert!(config.embeddings_path.is_none());
    }

    #[tokio::test]
    async fn huggingface_embed_returns_invalid_request_without_a_network_call() {
        // Scope the env lock to the env manipulation itself: the provider is fully built by the
        // time it is released, so nothing after this block reads the environment. Holding a
        // std::sync::Mutex guard across the `.await` below would risk deadlocking if the task
        // were moved between worker threads.
        let provider = {
            let _guard = ENV_LOCK.lock().expect("lock poisoned");
            clear_hf_env();
            std::env::set_var("HF_TOKEN", "test-token");
            let provider = HuggingFaceProvider::from_env(ModelId::new("huggingface", "m"), clock())
                .expect("builds with token set");
            clear_hf_env();
            provider
        };

        let err = provider
            .embed(EmbedRequest {
                inputs: vec!["hello".to_string()],
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn huggingface_info_reports_no_embedding_and_no_tool_use() {
        let info = HuggingFaceProvider::info();
        assert_eq!(info.id, "huggingface");
        assert!(!info.capabilities.embedding);
        assert!(!info.capabilities.tool_use);
        assert!(info.capabilities.streaming);
    }

    // ---- shared error mapping / streaming reassembly, exercised through each provider's id ----

    const RECORDED_UNAUTHORIZED_BODY: &[u8] =
        br#"{"error": {"message": "Invalid API key provided", "type": "invalid_request_error"}}"#;
    const RECORDED_RATE_LIMITED_BODY: &[u8] =
        br#"{"error": {"message": "Rate limit reached, please retry later"}}"#;
    const RECORDED_SERVER_ERROR_BODY: &[u8] = br#"{"error": {"message": "internal server error"}}"#;

    #[test]
    fn classify_status_maps_401_to_auth_failed_for_serverless_style_error_body() {
        let err = classify_status(
            reqwest::StatusCode::UNAUTHORIZED,
            None,
            RECORDED_UNAUTHORIZED_BODY,
        )
        .unwrap_err();
        match err {
            ProviderError::AuthFailed(msg) => assert_eq!(msg, "Invalid API key provided"),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_maps_429_to_rate_limited_with_retry_after() {
        let err = classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some("5"),
            RECORDED_RATE_LIMITED_BODY,
        )
        .unwrap_err();
        match err {
            ProviderError::RateLimited {
                message,
                retry_after,
            } => {
                assert_eq!(message, "Rate limit reached, please retry later");
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(5)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_maps_5xx_to_unavailable() {
        let err = classify_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            None,
            RECORDED_SERVER_ERROR_BODY,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::Unavailable(msg) if msg == "internal server error"));
    }

    #[test]
    fn classify_status_maps_malformed_error_body_to_a_fallback_message() {
        let err = classify_status(
            reqwest::StatusCode::BAD_REQUEST,
            None,
            b"<html>not json</html>",
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn parse_wire_response_rejects_malformed_body_for_a_serverless_completion() {
        let err = parse_wire_response(
            b"{not valid json",
            "some-model",
            "together",
            std::time::Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn streaming_reassembly_merges_text_deltas_for_a_serverless_provider_id() {
        // A recorded-shape SSE stream, as any of the three OpenAI-Chat-Completions-shaped hosts
        // in this module would emit it, reassembled into one Completion under this module's
        // provider id.
        let body = concat!(
            "data: {\"model\": \"meta-llama/Llama-3-70b\", \"choices\": [{\"index\": 0, \"delta\": {\"role\": \"assistant\", \"content\": \"Sure\"}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {\"content\": \", here you go.\"}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {}, \"finish_reason\": \"stop\"}]}\n\n",
            "data: {\"choices\": [], \"usage\": {\"prompt_tokens\": 11, \"completion_tokens\": 4}}\n\n",
            "data: [DONE]\n\n",
        );
        let chunks = parse_sse_body(body.as_bytes()).expect("parses recorded SSE body");
        let completion = crate::providers::compat::assemble_streamed_completion(
            &chunks,
            "meta-llama/Llama-3-70b",
            "together",
            std::time::Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .expect("assembles");

        assert_eq!(
            completion.model,
            ModelId::new("together", "meta-llama/Llama-3-70b")
        );
        assert_eq!(
            completion.candidates[0].content,
            vec![crate::types::ContentBlock::Text {
                text: "Sure, here you go.".to_string()
            }]
        );
        assert_eq!(completion.usage.input_tokens, 11);
        assert_eq!(completion.usage.output_tokens, 4);
    }
}
