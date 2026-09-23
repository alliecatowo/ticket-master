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
//!
//! # Why the OpenAI-compatible surface, not Ollama's native `/api/chat`
//!
//! Ollama also serves a native, non-OpenAI-shaped API (`/api/chat`, `/api/embeddings`) directly
//! under `OLLAMA_HOST` (no `/v1`), with its own request/response envelope. This module deliberately
//! targets Ollama's OpenAI-compatible `/v1` mount instead of that native API:
//! [`crate::providers::compat`] already owns one well-tested translation between
//! [`crate::types::CompletionRequest`]/[`crate::types::Completion`] and the OpenAI Chat
//! Completions wire shape, including retry/backoff, SSE reassembly and 401/429/5xx error mapping.
//! Speaking Ollama's native dialect instead would mean a second, parallel translation layer in
//! this file, duplicating everything `compat.rs` already gets right, for a local-only backend
//! where the OpenAI-compatible surface already covers chat, streaming, tool calls and embeddings.
//! The cost is real but narrow: the native API exposes a few Ollama-specific extras (e.g. `/api/pull`
//! to fetch a model, richer per-token timing fields) that the `/v1` surface does not — out of scope
//! for a [`crate::fabric::Provider`], which only needs `complete`/`embed`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::{AuthStyle, CompatConfig, CompatProvider};
use crate::providers::{Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// Timeout for [`probe_v1_models`]/[`list_v1_models`] — short, because these exist to answer
/// "is anything listening on localhost right now?" quickly, not to serve a real request.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// One entry of an OpenAI-style `GET /v1/models` list response.
#[derive(Debug, Deserialize)]
struct WireModelEntry {
    id: String,
}

/// An OpenAI-style `GET /v1/models` list response: `{"object": "list", "data": [...]}`.
#[derive(Debug, Deserialize)]
struct WireModelsList {
    data: Vec<WireModelEntry>,
}

/// Parse a `GET /v1/models` response body into the model ids it lists. Pure and unit-tested
/// against recorded JSON, kept separate from the network call in [`list_v1_models`] so the
/// parsing logic needs no HTTP client to test.
fn parse_models_list(body: &[u8]) -> Result<Vec<String>, ProviderError> {
    let parsed: WireModelsList = serde_json::from_slice(body)
        .map_err(|e| ProviderError::MalformedResponse(format!("invalid /v1/models body: {e}")))?;
    Ok(parsed.data.into_iter().map(|m| m.id).collect())
}

/// `GET {base_url}/models`, with a short timeout and an optional bearer key, returning the listed
/// model ids. `base_url` already includes the trailing `/v1`.
async fn list_v1_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, ProviderError> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| ProviderError::Unavailable(format!("failed to build HTTP client: {e}")))?;
    let mut request = client.get(format!("{base_url}/models"));
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(|e| {
        if e.is_timeout() {
            ProviderError::Timeout(e.to_string())
        } else {
            ProviderError::Unavailable(e.to_string())
        }
    })?;
    if !response.status().is_success() {
        return Err(ProviderError::Unavailable(format!(
            "{base_url}/models responded {}",
            response.status()
        )));
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| ProviderError::Unavailable(format!("failed to read response body: {e}")))?;
    parse_models_list(&body)
}

/// Short-timeout reachability check: `true` only if `GET {base_url}/models` succeeds. This is the
/// "is a local server actually listening" probe the module docs above call for — every env var
/// this module reads is optional, so [`ProviderInfo::is_configured`] alone can never rule out an
/// absent local server, and callers such as [`crate::providers::registry::Registry`] that need to
/// know whether one of these three is *actually* usable right now should await this before
/// offering the corresponding `from_env()` result as a live candidate.
async fn probe_v1_models(base_url: &str, api_key: Option<&str>) -> bool {
    list_v1_models(base_url, api_key).await.is_ok()
}

/// Normalize a user-supplied host into `{host}/v1` with no double slash, defaulting when unset.
fn v1_base_url(env_var: &str, default_host: &str) -> String {
    let host = std::env::var(env_var).unwrap_or_else(|_| default_host.to_string());
    format!("{}/v1", host.trim_end_matches('/'))
}

