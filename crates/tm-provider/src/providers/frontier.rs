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
//!
//! # IMPL: a known gap, `reasoning_content` on DeepSeek's `deepseek-reasoner` models
//!
//! DeepSeek's reasoner models add a `reasoning_content` string next to the normal `content` field
//! on the response message (and as an incremental `delta.reasoning_content` on the streaming
//! path), carrying the model's chain-of-thought separately from its final answer. The natural
//! mapping is a dedicated reasoning [`crate::types::ContentBlock`] variant, but that enum has no
//! such variant today, and it is defined in `crate/tm-provider/src/types.rs`, which this module
//! does not own (see the workspace rule: touch only the file you were assigned). Threading
//! `reasoning_content` through also needs a hook `compat.rs` doesn't have: `CompatProvider::complete`
//! parses the response body internally and only ever returns the fully-shaped
//! [`crate::types::Completion`], never the raw bytes or the wire struct, so there is no seam here
//! to intercept an extra field without either duplicating `compat.rs`'s HTTP/retry/SSE machinery
//! (which the module docs on `compat.rs` explicitly ask callers not to do) or widening
//! `CompatConfig`/`CompatProvider` itself (out of scope for this file, same as the
//! `max_tokens`/`max_completion_tokens` gap `providers/openai.rs` documents for the same reason).
//!
//! Net effect today: [`DeepSeekProvider`] delegates straight to [`CompatProvider`], exactly like
//! [`MistralProvider`] and [`XaiProvider`]; `WireResponseMessage`'s `#[serde(default)]` fields mean
//! an unrecognized `reasoning_content` key is silently ignored by `serde_json`, so a reasoner
//! model's answer still comes through as plain `Text`, just without its reasoning trace. Wiring
//! `reasoning_content` up for real needs one of: (a) a `ContentBlock::Reasoning` variant added by
//! whichever agent owns `types.rs`, plus (b) either a `compat.rs` extension point that exposes the
//! parsed-but-not-yet-`Completion`-shaped wire response to a provider-supplied hook, or a
//! DeepSeek-specific response parser in this file that reuses `compat.rs`'s pure translation
//! functions (`map_finish_reason`, `wire_usage_to_usage`-equivalent) but not its private
//! `send_with_retry`. Neither (a) nor (b) is decided here; see this doc comment as the flag for
//! whoever picks it up next.

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::CompatProvider;
use crate::providers::{missing_env_var, Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// DeepSeek.
pub struct DeepSeekProvider {
    compat: CompatProvider,
}

impl DeepSeekProvider {
    /// Build a provider for `model`, reading configuration from the environment. See the module
    /// docs for the `reasoning_content` gap on `deepseek-reasoner`.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let key =
            std::env::var("DEEPSEEK_API_KEY").map_err(|_| missing_env_var("DEEPSEEK_API_KEY"))?;
        let base_url = std::env::var("DEEPSEEK_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com".to_string());
        let config = crate::providers::compat::CompatConfig::new("deepseek", base_url, model.model)
            .with_api_key(key)
            .without_embeddings();
        let compat = CompatProvider::new(config, clock)?;
        Ok(DeepSeekProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "deepseek",
            display_name: "DeepSeek",
            env_vars: &[
                EnvVarRequirement {
                    name: "DEEPSEEK_API_KEY",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "DEEPSEEK_BASE_URL",
                    required: false,
                    description: "Override the default https://api.deepseek.com",
                },
            ],
            capabilities: Capabilities {
                completion: true,
                embedding: false,
                streaming: true,
                tool_use: true,
                vision: false,
            },
        }
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
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

/// Mistral.
pub struct MistralProvider {
    compat: CompatProvider,
}

impl MistralProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let key =
            std::env::var("MISTRAL_API_KEY").map_err(|_| missing_env_var("MISTRAL_API_KEY"))?;
        let base_url = std::env::var("MISTRAL_BASE_URL")
            .unwrap_or_else(|_| "https://api.mistral.ai/v1".to_string());
        let config = crate::providers::compat::CompatConfig::new("mistral", base_url, model.model)
            .with_api_key(key);
        let compat = CompatProvider::new(config, clock)?;
        Ok(MistralProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "mistral",
            display_name: "Mistral",
            env_vars: &[
                EnvVarRequirement {
                    name: "MISTRAL_API_KEY",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "MISTRAL_BASE_URL",
                    required: false,
                    description: "Override the default https://api.mistral.ai/v1",
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
impl Provider for MistralProvider {
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

/// xAI (Grok).
pub struct XaiProvider {
    compat: CompatProvider,
}

impl XaiProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let key = std::env::var("XAI_API_KEY").map_err(|_| missing_env_var("XAI_API_KEY"))?;
        let base_url =
            std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai/v1".to_string());
        let config = crate::providers::compat::CompatConfig::new("xai", base_url, model.model)
            .with_api_key(key)
            .without_embeddings();
        let compat = CompatProvider::new(config, clock)?;
        Ok(XaiProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "xai",
            display_name: "xAI",
            env_vars: &[
                EnvVarRequirement {
                    name: "XAI_API_KEY",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "XAI_BASE_URL",
                    required: false,
                    description: "Override the default https://api.x.ai/v1",
                },
            ],
            capabilities: Capabilities {
                completion: true,
                embedding: false,
                streaming: true,
                tool_use: true,
                vision: false,
            },
        }
    }
}

#[async_trait]
impl Provider for XaiProvider {
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
    use super::*;
    use crate::providers::compat::{
        assemble_streamed_completion, classify_status, parse_sse_body, parse_wire_embed_response,
        parse_wire_response,
    };
    use crate::types::{ContentBlock, StopReason};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    /// Serializes every test that mutates process env vars (`std::env::set_var` is process-wide),
    /// so `from_env` tests in this module can't race each other's env state.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn clock() -> Arc<dyn Clock> {
        Arc::new(tm_types::FixedClock::epoch())
    }

    fn clear_all_frontier_env() {
        for var in [
            "DEEPSEEK_API_KEY",
            "DEEPSEEK_BASE_URL",
            "MISTRAL_API_KEY",
            "MISTRAL_BASE_URL",
            "XAI_API_KEY",
            "XAI_BASE_URL",
        ] {
            std::env::remove_var(var);
        }
    }

    // ---- DeepSeek ----

    #[test]
    fn deepseek_from_env_fails_without_api_key() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_frontier_env();
        let err = DeepSeekProvider::from_env(ModelId::new("deepseek", "deepseek-chat"), clock())
            .err()
            .expect("from_env should fail without DEEPSEEK_API_KEY");
        match err {
            ProviderError::AuthFailed(msg) => assert!(msg.contains("DEEPSEEK_API_KEY")),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn deepseek_from_env_defaults_base_url_and_drops_embeddings() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_frontier_env();
        std::env::set_var("DEEPSEEK_API_KEY", "ds-test-key");
        let provider =
            DeepSeekProvider::from_env(ModelId::new("deepseek", "deepseek-chat"), clock())
                .expect("builds with key set");
        assert_eq!(provider.id(), "deepseek");
        clear_all_frontier_env();
    }

    #[test]
    fn deepseek_from_env_honors_base_url_override() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_frontier_env();
        std::env::set_var("DEEPSEEK_API_KEY", "ds-test-key");
        std::env::set_var("DEEPSEEK_BASE_URL", "https://example.invalid/deepseek");
        // CompatProvider keeps its config private, so the override can't be inspected directly
        // from a sibling module; this asserts the override is at least accepted, not rejected as
        // an invalid header value or similar.
        let provider =
            DeepSeekProvider::from_env(ModelId::new("deepseek", "deepseek-chat"), clock())
                .expect("builds with key set and a base URL override");
        assert_eq!(provider.id(), "deepseek");
        clear_all_frontier_env();
    }

    #[test]
    fn deepseek_info_declares_no_embeddings_and_required_key() {
        let info = DeepSeekProvider::info();
        assert_eq!(info.id, "deepseek");
        assert!(!info.capabilities.embedding);
        assert!(info.capabilities.completion);
        assert!(info.capabilities.streaming);
        let key_var = info
            .env_vars
            .iter()
            .find(|v| v.name == "DEEPSEEK_API_KEY")
            .expect("declares DEEPSEEK_API_KEY");
        assert!(key_var.required);
    }

    /// DeepSeek's `deepseek-reasoner` puts chain-of-thought in a `reasoning_content` field beside
    /// `content`. Documents today's actual (gap-flagged, see module docs) behavior: the shared
    /// compat parser has no slot for it, so it is silently dropped and only the final answer
    /// survives as `Text`.
    #[test]
    fn deepseek_reasoner_response_drops_reasoning_content_today() {
        let body = br#"{
            "model": "deepseek-reasoner",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "The answer is 42.",
                    "reasoning_content": "Let me think step by step... it's 42."
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 6}
        }"#;
        let completion = parse_wire_response(
            body,
            "deepseek-reasoner",
            "deepseek",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .expect("parses despite the unrecognized reasoning_content field");
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "The answer is 42.".to_string()
            }]
        );
    }

    // ---- Mistral ----

    #[test]
    fn mistral_from_env_fails_without_api_key() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_frontier_env();
        let err =
            MistralProvider::from_env(ModelId::new("mistral", "mistral-large-latest"), clock())
                .err()
                .expect("from_env should fail without MISTRAL_API_KEY");
        assert!(matches!(err, ProviderError::AuthFailed(msg) if msg.contains("MISTRAL_API_KEY")));
    }

    #[test]
    fn mistral_from_env_defaults_base_url() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_frontier_env();
        std::env::set_var("MISTRAL_API_KEY", "mistral-test-key");
        let provider =
            MistralProvider::from_env(ModelId::new("mistral", "mistral-large-latest"), clock())
                .expect("builds with key set");
        assert_eq!(provider.id(), "mistral");
        clear_all_frontier_env();
    }

    #[test]
    fn mistral_info_declares_embeddings_supported() {
        let info = MistralProvider::info();
        assert_eq!(info.id, "mistral");
        assert!(info.capabilities.embedding);
    }

    #[test]
    fn mistral_embeddings_response_parses_via_shared_compat_shape() {
        let body = br#"{
            "model": "mistral-embed",
            "data": [
                {"index": 0, "embedding": [0.1, 0.2, 0.3]},
                {"index": 1, "embedding": [0.4, 0.5, 0.6]}
            ],
            "usage": {"prompt_tokens": 5, "completion_tokens": 0}
        }"#;
        let embeddings = parse_wire_embed_response(body, "mistral-embed", "mistral")
            .expect("parses recorded Mistral embeddings response");
        assert_eq!(embeddings.model, ModelId::new("mistral", "mistral-embed"));
        assert_eq!(embeddings.vectors.len(), 2);
        assert_eq!(embeddings.vectors[0], vec![0.1, 0.2, 0.3]);
    }

    // ---- xAI ----

    #[test]
    fn xai_from_env_fails_without_api_key() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_frontier_env();
        let err = XaiProvider::from_env(ModelId::new("xai", "grok-2-latest"), clock())
            .err()
            .expect("from_env should fail without XAI_API_KEY");
        assert!(matches!(err, ProviderError::AuthFailed(msg) if msg.contains("XAI_API_KEY")));
    }

    #[test]
    fn xai_from_env_defaults_base_url_and_drops_embeddings() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_frontier_env();
        std::env::set_var("XAI_API_KEY", "xai-test-key");
        let provider = XaiProvider::from_env(ModelId::new("xai", "grok-2-latest"), clock())
            .expect("builds with key set");
        assert_eq!(provider.id(), "xai");
        assert!(!XaiProvider::info().capabilities.embedding);
        clear_all_frontier_env();
    }

    #[test]
    fn xai_info_lists_required_and_optional_env_vars() {
        let info = XaiProvider::info();
        assert_eq!(info.env_vars.len(), 2);
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "XAI_API_KEY" && v.required));
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "XAI_BASE_URL" && !v.required));
    }

    // ---- shared error mapping + streaming reassembly, against these three vendors' recorded
    // shapes (all plain OpenAI Chat Completions dialect, so these exercise compat.rs's shared
    // functions with fixtures shaped like what DeepSeek/Mistral/xAI actually send) ----

    #[test]
    fn classify_status_maps_401_429_and_5xx_for_this_fleet() {
        assert!(matches!(
            classify_status(
                reqwest::StatusCode::UNAUTHORIZED,
                None,
                br#"{"error": {"message": "Authentication Fails"}}"#,
            )
            .unwrap_err(),
            ProviderError::AuthFailed(_)
        ));

        match classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some("5"),
            br#"{"error": {"message": "rate limit exceeded"}}"#,
        )
        .unwrap_err()
        {
            ProviderError::RateLimited {
                retry_after,
                message,
            } => {
                assert_eq!(retry_after, Some(Duration::from_secs(5)));
                assert_eq!(message, "rate limit exceeded");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }

        assert!(matches!(
            classify_status(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                None,
                br#"{"error": {"message": "server busy, please retry"}}"#,
            )
            .unwrap_err(),
            ProviderError::Unavailable(_)
        ));
    }

    #[test]
    fn parse_wire_response_rejects_malformed_body() {
        let err = parse_wire_response(
            b"{not valid json",
            "grok-2-latest",
            "xai",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn xai_streaming_response_reassembles_into_one_completion() {
        let body = concat!(
            "data: {\"model\": \"grok-2-latest\", \"choices\": [{\"index\": 0, \"delta\": {\"role\": \"assistant\", \"content\": \"Hel\"}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {\"content\": \"lo from Grok!\"}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {}, \"finish_reason\": \"stop\"}]}\n\n",
            "data: {\"choices\": [], \"usage\": {\"prompt_tokens\": 8, \"completion_tokens\": 4}}\n\n",
            "data: [DONE]\n\n",
        );
        let chunks = parse_sse_body(body.as_bytes()).expect("parses SSE body");
        let completion = assemble_streamed_completion(
            &chunks,
            "grok-2-latest",
            "xai",
            Duration::from_millis(42),
            tm_types::Timestamp::EPOCH,
        )
        .expect("reassembles");

        assert_eq!(completion.model, ModelId::new("xai", "grok-2-latest"));
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "Hello from Grok!".to_string()
            }]
        );
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage.input_tokens, 8);
        assert_eq!(completion.usage.output_tokens, 4);
    }
}
