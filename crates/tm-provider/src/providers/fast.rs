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
//!
//! ## Free-tier capacity, and why this file parses rate-limit headers
//!
//! Both Groq and Cerebras sell latency, and both gate their free tiers with per-model
//! requests-per-minute, tokens-per-minute and (Groq) requests-per-day ceilings that are tight
//! enough to matter to a caller cycling through `providers.toml` candidates — a fabric that only
//! reacts to a bare `429` and backs off blindly will burn a large fraction of a free-tier budget
//! on retries that were never going to succeed before the window resets. Both backends report
//! their live budget on every response as `x-ratelimit-*` headers (OpenAI's own dialect does not
//! standardize these; this is a de-facto convention Groq and Cerebras both happen to follow):
//!
//! - `x-ratelimit-limit-requests`, `x-ratelimit-limit-tokens` — the ceiling for the current window.
//! - `x-ratelimit-remaining-requests`, `x-ratelimit-remaining-tokens` — budget left in the window.
//! - `x-ratelimit-reset-requests`, `x-ratelimit-reset-tokens` — time until the window rolls over,
//!   formatted as e.g. `"7.66s"` or `"2m59.56s"` rather than a Unix timestamp.
//!
//! [`parse_rate_limit_headers`] and [`suggested_backoff`] below turn those headers into a wait
//! duration a retry loop can use instead of blind exponential backoff. They are pure, unit-tested
//! functions, deliberately decoupled from any live HTTP call: [`crate::providers::compat`]'s
//! retry loop (`CompatProvider::send_with_retry`) is private to that module and does not currently
//! surface response headers to callers, and this file owns only `fast.rs` — extending that shared
//! retry loop to call [`suggested_backoff`] on every attempt is follow-up work for whoever owns
//! `compat.rs`, not invented here. Today [`GroqProvider`] and [`CerebrasProvider`] get the
//! `Retry-After`-aware backoff `compat.rs` already implements for every backend; these header
//! parsers are the ready-to-wire building block for the tighter, budget-aware version once that
//! hook exists.
//!
//! The exact numeric ceilings (RPM/RPD/TPM per model) are not hardcoded here: both vendors vary
//! them per model and revise them without notice, so baking specific numbers into this module
//! would go stale silently. The fabric should treat [`RateLimitSnapshot`], read live off each
//! response, as the source of truth for remaining capacity rather than a static table.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tm_types::Clock;

use crate::fabric::Provider;
use crate::providers::compat::{CompatConfig, CompatProvider};
use crate::providers::{missing_env_var, Capabilities, EnvVarRequirement, ProviderInfo};
use crate::types::{
    Completion, CompletionRequest, EmbedRequest, Embeddings, ModelId, ProviderError,
};

/// Default base URL for [`GroqProvider`], absent `GROQ_BASE_URL`.
const GROQ_DEFAULT_BASE_URL: &str = "https://api.groq.com/openai/v1";

/// Default base URL for [`CerebrasProvider`], absent `CEREBRAS_BASE_URL`.
const CEREBRAS_DEFAULT_BASE_URL: &str = "https://api.cerebras.ai/v1";

/// Build a `without_embeddings` Bearer-auth [`CompatConfig`] for one of this module's backends.
///
/// Pure and env-free by construction — `from_env` reads the environment and hands the results
/// here, so this function (and therefore the wiring it does) is unit-testable without mutating
/// process-wide env vars.
fn build_config(
    id: &'static str,
    base_url: Option<String>,
    default_base_url: &'static str,
    model: String,
    api_key: Option<String>,
    api_key_var: &'static str,
) -> Result<CompatConfig, ProviderError> {
    let api_key = api_key.ok_or_else(|| missing_env_var(api_key_var))?;
    let base_url = base_url.unwrap_or_else(|| default_base_url.to_string());
    Ok(CompatConfig::new(id, base_url, model)
        .with_api_key(api_key)
        .without_embeddings())
}

/// One response's worth of parsed `x-ratelimit-*` headers. Any field is `None` when the
/// corresponding header was absent or unparseable — a missing header is not itself an error,
/// since not every response (e.g. an error response) necessarily carries the full set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RateLimitSnapshot {
    /// `x-ratelimit-limit-requests`: the ceiling for the current request window.
    pub limit_requests: Option<u32>,
    /// `x-ratelimit-limit-tokens`: the ceiling for the current token window.
    pub limit_tokens: Option<u32>,
    /// `x-ratelimit-remaining-requests`: requests left before the window resets.
    pub remaining_requests: Option<u32>,
    /// `x-ratelimit-remaining-tokens`: tokens left before the window resets.
    pub remaining_tokens: Option<u32>,
    /// `x-ratelimit-reset-requests`: time until the request window resets.
    pub reset_requests: Option<Duration>,
    /// `x-ratelimit-reset-tokens`: time until the token window resets.
    pub reset_tokens: Option<Duration>,
}