/// Ollama.
pub struct OllamaProvider {
    compat: CompatProvider,
    base_url: String,
}

impl OllamaProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let base_url = v1_base_url("OLLAMA_HOST", "http://localhost:11434");
        let config = CompatConfig::new("ollama", base_url.clone(), model.model)
            .with_auth_style(AuthStyle::None);
        let compat = CompatProvider::new(config, clock)?;
        Ok(OllamaProvider { compat, base_url })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract
    /// (and this module's docs for why `is_configured` alone is not enough here).
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "ollama",
            display_name: "Ollama",
            env_vars: &[EnvVarRequirement {
                name: "OLLAMA_HOST",
                required: false,
                description: "Override the default http://localhost:11434",
            }],
            capabilities: Capabilities {
                completion: true,
                embedding: true,
                streaming: true,
                tool_use: true,
                vision: false,
            },
        }
    }

    /// Short-timeout reachability check; see [`probe_v1_models`].
    pub async fn probe(&self) -> bool {
        probe_v1_models(&self.base_url, None).await
    }

    /// List model ids Ollama currently has pulled, via `GET /v1/models`.
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        list_v1_models(&self.base_url, None).await
    }
}

#[async_trait]
impl Provider for OllamaProvider {
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

/// LM Studio.
pub struct LmStudioProvider {
    compat: CompatProvider,
    base_url: String,
}

impl LmStudioProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let base_url = v1_base_url("LM_STUDIO_HOST", "http://localhost:1234");
        let config = CompatConfig::new("lm-studio", base_url.clone(), model.model)
            .with_auth_style(AuthStyle::None);
        let compat = CompatProvider::new(config, clock)?;
        Ok(LmStudioProvider { compat, base_url })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract
    /// (and this module's docs for why `is_configured` alone is not enough here).
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "lm-studio",
            display_name: "LM Studio",
            env_vars: &[EnvVarRequirement {
                name: "LM_STUDIO_HOST",
                required: false,
                description: "Override the default http://localhost:1234",
            }],
            capabilities: Capabilities {
                completion: true,
                embedding: true,
                streaming: true,
                tool_use: true,
                vision: false,
            },
        }
    }

    /// Short-timeout reachability check; see [`probe_v1_models`].
    pub async fn probe(&self) -> bool {
        probe_v1_models(&self.base_url, None).await
    }

    /// List model ids currently loaded/served by LM Studio, via `GET /v1/models`.
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        list_v1_models(&self.base_url, None).await
    }
}

#[async_trait]
impl Provider for LmStudioProvider {
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

/// llama.cpp's `llama-server`.
pub struct LlamaCppProvider {
    compat: CompatProvider,
    base_url: String,
    api_key: Option<String>,
}

impl LlamaCppProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let base_url = v1_base_url("LLAMA_CPP_HOST", "http://localhost:8080");
        let api_key = std::env::var("LLAMA_CPP_API_KEY").ok();

        let mut config = CompatConfig::new("llama-cpp", base_url.clone(), model.model);
        config = match &api_key {
            Some(key) => config.with_api_key(key.clone()),
            None => config.with_auth_style(AuthStyle::None),
        };
        let compat = CompatProvider::new(config, clock)?;
        Ok(LlamaCppProvider {
            compat,
            base_url,
            api_key,
        })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract
    /// (and this module's docs for why `is_configured` alone is not enough here).
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "llama-cpp",
            display_name: "llama.cpp",
            env_vars: &[
                EnvVarRequirement {
                    name: "LLAMA_CPP_HOST",
                    required: false,
                    description: "Override the default http://localhost:8080",
                },
                EnvVarRequirement {
                    name: "LLAMA_CPP_API_KEY",
                    required: false,
                    description: "Only needed if llama-server was started with --api-key",
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

    /// Short-timeout reachability check; see [`probe_v1_models`].
    pub async fn probe(&self) -> bool {
        probe_v1_models(&self.base_url, self.api_key.as_deref()).await
    }

    /// List model ids `llama-server` reports, via `GET /v1/models`.
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        list_v1_models(&self.base_url, self.api_key.as_deref()).await
    }
}

#[async_trait]
impl Provider for LlamaCppProvider {
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
        assemble_streamed_completion, classify_status, parse_sse_body, parse_wire_response,
    };
    use crate::types::{ContentBlock, StopReason};
    use std::sync::{Mutex, OnceLock};

