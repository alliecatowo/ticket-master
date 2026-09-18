//! Cloudflare Workers AI, over its OpenAI-compatible endpoint, built on
//! [`crate::providers::compat`].
//!
//! Env vars read by [`CloudflareWorkersAiProvider::from_env`]:
//! - `CF_API_TOKEN` (required) — Bearer key (a Cloudflare API token scoped for Workers AI, not
//!   the account's Global API Key).
//! - `CF_ACCOUNT_ID` (required) — the account id is part of the URL path, not a header.
//!
//! ## Free daily allocation
//!
//! Workers AI ships a free daily allocation of "neurons" (Cloudflare's model-agnostic compute
//! unit) on every account, no billing details required, which is enough to run a meaningful
//! number of requests against the smaller hosted models before Cloudflare either throttles or
//! requires enabling billing to keep going past the ceiling. This crate does not special-case
//! that allocation: it is capacity with a daily reset, exactly like the other providers in this
//! fabric that fold a free tier into their normal request path rather than a separately-priced
//! product. The exact neuron ceiling and per-model neuron cost are account/plan-dependent and
//! published at <https://developers.cloudflare.com/workers-ai/platform/pricing/> rather than
//! discoverable from a response header this module can read, so no number is pinned here — once
//! the daily budget is exhausted Cloudflare answers with a 429, which flows through
//! [`compat::classify_status`] to [`crate::types::ProviderError::RateLimited`] exactly like any
//! other backend's rate limit, honoring `Retry-After` when Cloudflare sends one.
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
//!
//! # Uncertainty flagged for the caller
//!
//! The `/ai/v1` OpenAI-compatible surface, its coexistence with the older `/ai/run/{model}`
//! native shape, and the free daily neuron allocation are all documented Cloudflare behavior as
//! of this writing, but no live call was made against the endpoint while writing this module —
//! there is no network access in this sandbox. Treat the base URL and path layout as correct per
//! Cloudflare's public docs, not as independently verified against a live response.

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::{CompatConfig, CompatProvider};
use crate::providers::{missing_env_var, Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// Build the [`CompatConfig`] for [`CloudflareWorkersAiProvider`] from already-resolved
/// settings, kept separate from `from_env` so the env-var/URL wiring is unit-testable without
/// touching real process environment.
fn cloudflare_config(model: &str, api_token: String, account_id: &str) -> CompatConfig {
    let base_url = format!("https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1");
    CompatConfig::new("cloudflare", base_url, model).with_api_key(api_token)
    // Default embeddings_path ("/embeddings") is correct: Workers AI serves its @cf/baai/bge-*
    // embedding models through this same OpenAI-compatible surface.
}

/// Cloudflare Workers AI. See the module doc comment's `# IMPL` section for the exact steps.
pub struct CloudflareWorkersAiProvider {
    compat: CompatProvider,
}

impl CloudflareWorkersAiProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let api_token =
            std::env::var("CF_API_TOKEN").map_err(|_| missing_env_var("CF_API_TOKEN"))?;
        let account_id =
            std::env::var("CF_ACCOUNT_ID").map_err(|_| missing_env_var("CF_ACCOUNT_ID"))?;

        let config = cloudflare_config(&model.model, api_token, &account_id);
        let compat = CompatProvider::new(config, clock)?;
        Ok(CloudflareWorkersAiProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "cloudflare",
            display_name: "Cloudflare Workers AI",
            env_vars: &[
                EnvVarRequirement {
                    name: "CF_API_TOKEN",
                    required: true,
                    description: "Cloudflare API token scoped for Workers AI",
                },
                EnvVarRequirement {
                    name: "CF_ACCOUNT_ID",
                    required: true,
                    description: "Cloudflare account id (part of the URL path)",
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
impl Provider for CloudflareWorkersAiProvider {
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
        assemble_streamed_completion, build_headers, classify_status, parse_sse_body,
        parse_wire_response,
    };
    use crate::types::{ContentBlock, StopReason};
    use std::sync::{Mutex, OnceLock};
    use tm_types::Timestamp;

    /// Serializes every test that mutates process env vars (`std::env::set_var` is process-wide),
    /// since `cargo test` otherwise runs these concurrently on separate threads.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    // ---- config wiring -------------------------------------------------------------------

    #[test]
    fn cloudflare_config_builds_the_ai_v1_url_with_account_id() {
        let config = cloudflare_config(
            "@cf/meta/llama-3.1-8b-instruct",
            "cf-token".to_string(),
            "acct123",
        );
        assert_eq!(config.id, "cloudflare");
        assert_eq!(
            config.base_url,
            "https://api.cloudflare.com/client/v4/accounts/acct123/ai/v1"
        );
        assert_eq!(config.model, "@cf/meta/llama-3.1-8b-instruct");
        assert_eq!(config.api_key.as_deref(), Some("cf-token"));
        // Embeddings stay enabled at the default path: Workers AI serves bge-* models here too.
        assert_eq!(config.embeddings_path.as_deref(), Some("/embeddings"));
    }

    #[test]
    fn from_env_reports_missing_api_token_without_leaking_material() {
        let _guard = env_lock().lock().expect("lock poisoned");
        std::env::remove_var("CF_API_TOKEN");
        std::env::remove_var("CF_ACCOUNT_ID");
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(Timestamp::EPOCH));
        match CloudflareWorkersAiProvider::from_env(
            ModelId::new("cloudflare", "@cf/meta/llama-3.1-8b-instruct"),
            clock,
        ) {
            Err(ProviderError::AuthFailed(msg)) => assert_eq!(msg, "CF_API_TOKEN is not set"),
            Err(other) => panic!("expected AuthFailed, got {other:?}"),
            Ok(_) => panic!("expected missing token to fail"),
        }
    }

    #[test]
    fn from_env_reports_missing_account_id_without_leaking_material() {
        let _guard = env_lock().lock().expect("lock poisoned");
        std::env::set_var("CF_API_TOKEN", "cf-token-value");
        std::env::remove_var("CF_ACCOUNT_ID");
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(Timestamp::EPOCH));
        let result = CloudflareWorkersAiProvider::from_env(
            ModelId::new("cloudflare", "@cf/meta/llama-3.1-8b-instruct"),
            clock,
        );
        std::env::remove_var("CF_API_TOKEN");
        match result {
            Err(ProviderError::AuthFailed(msg)) => assert_eq!(msg, "CF_ACCOUNT_ID is not set"),
            Err(other) => panic!("expected AuthFailed, got {other:?}"),
            Ok(_) => panic!("expected missing account id to fail"),
        }
    }

    #[test]
    fn info_declares_required_env_vars_and_capabilities() {
        let info = CloudflareWorkersAiProvider::info();
        assert_eq!(info.id, "cloudflare");
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "CF_API_TOKEN" && v.required));
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "CF_ACCOUNT_ID" && v.required));
        assert!(info.capabilities.embedding);
        assert!(info.capabilities.streaming);
    }

    #[test]
    fn build_headers_carries_bearer_auth() {
        let config = cloudflare_config(
            "@cf/meta/llama-3.1-8b-instruct",
            "cf-token".to_string(),
            "acct123",
        );
        let headers = build_headers(&config).expect("builds headers");
        assert_eq!(
            headers.get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer cf-token"
        );
    }

    // ---- wire shapes / error mapping, exercised via the shared compat core with recorded ----
    // ---- JSON bodies shaped like real Cloudflare Workers AI /ai/v1 responses. ---------------

    #[test]
    fn parses_cloudflare_style_chat_completion_response() {
        let body = br#"{
            "id": "chatcmpl-abc123",
            "model": "@cf/meta/llama-3.1-8b-instruct",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello from workers ai"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 6}
        }"#;
        let completion = parse_wire_response(
            body,
            "@cf/meta/llama-3.1-8b-instruct",
            "cloudflare",
            std::time::Duration::from_millis(1),
            Timestamp::EPOCH,
        )
        .expect("parses");
        assert_eq!(completion.model.provider, "cloudflare");
        assert_eq!(completion.usage.input_tokens, 12);
        assert_eq!(completion.usage.output_tokens, 6);
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "hello from workers ai".to_string()
            }]
        );
    }

    #[test]
    fn maps_401_to_auth_failed() {
        let body =
            br#"{"error": {"message": "Invalid API token", "type": "authentication_error"}}"#;
        let err = classify_status(reqwest::StatusCode::UNAUTHORIZED, None, body).unwrap_err();
        match err {
            ProviderError::AuthFailed(msg) => assert!(msg.contains("Invalid API token")),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn maps_429_to_rate_limited_honoring_retry_after() {
        // Free daily neuron allocation exhausted, or a plain rate limit: both surface as 429.
        let body = br#"{"error": {"message": "daily free allocation exhausted"}}"#;
        let err =
            classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, Some("30"), body).unwrap_err();
        match err {
            ProviderError::RateLimited {
                message,
                retry_after,
            } => {
                assert!(message.contains("daily free allocation exhausted"));
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(30)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn maps_5xx_to_unavailable() {
        let body = br#"{"error": {"message": "upstream overloaded"}}"#;
        let err =
            classify_status(reqwest::StatusCode::SERVICE_UNAVAILABLE, None, body).unwrap_err();
        assert!(
            matches!(err, ProviderError::Unavailable(msg) if msg.contains("upstream overloaded"))
        );
    }

    #[test]
    fn malformed_body_is_malformed_response_not_a_panic() {
        let body = b"not json at all";
        let err = parse_wire_response(
            body,
            "@cf/meta/llama-3.1-8b-instruct",
            "cloudflare",
            std::time::Duration::from_millis(1),
            Timestamp::EPOCH,
        )
        .expect_err("must not parse");
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn streaming_reassembles_text_and_final_usage_chunk() {
        let sse = concat!(
            "data: {\"model\":\"@cf/meta/llama-3.1-8b-instruct\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let chunks = parse_sse_body(sse.as_bytes()).expect("parses SSE");
        let completion = assemble_streamed_completion(
            &chunks,
            "@cf/meta/llama-3.1-8b-instruct",
            "cloudflare",
            std::time::Duration::from_millis(1),
            Timestamp::EPOCH,
        )
        .expect("assembles");
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "Hello".to_string()
            }]
        );
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage.input_tokens, 4);
        assert_eq!(completion.usage.output_tokens, 2);
    }
}
