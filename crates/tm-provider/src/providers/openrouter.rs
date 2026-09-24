//! OpenRouter and GitHub Models: two independently-billed gateways that both speak the OpenAI
//! Chat Completions dialect closely enough to build on [`crate::providers::compat`].
//!
//! [`OpenRouterProvider`] env vars:
//! - `OPENROUTER_API_KEY` (required) — Bearer key.
//! - `OPENROUTER_BASE_URL` (optional, default `"https://openrouter.ai/api/v1"`).
//! - `OPENROUTER_HTTP_REFERER` (optional) — sent as `HTTP-Referer`; OpenRouter uses this and
//!   `X-Title` for its public leaderboard attribution, not for auth.
//! - `OPENROUTER_APP_TITLE` (optional) — sent as `X-Title`.
//!
//! OpenRouter model ids are vendor-prefixed (e.g. `"anthropic/claude-sonnet-4.5"`,
//! `"openai/gpt-4o"`) — `model.model` is passed through to the wire verbatim; `providers.toml`
//! candidates routed here must already spell the model that way.
//!
//! ## Cost
//!
//! OpenRouter annotates each generation's usage with an extra `cost` field (in USD) beyond the
//! plain `prompt_tokens`/`completion_tokens` [`crate::providers::compat::WireUsage`] already
//! carries, and per-request provider-routing metadata (which upstream actually served it) on a
//! separate `GET /generation` endpoint keyed by generation id. Neither has anywhere to land today:
//! [`crate::types::Usage`] has no cost field (`input_tokens`, `output_tokens`,
//! `cache_read_tokens`, `cache_write_tokens` only), and this module owns no wire shapes of its own
//! — [`compat::WireUsage`] is shared across eight provider modules, so adding an OpenRouter-only
//! field to it would either silently ignore it for the other seven backends or require every one
//! of them to leave it `None`. Until `Usage` grows a cost field, that cost is simply dropped on
//! the floor the same way `compat::parse_wire_response` drops any wire field it doesn't map;
//! nothing here invents a field to stash it in outside that type's real home in `crate::types`.
//!
//! [`GithubModelsProvider`] env vars:
//! - `GITHUB_TOKEN` (required, falls back to `GITHUB_MODELS_TOKEN`) — a GitHub PAT or the
//!   `GITHUB_TOKEN` GitHub Actions injects automatically, scoped with `models: read` for GitHub
//!   Models access. Sent as `Authorization: Bearer <token>`.
//! - `GITHUB_MODELS_BASE_URL` (optional, default `"https://models.github.ai/inference"`).
//!
//! ## Rate limits (the genuinely free tier)
//!
//! GitHub Models is free to use up to per-account, per-model-size rate limits that scale with the
//! authenticating account's Copilot plan (no Copilot subscription at all still gets a "low" tier,
//! not zero access): requests-per-minute, requests-per-day and tokens-per-request/-per-day caps
//! that are tightest on a free/no-plan account and loosen on Copilot Individual/Business/
//! Enterprise. GitHub publishes the current numeric limits per model and plan tier at
//! <https://docs.github.com/en/github-models/prototyping-with-ai-models#rate-limits> rather than
//! as a stable API response header this module can read, and they are subject to change without
//! this crate's involvement — this doc comment intentionally does not pin specific numbers that
//! would silently go stale. What matters for this provider's behavior: a 429 here maps through
//! [`compat::classify_status`] to [`ProviderError::RateLimited`] exactly like any other backend,
//! honoring `Retry-After` when GitHub sends one, so no special-casing was needed beyond the shared
//! core.