/// Parse Groq/Cerebras-style `x-ratelimit-*` headers out of an arbitrary case-insensitive
/// `(name, value)` sequence (deliberately not tied to `reqwest::header::HeaderMap`, so tests
/// below need no HTTP types at all).
pub fn parse_rate_limit_headers<'a, I>(headers: I) -> RateLimitSnapshot
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut snapshot = RateLimitSnapshot::default();
    for (name, value) in headers {
        match name.to_ascii_lowercase().as_str() {
            "x-ratelimit-limit-requests" => snapshot.limit_requests = value.trim().parse().ok(),
            "x-ratelimit-limit-tokens" => snapshot.limit_tokens = value.trim().parse().ok(),
            "x-ratelimit-remaining-requests" => {
                snapshot.remaining_requests = value.trim().parse().ok()
            }
            "x-ratelimit-remaining-tokens" => snapshot.remaining_tokens = value.trim().parse().ok(),
            "x-ratelimit-reset-requests" => snapshot.reset_requests = parse_reset_duration(value),
            "x-ratelimit-reset-tokens" => snapshot.reset_tokens = parse_reset_duration(value),
            _ => {}
        }
    }
    snapshot
}

/// Parse a reset duration in the `"7.66s"` / `"2m59.56s"` shape Groq and Cerebras both report,
/// rather than a Unix timestamp. Returns `None` on anything that doesn't fit that shape (negative,
/// non-finite, or missing the trailing `"s"`), so a malformed header degrades to "no hint" instead
/// of a panic or a nonsensical wait.
fn parse_reset_duration(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (minutes, seconds_part) = match raw.split_once('m') {
        // "150ms" is milliseconds, not "150m" + "s" (no seconds digits between 'm' and 's').
        Some((_, rest)) if rest.starts_with('s') && rest[1..].parse::<f64>().is_err() => {
            let millis = parse_finite_nonneg(&raw[..raw.len() - 2])?;
            return Some(Duration::from_secs_f64(millis / 1000.0));
        }
        Some((mins, rest)) => (parse_finite_nonneg(mins)?, rest),
        None => (0.0, raw),
    };
    let seconds_part = seconds_part.strip_suffix('s')?;
    let seconds = if seconds_part.is_empty() {
        0.0
    } else {
        parse_finite_nonneg(seconds_part)?
    };
    Some(Duration::from_secs_f64(minutes * 60.0 + seconds))
}

