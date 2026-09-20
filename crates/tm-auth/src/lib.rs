#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Auth adapters (`SPEC.md` §28.2): "how are we *allowed* to use it", kept as a pluggable
//! concern distinct from the provider/protocol/runtime layers around it (§28).
//!
//! Authentication is routing input, not a connection detail: the same provider is reachable
//! several ways, and which way changes cost, rate limits and what's permitted. Before this
//! crate, every one of the workspace's ~18 credential reads was a bare `std::env::var` call
//! scattered through `tm-provider`/`tm-mirror`/`tm-server`, with no shared shape for "what am I
//! entitled to" and no single choke point a redaction guarantee could be proven against.
//!
//! - [`AuthAdapter`]: `id` / `kind` / `credential` / `entitlement`. Every credential source in
//!   the workspace is meant to implement this, not read `std::env::var` directly.
//! - [`Credential`]: the resolved secret material. `Debug`/`Display` are a fixed `<redacted>`
//!   string and it deliberately does **not** derive `Serialize` — a credential that can leak
//!   into a `--json` payload or an event log is exactly the failure mode this crate exists to
//!   prevent. See [`Credential`]'s docs for the one sanctioned way to read the secret out.
//! - [`EnvApiKey`]: reads a named environment variable. The simplest adapter and the one most
//!   existing call sites should migrate to.
//! - [`KeychainApiKey`]: OS keychain storage. Trait/type shape only — see its docs for why the
//!   macOS Keychain integration itself is a documented follow-up rather than guessed at here.
//! - [`DeviceCodeOAuth`]: OAuth 2.0 device authorization grant (RFC 8628) with refresh, built on
//!   `reqwest` (already a workspace dependency) rather than a dedicated OAuth crate.
//! - [`Entitlement`] / [`QuotaClass`]: what an adapter reports itself entitled to, so the fabric
//!   can route on subscription-vs-metered capacity per §28.2/§31.1.
//! - [`redact`]: a distinct, complementary guarantee to [`Credential`]'s -- scans *arbitrary*
//!   text for secret-*shaped* substrings this crate never resolved (a tool's stdout, a model's
//!   echoed output) rather than redacting a known value. See [`redact` module docs][redact] for
//!   the full split between the pure/persistence-path redactor and [`SessionRedactor`]'s
//!   in-memory-only local restore.

mod credential;
mod device_code;
mod entitlement;
mod env_api_key;
mod error;
mod keychain;
pub mod redact;

pub use credential::Credential;
pub use device_code::{
    DeviceAuthorizationResponse, DeviceCodeConfig, DeviceCodeOAuth, InMemoryTokenStore,
    OAuthTokens, PollOutcome, TokenStore,
};
pub use entitlement::{Entitlement, QuotaClass};
pub use env_api_key::EnvApiKey;
pub use error::AuthError;
pub use keychain::KeychainApiKey;
pub use redact::{redact, redact_json, SessionRedactor};

use async_trait::async_trait;

/// The kind of credential an [`AuthAdapter`] resolves, per `SPEC.md` §28.2's exact list.
///
/// This is reported alongside every credential (never inferred from the adapter's type alone)
/// because it changes routing behaviour: the fabric treats subscription capacity and metered
/// spend differently (§31.1), and an executor holding its own credentials (`Delegated`) is never
/// something this crate tries to resolve on its behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialKind {
    /// A bearer secret read from a named environment variable or the OS keychain. The base
    /// case; never from the repository, never logged, never returned by any introspection call.
    ApiKey,
    /// Device-code or PKCE flow against a consumer plan (a ChatGPT Plus/Pro subscription, a
    /// Claude Pro/Max plan, a Gemini plan), yielding a refreshable token used instead of metered
    /// API credits.
    SubscriptionOAuth,
    /// AWS SigV4 with the ambient credential chain, Google application default credentials and
    /// service-account impersonation, Azure AD. Carries its own expiry/refresh semantics.
    CloudIam,
    /// A platform-issued, scoped, short-lived token minted per unit of work — a GitHub App
    /// installation token is the model.
    PlatformEphemeral,
    /// The executor holds its own credentials and we never see them (the common case for an
    /// external harness a human already logged into), or no auth is needed at all.
    Delegated,
}

/// A pluggable credential source. Every credential read in the workspace should go through an
/// implementation of this trait rather than a bare `std::env::var`/keychain/OAuth call, so
/// redaction (see [`Credential`]) and entitlement reporting (see [`Entitlement`]) are enforced
/// in one place instead of re-derived ad hoc at every call site.
#[async_trait]
pub trait AuthAdapter: Send + Sync {
    /// This adapter's stable identifier, e.g. `"anthropic-env"` or `"anthropic-oauth"`. Distinct
    /// from the *provider* slug (`tm_provider`'s `"anthropic"`) because one provider can have
    /// several adapters (an API key and a subscription OAuth flow both reach Anthropic).
    fn id(&self) -> &str;

    /// Which of `SPEC.md` §28.2's five credential kinds this adapter resolves.
    fn kind(&self) -> CredentialKind;

    /// Resolve the credential. Refresh (for kinds that have it) happens transparently inside
    /// this call — a caller never sees a stale token, and a refresh failure is reported as an
    /// [`AuthError`] (a routing input: "this candidate is unavailable"), never a panic.
    async fn credential(&self) -> Result<Credential, AuthError>;

    /// What this adapter's credential is entitled to: quota class, rate limits, whether spend is
    /// metered or subscription capacity, and any daily ceiling (§28.2, §31.1). Adapters that
    /// cannot introspect real quota (most API-key backends have no such endpoint) return
    /// [`Entitlement::default`], which is conservative (`metered: true`, no known ceiling) rather
    /// than silently claiming unlimited/free capacity.
    fn entitlement(&self) -> Entitlement {
        Entitlement::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal adapter proving the trait is object-safe / usable through `dyn AuthAdapter`,
    /// the shape every real caller (a registry keyed by provider slug) needs.
    struct Fixed(String);

    #[async_trait]
    impl AuthAdapter for Fixed {
        fn id(&self) -> &str {
            "fixed"
        }
        fn kind(&self) -> CredentialKind {
            CredentialKind::ApiKey
        }
        async fn credential(&self) -> Result<Credential, AuthError> {
            Ok(Credential::api_key(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn dyn_auth_adapter_resolves_credential() {
        let adapter: Box<dyn AuthAdapter> = Box::new(Fixed("secret-value".to_string()));
        let cred = adapter.credential().await.expect("resolve");
        assert_eq!(cred.expose_secret(), "secret-value");
        assert_eq!(adapter.kind(), CredentialKind::ApiKey);
        assert_eq!(adapter.entitlement(), Entitlement::default());
    }
}