use std::sync::Arc;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::{CompatConfig, CompatProvider};
use crate::providers::{missing_env_var, Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

const OPENROUTER_DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const GITHUB_MODELS_DEFAULT_BASE_URL: &str = "https://models.github.ai/inference";

/// Build the [`CompatConfig`] for [`OpenRouterProvider`] from already-resolved settings, kept
/// separate from `from_env` so the env-var/default wiring is unit-testable without touching real
/// process environment.
fn openrouter_config(
    model: &str,
    api_key: String,
    base_url: Option<String>,
    http_referer: Option<String>,
    app_title: Option<String>,
) -> CompatConfig {
    let mut config = CompatConfig::new(
        "openrouter",
        base_url.unwrap_or_else(|| OPENROUTER_DEFAULT_BASE_URL.to_string()),
        model,
    )
    .with_api_key(api_key)
    // OpenRouter does not serve an OpenAI-compatible embeddings endpoint today.
    .without_embeddings();

    if let Some(referer) = http_referer {
        config = config.with_extra_header("HTTP-Referer", referer);
    }
    if let Some(title) = app_title {
        config = config.with_extra_header("X-Title", title);
    }
    config
}

/// Build the [`CompatConfig`] for [`GithubModelsProvider`] from already-resolved settings; see
/// [`openrouter_config`] for why this is split out from `from_env`.
fn github_models_config(model: &str, token: String, base_url: Option<String>) -> CompatConfig {
    CompatConfig::new(
        "github-models",
        base_url.unwrap_or_else(|| GITHUB_MODELS_DEFAULT_BASE_URL.to_string()),
        model,
    )
    .with_api_key(token)
    // GitHub Models does not serve embeddings.
    .without_embeddings()
}

pub(super) fn github_models_token<'a>(
    primary: Option<&'a str>,
    alias: Option<&'a str>,
) -> Option<&'a str> {
    primary
        .filter(|token| !token.trim().is_empty())
        .or_else(|| alias.filter(|token| !token.trim().is_empty()))
}

/// OpenRouter — a single API in front of dozens of upstream model vendors.
pub struct OpenRouterProvider {
    compat: CompatProvider,
}

