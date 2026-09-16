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

use crate::state::{AppState, ServerError};

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
    // IMPL: configured.map(str::to_owned).or_else(|| std::env::var("TM_SERVER_TOKEN").ok()).
    todo!("prefer the configured token, falling back to the TM_SERVER_TOKEN env var")
}

/// Constant-time comparison of `provided` against `expected`, so token comparison time does not
/// leak how many leading bytes matched.
///
/// # Invariant
/// Must run in time independent of *where* `provided` and `expected` first differ — an early
/// `return false` on the first mismatched byte, or on a length mismatch, defeats the purpose.
pub fn token_matches(expected: &str, provided: &str) -> bool {
    // IMPL: fold XOR of every byte pair over the longer of the two lengths (treating missing
    // bytes as a fixed sentinel so length itself doesn't short-circuit), OR in a length-mismatch
    // flag, and only branch on the combined result at the very end. Do not use `==` or early
    // `return` inside the loop.
    todo!("byte-wise constant-time equality check, no early return")
}

/// Extract the bearer token from a request's `Authorization` header, if present and
/// well-formed (`Bearer <token>`).
pub fn bearer_token(req: &Request) -> Option<&str> {
    // IMPL: req.headers().get(AUTHORIZATION), to_str().ok(), strip_prefix("Bearer ").
    todo!("parse the Authorization header for a Bearer token")
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
    // IMPL: if !state.config.requires_auth() { return Ok(next.run(req).await); } otherwise
    // resolve_token(state.config.token.as_deref()) -> AuthError::NotConfigured if None;
    // bearer_token(&req) -> AuthError::MissingToken if None; token_matches(...) -> else
    // AuthError::InvalidToken; on success, next.run(req).await.
    todo!("apply BindAddress::requires_token policy, comparing tokens with token_matches")
}