    /// Serializes every test that mutates process env vars (`std::env::set_var` is process-wide),
    /// so `from_env` tests in this module can't race each other's env state.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn clock() -> Arc<dyn Clock> {
        Arc::new(tm_types::FixedClock::epoch())
    }

    fn clear_all_local_env() {
        for var in [
            "OLLAMA_HOST",
            "LM_STUDIO_HOST",
            "LLAMA_CPP_HOST",
            "LLAMA_CPP_API_KEY",
        ] {
            std::env::remove_var(var);
        }
    }

    // ---- construction never fails just because nothing is listening ----

    #[test]
    fn ollama_from_env_builds_with_no_env_at_all() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_local_env();
        let provider = OllamaProvider::from_env(ModelId::new("ollama", "llama3"), clock())
            .expect("builds with no env vars set; local providers must not require a key");
        assert_eq!(provider.id(), "ollama");
        clear_all_local_env();
    }

    #[test]
    fn ollama_from_env_honors_host_override() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_local_env();
        std::env::set_var("OLLAMA_HOST", "http://example.invalid:11434/");
        let provider = OllamaProvider::from_env(ModelId::new("ollama", "llama3"), clock())
            .expect("builds with an overridden host");
        assert_eq!(provider.base_url, "http://example.invalid:11434/v1");
        clear_all_local_env();
    }

    #[test]
    fn lm_studio_from_env_builds_with_no_env_at_all() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_local_env();
        let provider =
            LmStudioProvider::from_env(ModelId::new("lm-studio", "local-model"), clock())
                .expect("builds with no env vars set");
        assert_eq!(provider.id(), "lm-studio");
        assert_eq!(provider.base_url, "http://localhost:1234/v1");
        clear_all_local_env();
    }

    #[test]
    fn llama_cpp_from_env_builds_with_no_env_at_all() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_local_env();
        let provider =
            LlamaCppProvider::from_env(ModelId::new("llama-cpp", "local-model"), clock())
                .expect("builds with no env vars set");
        assert_eq!(provider.id(), "llama-cpp");
        assert_eq!(provider.base_url, "http://localhost:8080/v1");
        assert!(provider.api_key.is_none());
        clear_all_local_env();
    }

    #[test]
    fn llama_cpp_from_env_picks_up_optional_api_key() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_all_local_env();
        std::env::set_var("LLAMA_CPP_API_KEY", "sk-local-test");
        let provider =
            LlamaCppProvider::from_env(ModelId::new("llama-cpp", "local-model"), clock())
                .expect("builds with an api key set");
        assert_eq!(provider.api_key.as_deref(), Some("sk-local-test"));
        clear_all_local_env();
    }

    // ---- info() metadata: every env var here is optional, per the module docs ----

    #[test]
    fn all_three_info_declare_every_env_var_optional() {
        for info in [
            OllamaProvider::info(),
            LmStudioProvider::info(),
            LlamaCppProvider::info(),
        ] {
            assert!(
                info.env_vars.iter().all(|v| !v.required),
                "{} must declare no required env vars: absence of one never proves the local \
                 server is unreachable, it just means the default host applies",
                info.id
            );
            assert!(info.capabilities.completion);
            assert!(info.capabilities.embedding);
        }
        assert_eq!(OllamaProvider::info().id, "ollama");
        assert_eq!(LmStudioProvider::info().id, "lm-studio");
        assert_eq!(LlamaCppProvider::info().id, "llama-cpp");
    }

    // ---- model listing: pure parsing, no network ----

    #[test]
    fn parse_models_list_reads_openai_shaped_body() {
        let body = br#"{
            "object": "list",
            "data": [
                {"id": "llama3:latest", "object": "model"},
                {"id": "nomic-embed-text", "object": "model"}
            ]
        }"#;
        let models = parse_models_list(body).expect("parses a well-formed /v1/models body");
        assert_eq!(models, vec!["llama3:latest", "nomic-embed-text"]);
    }

    #[test]
    fn parse_models_list_rejects_malformed_body() {
        let err = parse_models_list(b"{not valid json").unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[tokio::test]
    async fn list_v1_models_distinguishes_live_empty_server_from_populated_server() {
        async fn serve_once(body: &'static str) -> String {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind loopback mock server");
            let address = listener.local_addr().expect("mock address");
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept probe");
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            });
            format!("http://{address}/v1")
        }

        let empty = serve_once(r#"{"data":[]}"#).await;
        assert!(
            list_v1_models(&empty, None)
                .await
                .expect("empty list")
                .is_empty()
        );

        let populated = serve_once(r#"{"data":[{"id":"actual-pulled-model"}]}"#).await;
        assert_eq!(
            list_v1_models(&populated, None)
                .await
                .expect("populated list"),
            ["actual-pulled-model"]
        );
    }

    // ---- shared error mapping + streaming reassembly, against this fleet's recorded shapes
    // (all plain OpenAI Chat Completions dialect, so these exercise compat.rs's shared functions
    // with fixtures shaped like what these three local backends actually send) ----

    #[test]
    fn classify_status_maps_401_429_and_5xx_for_this_fleet() {
        assert!(matches!(
            classify_status(
                reqwest::StatusCode::UNAUTHORIZED,
                None,
                br#"{"error": {"message": "invalid api key"}}"#,
            )
            .unwrap_err(),
            ProviderError::AuthFailed(_)
        ));

        match classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some("2"),
            br#"{"error": {"message": "model is busy"}}"#,
        )
        .unwrap_err()
        {
            ProviderError::RateLimited {
                retry_after,
                message,
            } => {
                assert_eq!(retry_after, Some(Duration::from_secs(2)));
                assert_eq!(message, "model is busy");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }

        assert!(matches!(
            classify_status(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                None,
                br#"{"error": {"message": "failed to load model"}}"#,
            )
            .unwrap_err(),
            ProviderError::Unavailable(_)
        ));
    }

    #[test]
    fn parse_wire_response_rejects_malformed_body() {
        let err = parse_wire_response(
            b"{not valid json",
            "llama3",
            "ollama",
            Duration::ZERO,
            tm_types::Timestamp::EPOCH,
        )
        .unwrap_err();
        assert!(matches!(err, ProviderError::MalformedResponse(_)));
    }

    #[test]
    fn ollama_streaming_response_reassembles_into_one_completion() {
        let body = concat!(
            "data: {\"model\": \"llama3\", \"choices\": [{\"index\": 0, \"delta\": {\"role\": \"assistant\", \"content\": \"Hel\"}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {\"content\": \"lo from Ollama!\"}}]}\n\n",
            "data: {\"choices\": [{\"index\": 0, \"delta\": {}, \"finish_reason\": \"stop\"}]}\n\n",
            "data: {\"choices\": [], \"usage\": {\"prompt_tokens\": 6, \"completion_tokens\": 4}}\n\n",
            "data: [DONE]\n\n",
        );
        let chunks = parse_sse_body(body.as_bytes()).expect("parses SSE body");
        let completion = assemble_streamed_completion(
            &chunks,
            "llama3",
            "ollama",
            Duration::from_millis(10),
            tm_types::Timestamp::EPOCH,
        )
        .expect("reassembles");

        assert_eq!(completion.model, ModelId::new("ollama", "llama3"));
        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(
            completion.candidates[0].content,
            vec![ContentBlock::Text {
                text: "Hello from Ollama!".to_string()
            }]
        );
        assert_eq!(completion.candidates[0].stop_reason, StopReason::EndTurn);
        assert_eq!(completion.usage.input_tokens, 6);
        assert_eq!(completion.usage.output_tokens, 4);
    }
}