impl OpenRouterProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .map_err(|_| missing_env_var("OPENROUTER_API_KEY"))?;
        let base_url = std::env::var("OPENROUTER_BASE_URL").ok();
        let http_referer = std::env::var("OPENROUTER_HTTP_REFERER").ok();
        let app_title = std::env::var("OPENROUTER_APP_TITLE").ok();

        let config = openrouter_config(&model.model, api_key, base_url, http_referer, app_title);
        let compat = CompatProvider::new(config, clock)?;
        Ok(OpenRouterProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "openrouter",
            display_name: "OpenRouter",
            env_vars: &[
                EnvVarRequirement {
                    name: "OPENROUTER_API_KEY",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "OPENROUTER_BASE_URL",
                    required: false,
                    description: "Override the default https://openrouter.ai/api/v1",
                },
                EnvVarRequirement {
                    name: "OPENROUTER_HTTP_REFERER",
                    required: false,
                    description: "Sent as the HTTP-Referer header",
                },
                EnvVarRequirement {
                    name: "OPENROUTER_APP_TITLE",
                    required: false,
                    description: "Sent as the X-Title header",
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
impl Provider for OpenRouterProvider {
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

/// GitHub Models — GitHub's own model-hosting gateway, OpenAI-Chat-Completions-shaped, notable
/// for a genuinely free tier gated only by account rate limits (see module docs).
pub struct GithubModelsProvider {
    compat: CompatProvider,
}

impl GithubModelsProvider {
    /// Build a provider for `model`, reading configuration from the environment. Reads
    /// `GITHUB_TOKEN` first, falling back to `GITHUB_MODELS_TOKEN` so a deployment already using
    /// `GITHUB_TOKEN` for something else (e.g. an unrelated `gh` CLI login) doesn't collide.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let primary = std::env::var("GITHUB_TOKEN").ok();
        let alias = std::env::var("GITHUB_MODELS_TOKEN").ok();
        let token = github_models_token(primary.as_deref(), alias.as_deref())
            .map(str::to_owned)
            .ok_or_else(|| missing_env_var("GITHUB_TOKEN"))?;
        let base_url = std::env::var("GITHUB_MODELS_BASE_URL").ok();

        let config = github_models_config(&model.model, token, base_url);
        let compat = CompatProvider::new(config, clock)?;
        Ok(GithubModelsProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "github-models",
            display_name: "GitHub Models",
            env_vars: &[
                EnvVarRequirement {
                    name: "GITHUB_TOKEN",
                    required: true,
                    description:
                        "GitHub PAT or Actions token scoped for GitHub Models (or GITHUB_MODELS_TOKEN)",
                },
                EnvVarRequirement {
                    name: "GITHUB_MODELS_BASE_URL",
                    required: false,
                    description: "Override the default https://models.github.ai/inference",
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
impl Provider for GithubModelsProvider {
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
    use tm_types::Timestamp;

    // ---- config wiring -------------------------------------------------------------------

    #[test]
    fn openrouter_config_uses_default_base_url_and_disables_embeddings() {
        let config = openrouter_config("openai/gpt-4o", "sk-or-test".to_string(), None, None, None);
        assert_eq!(config.id, "openrouter");
        assert_eq!(config.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(config.model, "openai/gpt-4o");
        assert_eq!(config.api_key.as_deref(), Some("sk-or-test"));
        assert_eq!(config.embeddings_path, None);
        assert!(config.extra_headers.is_empty());
    }

    #[test]
    fn openrouter_config_overrides_base_url_and_sends_attribution_headers() {
        let config = openrouter_config(
            "anthropic/claude-sonnet-4.5",
            "sk-or-test".to_string(),
            Some("https://custom.invalid/v1".to_string()),
            Some("https://example.com".to_string()),
            Some("My App".to_string()),
        );
        assert_eq!(config.base_url, "https://custom.invalid/v1");
        assert!(config.extra_headers.contains(&(
            "HTTP-Referer".to_string(),
            "https://example.com".to_string()
        )));
        assert!(config
            .extra_headers
            .contains(&("X-Title".to_string(), "My App".to_string())));
    }

    #[test]
    fn openrouter_from_env_reports_missing_api_key_without_leaking_material() {
        // Ensure a stale value from another test/process doesn't leak in.
        std::env::remove_var("OPENROUTER_API_KEY");
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(Timestamp::EPOCH));
        match OpenRouterProvider::from_env(ModelId::new("openrouter", "openai/gpt-4o"), clock) {
            Err(ProviderError::AuthFailed(msg)) => {
                assert_eq!(msg, "OPENROUTER_API_KEY is not set")
            }
            Err(other) => panic!("expected AuthFailed, got {other:?}"),
            Ok(_) => panic!("expected missing key to fail"),
        }
    }

    #[test]
    fn github_models_config_uses_default_base_url_and_disables_embeddings() {
        let config = github_models_config("gpt-4o-mini", "ghp_test".to_string(), None);
        assert_eq!(config.id, "github-models");
        assert_eq!(config.base_url, "https://models.github.ai/inference");
        assert_eq!(config.api_key.as_deref(), Some("ghp_test"));
        assert_eq!(config.embeddings_path, None);
    }

    #[test]
    fn github_models_config_overrides_base_url() {
        let config = github_models_config(
            "gpt-4o-mini",
            "ghp_test".to_string(),
            Some("https://custom.invalid".to_string()),
        );
        assert_eq!(config.base_url, "https://custom.invalid");
    }

    #[test]
    fn github_models_from_env_reports_missing_token_without_leaking_material() {
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("GITHUB_MODELS_TOKEN");
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(Timestamp::EPOCH));
        match GithubModelsProvider::from_env(ModelId::new("github-models", "gpt-4o-mini"), clock) {
            Err(ProviderError::AuthFailed(msg)) => assert_eq!(msg, "GITHUB_TOKEN is not set"),
            Err(other) => panic!("expected AuthFailed, got {other:?}"),
            Ok(_) => panic!("expected missing token to fail"),
        }
    }

    #[test]
    fn github_models_token_falls_back_from_empty_primary_and_ignores_blank_values() {
        assert_eq!(
            github_models_token(Some("  "), Some("ghp-alias")),
            Some("ghp-alias")
        );
        assert_eq!(
            github_models_token(Some("ghp-primary"), Some("ghp-alias")),
            Some("ghp-primary")
        );
        assert_eq!(github_models_token(None, Some(" \t")), None);
    }

    #[test]
    fn github_models_info_accepts_the_alias_credential() {
        // The pure selection helper is the same gate used by ProviderInfo::is_configured and
        // from_env, so discovery must not disagree with construction for the alias.
        assert!(github_models_token(None, Some("ghp-alias")).is_some());
        assert!(github_models_token(None, None).is_none());
    }

    #[test]
    fn info_declares_required_and_optional_env_vars() {
        let or_info = OpenRouterProvider::info();
        assert_eq!(or_info.id, "openrouter");
        assert!(or_info
            .env_vars
            .iter()
            .any(|v| v.name == "OPENROUTER_API_KEY" && v.required));
        assert!(!or_info.capabilities.embedding);

        let gh_info = GithubModelsProvider::info();
        assert_eq!(gh_info.id, "github-models");
        assert!(gh_info
            .env_vars
            .iter()
            .any(|v| v.name == "GITHUB_TOKEN" && v.required));
        assert!(!gh_info.capabilities.embedding);
    }

    #[test]
    fn build_headers_carries_bearer_auth_and_attribution() {
        let config = openrouter_config(
            "openai/gpt-4o",
            "sk-or-test".to_string(),
            None,
            Some("https://example.com".to_string()),
            Some("My App".to_string()),
        );
        let headers = build_headers(&config).expect("builds headers");
        assert_eq!(
            headers.get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer sk-or-test"
        );
        assert_eq!(headers.get("HTTP-Referer").unwrap(), "https://example.com");
        assert_eq!(headers.get("X-Title").unwrap(), "My App");
    }

    // ---- wire shapes / error mapping, exercised via the shared compat core with recorded ----
    // ---- JSON bodies shaped like real OpenRouter / GitHub Models responses. -----------------

    #[test]
    fn parses_openrouter_style_response_ignoring_extra_cost_field() {
        // OpenRouter appends a "cost" field (and other routing metadata) to usage/choices beyond
        // the plain OpenAI shape; parse_wire_response must ignore unknown fields rather than fail.
        let body = br#"{
            "id": "gen-123",
            "provider": "Anthropic",
            "model": "anthropic/claude-sonnet-4.5",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.00042}
        }"#;
        let completion = parse_wire_response(
            body,
            "anthropic/claude-sonnet-4.5",
            "openrouter",
            std::time::Duration::from_millis(1),
            Timestamp::EPOCH,
        )
        .expect("parses");
        assert_eq!(completion.model.provider, "openrouter");
        assert_eq!(completion.usage.input_tokens, 10);
        assert_eq!(completion.usage.output_tokens, 5);
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "hello".to_string()
            }]
        );
    }

    #[test]
    fn parses_github_models_style_response() {
        let body = br#"{
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi there"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}
        }"#;
        let completion = parse_wire_response(
            body,
            "gpt-4o-mini",
            "github-models",
            std::time::Duration::from_millis(1),
            Timestamp::EPOCH,
        )
        .expect("parses");
        assert_eq!(completion.model.model, "gpt-4o-mini");
        assert_eq!(completion.usage.input_tokens, 3);
    }

    #[test]
    fn maps_401_to_auth_failed() {
        let body = br#"{"error": {"message": "Invalid API key", "type": "invalid_request_error"}}"#;
        let err = classify_status(reqwest::StatusCode::UNAUTHORIZED, None, body).unwrap_err();
        match err {
            ProviderError::AuthFailed(msg) => assert!(msg.contains("Invalid API key")),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn maps_429_to_rate_limited_honoring_retry_after() {
        let body = br#"{"error": {"message": "rate limit exceeded"}}"#;
        let err =
            classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, Some("2"), body).unwrap_err();
        match err {
            ProviderError::RateLimited {
                message,
                retry_after,
            } => {
                assert!(message.contains("rate limit exceeded"));
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(2)));
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
            "openai/gpt-4o",
            "openrouter",
            std::time::Duration::from_millis(1),
            Timestamp::EPOCH,
        )
        .expect_err("must not parse");
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn streaming_reassembles_text_and_final_usage_chunk() {
        let sse = concat!(
            "data: {\"model\":\"openai/gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let chunks = parse_sse_body(sse.as_bytes()).expect("parses SSE");
        let completion = assemble_streamed_completion(
            &chunks,
            "openai/gpt-4o",
            "openrouter",
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