/// Parse a finite, non-negative `f64`, rejecting `NaN`/`inf`/negative values that would make
/// [`Duration::from_secs_f64`] panic.
fn parse_finite_nonneg(s: &str) -> Option<f64> {
    let v: f64 = s.parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// Given the rate-limit state observed on a response, suggest a wait before the next attempt —
/// or `None` when there is no header-derived reason to wait longer than ordinary exponential
/// backoff (i.e. plenty of budget remains, or no rate-limit headers were present at all).
///
/// This is the decision [`crate::providers::compat`]'s retry loop would call on every attempt if
/// wired up (see the module docs for why that wiring isn't done in this file). It waits for
/// whichever window (requests or tokens) is actually exhausted, since retrying before either
/// resets is guaranteed to draw another 429.
pub fn suggested_backoff(snapshot: &RateLimitSnapshot) -> Option<Duration> {
    let requests_exhausted = snapshot.remaining_requests == Some(0);
    let tokens_exhausted = snapshot.remaining_tokens == Some(0);
    match (requests_exhausted, tokens_exhausted) {
        (true, true) => snapshot.reset_requests.max(snapshot.reset_tokens),
        (true, false) => snapshot.reset_requests,
        (false, true) => snapshot.reset_tokens,
        (false, false) => None,
    }
}

/// Groq — LPU-backed low-latency inference.
pub struct GroqProvider {
    compat: CompatProvider,
}

impl GroqProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let api_key = std::env::var("GROQ_API_KEY").ok();
        let base_url = std::env::var("GROQ_BASE_URL").ok();
        let config = build_config(
            "groq",
            base_url,
            GROQ_DEFAULT_BASE_URL,
            model.model,
            api_key,
            "GROQ_API_KEY",
        )?;
        let compat = CompatProvider::new(config, clock)?;
        Ok(GroqProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "groq",
            display_name: "Groq",
            env_vars: &[
                EnvVarRequirement {
                    name: "GROQ_API_KEY",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "GROQ_BASE_URL",
                    required: false,
                    description: "Override the default https://api.groq.com/openai/v1",
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
impl Provider for GroqProvider {
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

/// Cerebras — wafer-scale-engine-backed low-latency inference.
pub struct CerebrasProvider {
    compat: CompatProvider,
}

impl CerebrasProvider {
    /// Build a provider for `model`, reading configuration from the environment.
    pub fn from_env(model: ModelId, clock: Arc<dyn Clock>) -> Result<Self, ProviderError> {
        let api_key = std::env::var("CEREBRAS_API_KEY").ok();
        let base_url = std::env::var("CEREBRAS_BASE_URL").ok();
        let config = build_config(
            "cerebras",
            base_url,
            CEREBRAS_DEFAULT_BASE_URL,
            model.model,
            api_key,
            "CEREBRAS_API_KEY",
        )?;
        let compat = CompatProvider::new(config, clock)?;
        Ok(CerebrasProvider { compat })
    }

    /// Static capability/env-var metadata; see `providers/mod.rs` module docs for the contract.
    pub fn info() -> ProviderInfo {
        ProviderInfo {
            id: "cerebras",
            display_name: "Cerebras",
            env_vars: &[
                EnvVarRequirement {
                    name: "CEREBRAS_API_KEY",
                    required: true,
                    description: "Bearer API key",
                },
                EnvVarRequirement {
                    name: "CEREBRAS_BASE_URL",
                    required: false,
                    description: "Override the default https://api.cerebras.ai/v1",
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
impl Provider for CerebrasProvider {
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
    use crate::providers::compat::AuthStyle;

    // ---- build_config / from_env wiring (pure — no env mutation, no network) ----

    #[test]
    fn build_config_missing_key_is_auth_failed_with_var_name() {
        let err = build_config(
            "groq",
            None,
            GROQ_DEFAULT_BASE_URL,
            "llama-3.3-70b".to_string(),
            None,
            "GROQ_API_KEY",
        )
        .unwrap_err();
        match err {
            ProviderError::AuthFailed(msg) => assert!(msg.contains("GROQ_API_KEY")),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn build_config_never_leaks_the_key_into_the_error() {
        let err = build_config(
            "groq",
            None,
            GROQ_DEFAULT_BASE_URL,
            "llama-3.3-70b".to_string(),
            None,
            "GROQ_API_KEY",
        )
        .unwrap_err();
        let ProviderError::AuthFailed(msg) = err else {
            panic!("expected AuthFailed");
        };
        assert!(
            !msg.contains("gsk_"),
            "message should not embed a key: {msg}"
        );
    }

    #[test]
    fn build_config_defaults_base_url_and_disables_embeddings() {
        let config = build_config(
            "groq",
            None,
            GROQ_DEFAULT_BASE_URL,
            "llama-3.3-70b".to_string(),
            Some("secret-key".to_string()),
            "GROQ_API_KEY",
        )
        .expect("key present, should build");
        assert_eq!(config.id, "groq");
        assert_eq!(config.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(config.model, "llama-3.3-70b");
        assert_eq!(config.api_key, Some("secret-key".to_string()));
        assert_eq!(config.auth_style, AuthStyle::Bearer);
        assert_eq!(config.embeddings_path, None);
    }

    #[test]
    fn build_config_honors_base_url_override() {
        let config = build_config(
            "cerebras",
            Some("https://proxy.internal/v1".to_string()),
            CEREBRAS_DEFAULT_BASE_URL,
            "llama3.1-8b".to_string(),
            Some("secret-key".to_string()),
            "CEREBRAS_API_KEY",
        )
        .expect("key present, should build");
        assert_eq!(config.base_url, "https://proxy.internal/v1");
    }

    #[test]
    fn cerebras_default_base_url_is_the_documented_endpoint() {
        let config = build_config(
            "cerebras",
            None,
            CEREBRAS_DEFAULT_BASE_URL,
            "llama3.1-8b".to_string(),
            Some("secret-key".to_string()),
            "CEREBRAS_API_KEY",
        )
        .expect("key present, should build");
        assert_eq!(config.base_url, "https://api.cerebras.ai/v1");
    }

    // ---- info() ----

    #[test]
    fn groq_info_declares_required_key_and_optional_base_url() {
        let info = GroqProvider::info();
        assert_eq!(info.id, "groq");
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "GROQ_API_KEY" && v.required));
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "GROQ_BASE_URL" && !v.required));
        assert!(!info.capabilities.embedding);
        assert!(info.capabilities.streaming);
    }

    #[test]
    fn cerebras_info_declares_required_key_and_optional_base_url() {
        let info = CerebrasProvider::info();
        assert_eq!(info.id, "cerebras");
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "CEREBRAS_API_KEY" && v.required));
        assert!(info
            .env_vars
            .iter()
            .any(|v| v.name == "CEREBRAS_BASE_URL" && !v.required));
        assert!(!info.capabilities.embedding);
    }

    // ---- rate-limit header parsing ----

    #[test]
    fn parse_rate_limit_headers_reads_groq_style_shape() {
        let headers = [
            ("x-ratelimit-limit-requests", "14400"),
            ("x-ratelimit-limit-tokens", "6000"),
            ("x-ratelimit-remaining-requests", "14399"),
            ("x-ratelimit-remaining-tokens", "5992"),
            ("x-ratelimit-reset-requests", "2m59.56s"),
            ("x-ratelimit-reset-tokens", "7.66s"),
        ];
        let snapshot = parse_rate_limit_headers(headers);
        assert_eq!(snapshot.limit_requests, Some(14400));
        assert_eq!(snapshot.limit_tokens, Some(6000));
        assert_eq!(snapshot.remaining_requests, Some(14399));
        assert_eq!(snapshot.remaining_tokens, Some(5992));
        assert_eq!(
            snapshot.reset_requests,
            Some(Duration::from_secs_f64(2.0 * 60.0 + 59.56))
        );
        assert_eq!(snapshot.reset_tokens, Some(Duration::from_secs_f64(7.66)));
    }

    #[test]
    fn parse_rate_limit_headers_is_case_insensitive() {
        let headers = [("X-RateLimit-Remaining-Requests", "0")];
        let snapshot = parse_rate_limit_headers(headers);
        assert_eq!(snapshot.remaining_requests, Some(0));
    }

    #[test]
    fn parse_rate_limit_headers_ignores_unrelated_headers() {
        let headers = [("content-type", "application/json"), ("retry-after", "5")];
        let snapshot = parse_rate_limit_headers(headers);
        assert_eq!(snapshot, RateLimitSnapshot::default());
    }

    #[test]
    fn parse_rate_limit_headers_tolerates_malformed_values() {
        let headers = [
            ("x-ratelimit-remaining-requests", "not-a-number"),
            ("x-ratelimit-reset-tokens", "soon"),
        ];
        let snapshot = parse_rate_limit_headers(headers);
        assert_eq!(snapshot.remaining_requests, None);
        assert_eq!(snapshot.reset_tokens, None);
    }

    #[test]
    fn parse_reset_duration_handles_seconds_only() {
        assert_eq!(
            parse_reset_duration("7.66s"),
            Some(Duration::from_secs_f64(7.66))
        );
    }

    #[test]
    fn parse_reset_duration_handles_minutes_and_seconds() {
        assert_eq!(
            parse_reset_duration("2m59.56s"),
            Some(Duration::from_secs_f64(179.56))
        );
    }

    #[test]
    fn parse_reset_duration_handles_milliseconds() {
        assert_eq!(
            parse_reset_duration("150ms"),
            Some(Duration::from_secs_f64(0.15))
        );
    }

    #[test]
    fn parse_reset_duration_rejects_negative_and_garbage() {
        assert_eq!(parse_reset_duration("-1s"), None);
        assert_eq!(parse_reset_duration("nope"), None);
        assert_eq!(parse_reset_duration(""), None);
    }

    // ---- suggested_backoff ----

    #[test]
    fn suggested_backoff_is_none_with_budget_remaining() {
        let snapshot = RateLimitSnapshot {
            remaining_requests: Some(10),
            remaining_tokens: Some(500),
            ..Default::default()
        };
        assert_eq!(suggested_backoff(&snapshot), None);
    }

    #[test]
    fn suggested_backoff_is_none_with_no_headers_observed() {
        assert_eq!(suggested_backoff(&RateLimitSnapshot::default()), None);
    }

    #[test]
    fn suggested_backoff_waits_for_request_window_when_only_requests_exhausted() {
        let snapshot = RateLimitSnapshot {
            remaining_requests: Some(0),
            remaining_tokens: Some(500),
            reset_requests: Some(Duration::from_secs(30)),
            reset_tokens: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        assert_eq!(suggested_backoff(&snapshot), Some(Duration::from_secs(30)));
    }

    #[test]
    fn suggested_backoff_waits_for_token_window_when_only_tokens_exhausted() {
        let snapshot = RateLimitSnapshot {
            remaining_requests: Some(10),
            remaining_tokens: Some(0),
            reset_tokens: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        assert_eq!(suggested_backoff(&snapshot), Some(Duration::from_secs(5)));
    }

    #[test]
    fn suggested_backoff_waits_for_the_longer_window_when_both_exhausted() {
        let snapshot = RateLimitSnapshot {
            remaining_requests: Some(0),
            remaining_tokens: Some(0),
            reset_requests: Some(Duration::from_secs(3)),
            reset_tokens: Some(Duration::from_secs(45)),
            ..Default::default()
        };
        assert_eq!(suggested_backoff(&snapshot), Some(Duration::from_secs(45)));
    }
}
