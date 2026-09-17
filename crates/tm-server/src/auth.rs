//! Bind-address-aware authentication (`SPEC.md` §14: "local socket / loopback by default;
//! bearer token when bound to a non-loopback address").
//!
//! The policy is a pure function of the bind address: [`BindAddress::requires_token`] decides
//! whether a token is required at all, entirely independent of any given request — so it's unit
//! tested directly, without spinning up axum. [`authenticate`] is the axum middleware that
//! applies that policy per request, using [`token_matches`] (constant-time) to compare whatever
//! `Authorization: Bearer <token>` header arrived against the configured token.

use std::net::SocketAddr;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;

use crate::state::{AppState, ServerConfig, ServerError};

/// Server authentication configuration (bind address and optional bearer token).
pub type AuthConfig = ServerConfig;

/// The address a server instance is bound to, wrapping the loopback-vs-not policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindAddress(pub SocketAddr);

impl BindAddress {
    /// True when this address is loopback (`127.0.0.0/8` or `::1`), and thus per `SPEC.md` §14
    /// does **not** require a bearer token.
    pub fn is_loopback(&self) -> bool {
        self.0.ip().is_loopback()
    }

    /// The inverse of [`BindAddress::is_loopback`]: whether a bearer token must be present and
    /// valid on every request.
    pub fn requires_token(&self) -> bool {
        !self.is_loopback()
    }
}

/// Why [`authenticate`] rejected a request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The server is bound non-loopback, and the request carried no `Authorization` header (or
    /// not a `Bearer` one).
    #[error("missing bearer token")]
    MissingToken,
    /// A bearer token was present but did not match the configured token.
    #[error("invalid bearer token")]
    InvalidToken,
    /// The server is configured to require a token (non-loopback bind) but has none configured
    /// (neither `ServerConfig::token` nor the environment supplied one) — a startup
    /// misconfiguration, surfaced per-request as a safe refusal rather than silently allowing
    /// everyone through.
    #[error("server requires a token but none is configured")]
    NotConfigured,
}

impl From<AuthError> for ServerError {
    fn from(_: AuthError) -> Self {
        ServerError::Unauthorized
    }
}

/// Resolve the effective bearer token: `configured` (from [`crate::state::ServerConfig::token`])
/// if set, otherwise the `TM_SERVER_TOKEN` environment variable.
///
/// Impure (reads the environment) by necessity — kept as the one small I/O seam so
/// [`token_matches`] and [`BindAddress::requires_token`] stay pure and unit-testable.
pub fn resolve_token(configured: Option<&str>) -> Option<String> {
    configured
        .map(str::to_owned)
        .or_else(|| std::env::var("TM_SERVER_TOKEN").ok())
}

/// Constant-time comparison of `provided` against `expected`, so token comparison time does not
/// leak how many leading bytes matched.
///
/// # Invariant
/// Must run in time independent of *where* `provided` and `expected` first differ — an early
/// `return false` on the first mismatched byte, or on a length mismatch, defeats the purpose.
pub fn token_matches(expected: &str, provided: &str) -> bool {
    let expected_bytes = expected.as_bytes();
    let provided_bytes = provided.as_bytes();

    let mut result = 0u8;
    let max_len = expected_bytes.len().max(provided_bytes.len());

    for i in 0..max_len {
        let e = *expected_bytes.get(i).unwrap_or(&0);
        let p = *provided_bytes.get(i).unwrap_or(&0);
        result |= e ^ p;
    }

    result |= (expected_bytes.len() != provided_bytes.len()) as u8;

    result == 0
}

/// Extract the bearer token from a request's `Authorization` header, if present and
/// well-formed (`Bearer <token>`).
pub fn bearer_token(req: &Request) -> Option<&str> {
    req.headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}

/// Axum middleware: when `state.config.requires_auth()`, reject the request unless
/// [`bearer_token`] is present and [`token_matches`] the resolved token; otherwise (loopback
/// bind) pass every request through unchanged.
///
/// # Errors
/// [`ServerError::Unauthorized`] (via [`AuthError`]) on a missing, invalid, or (misconfigured)
/// absent-but-required token.
pub async fn authenticate(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ServerError> {
    if !state.config.requires_auth() {
        return Ok(next.run(req).await);
    }

    let token = resolve_token(state.config.token.as_deref()).ok_or(AuthError::NotConfigured)?;

    let provided = bearer_token(&req).ok_or(AuthError::MissingToken)?;

    if !token_matches(&token, provided) {
        return Err(AuthError::InvalidToken.into());
    }

    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;

    #[test]
    fn loopback_v4_not_required_auth() {
        let bind = BindAddress(SocketAddr::from((Ipv4Addr::LOCALHOST, 8080)));
        assert!(bind.is_loopback());
        assert!(!bind.requires_token());
    }

    #[test]
    fn loopback_v6_not_required_auth() {
        let bind = BindAddress(SocketAddr::from((Ipv6Addr::LOCALHOST, 8080)));
        assert!(bind.is_loopback());
        assert!(!bind.requires_token());
    }

    #[test]
    fn non_loopback_requires_auth() {
        let bind = BindAddress(SocketAddr::from((
            IpAddr::from_str("192.168.1.1").unwrap(),
            8080,
        )));
        assert!(!bind.is_loopback());
        assert!(bind.requires_token());
    }

    #[test]
    fn any_v4_requires_auth() {
        let bind = BindAddress(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8080)));
        assert!(!bind.is_loopback());
        assert!(bind.requires_token());
    }

    #[test]
    fn resolve_token_prefers_configured() {
        let result = resolve_token(Some("configured"));
        assert_eq!(result, Some("configured".to_owned()));
    }

    #[test]
    fn resolve_token_empty_string_is_valid() {
        let result = resolve_token(Some(""));
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn resolve_token_falls_back_to_env() {
        std::env::set_var("TM_SERVER_TOKEN", "from_env");
        let result = resolve_token(None);
        assert_eq!(result, Some("from_env".to_owned()));
        std::env::remove_var("TM_SERVER_TOKEN");
    }

    #[test]
    fn resolve_token_env_not_set_returns_none() {
        std::env::remove_var("TM_SERVER_TOKEN");
        let result = resolve_token(None);
        assert_eq!(result, None);
    }

    #[test]
    fn token_matches_exact_match() {
        assert!(token_matches("secret", "secret"));
    }

    #[test]
    fn token_matches_empty() {
        assert!(token_matches("", ""));
    }

    #[test]
    fn token_matches_different_tokens() {
        assert!(!token_matches("secret1", "secret2"));
    }

    #[test]
    fn token_matches_length_mismatch() {
        assert!(!token_matches("short", "much_longer_token"));
    }

    #[test]
    fn token_matches_prefix_mismatch() {
        assert!(!token_matches("secret", "secre"));
    }

    #[test]
    fn token_matches_suffix_mismatch() {
        assert!(!token_matches("secret", "secert"));
    }

    #[test]
    fn token_matches_one_char_differs() {
        assert!(!token_matches("a", "b"));
    }

    #[test]
    fn token_matches_middle_char_differs() {
        assert!(!token_matches("abcdef", "abXdef"));
    }

    #[test]
    fn bearer_token_valid() {
        let req = axum::http::Request::builder()
            .header("Authorization", "Bearer mytoken123")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(bearer_token(&req), Some("mytoken123"));
    }

    #[test]
    fn bearer_token_with_spaces_in_token() {
        let req = axum::http::Request::builder()
            .header("Authorization", "Bearer my token 123")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(bearer_token(&req), Some("my token 123"));
    }

    #[test]
    fn bearer_token_missing_header() {
        let req = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(bearer_token(&req), None);
    }

    #[test]
    fn bearer_token_wrong_scheme() {
        let req = axum::http::Request::builder()
            .header("Authorization", "Basic dXNlcjpwYXNzd29yZA==")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(bearer_token(&req), None);
    }

    #[test]
    fn bearer_token_bearer_without_token() {
        let req = axum::http::Request::builder()
            .header("Authorization", "Bearer ")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(bearer_token(&req), Some(""));
    }

    #[test]
    fn bearer_token_bearer_without_space() {
        let req = axum::http::Request::builder()
            .header("Authorization", "Bearer")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(bearer_token(&req), None);
    }

    #[test]
    fn bearer_token_invalid_header_value() {
        let req = axum::http::Request::builder()
            .header("Authorization", vec![0xFF, 0xFE])
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(bearer_token(&req), None);
    }
}
