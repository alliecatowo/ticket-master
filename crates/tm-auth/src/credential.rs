//! [`Credential`]: resolved secret material that can never print or serialize itself.
//!
//! `SPEC.md` §28.2: "Credential material never enters the event log, a context pack, a `--json`
//! payload, an error message, or a rendered frame." Two independent enforcement mechanisms back
//! that sentence here:
//!
//! 1. [`Credential`] does **not** derive (or implement) `serde::Serialize`. Nothing that holds a
//!    `Credential` field can be serialized without a compile error, which rules out the
//!    `--json`-payload and event-log leak paths structurally rather than by convention.
//! 2. [`Credential`]'s `Debug`/`Display` impls print the fixed string `<redacted>` regardless of
//!    what the credential holds, which rules out the error-message and rendered-frame leak paths
//!    for any code that formats a `Credential` directly (`format!("{cred}")`,
//!    `format!("{cred:?}")`, `tracing::info!(?cred)`, ...).
//!
//! The one sanctioned escape hatch is [`Credential::expose_secret`], named loudly on purpose: a
//! caller that needs the actual bytes (to set an `Authorization` header, say) has to say so
//! explicitly at the call site, which is what makes every such call site `grep`-able.

use std::fmt;

/// Resolved credential material, returned by [`crate::AuthAdapter::credential`].
///
/// Deliberately **not** `#[derive(serde::Serialize)]` — see this module's docs. Holds a primary
/// secret (the API key, or an OAuth access token) and an optional secondary secret (an OAuth
/// refresh token), which covers every [`crate::CredentialKind`] this crate actually resolves
/// ([`crate::CredentialKind::ApiKey`] via [`crate::EnvApiKey`]/[`crate::KeychainApiKey`],
/// [`crate::CredentialKind::SubscriptionOAuth`] via [`crate::DeviceCodeOAuth`]) without a
/// variant for kinds no adapter in this crate resolves yet ([`crate::CredentialKind::CloudIam`],
/// [`crate::CredentialKind::PlatformEphemeral`], [`crate::CredentialKind::Delegated`]).
#[derive(Clone)]
pub struct Credential {
    secret: String,
    refresh: Option<String>,
}

impl Credential {
    /// Wrap a bare API key / bearer token as a credential with no refresh material.
    pub fn api_key(secret: impl Into<String>) -> Self {
        Credential {
            secret: secret.into(),
            refresh: None,
        }
    }

    /// Wrap an OAuth access token, with an optional refresh token alongside it.
    pub fn oauth(access_token: impl Into<String>, refresh_token: Option<String>) -> Self {
        Credential {
            secret: access_token.into(),
            refresh: refresh_token,
        }
    }

    /// Expose the primary secret (API key, or OAuth access token) as a borrowed `&str`.
    ///
    /// This is the one sanctioned way to read the actual credential bytes back out — named
    /// `expose_secret` rather than something anodyne like `as_str` so every call site is
    /// self-documenting and `grep -rn expose_secret` finds every place the raw value leaves this
    /// type. Callers must not log, serialize, or otherwise persist the returned `&str` outside
    /// the immediate use (e.g. building one HTTP header) it was exposed for.
    pub fn expose_secret(&self) -> &str {
        &self.secret
    }

    /// Expose the OAuth refresh token, if this credential carries one.
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh.as_deref()
    }
}

/// Always `<redacted>`, regardless of what this credential holds.
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted>")
    }
}

/// Always `<redacted>`, regardless of what this credential holds.
impl fmt::Display for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_are_fixed_redacted_strings() {
        let cred = Credential::api_key("sk-super-secret-value");
        assert_eq!(format!("{cred}"), "<redacted>");
        assert_eq!(format!("{cred:?}"), "<redacted>");
        assert!(!format!("{cred}").contains("sk-super-secret-value"));
        assert!(!format!("{cred:?}").contains("sk-super-secret-value"));
    }

    #[test]
    fn expose_secret_returns_the_wrapped_value() {
        let cred = Credential::api_key("sk-super-secret-value");
        assert_eq!(cred.expose_secret(), "sk-super-secret-value");
    }

    #[test]
    fn oauth_credential_carries_refresh_token_separately_from_display() {
        let cred = Credential::oauth("access-tok", Some("refresh-tok".to_string()));
        assert_eq!(cred.expose_secret(), "access-tok");
        assert_eq!(cred.refresh_token(), Some("refresh-tok"));
        assert_eq!(format!("{cred}"), "<redacted>");
        assert_eq!(format!("{cred:?}"), "<redacted>");
    }

    // `Credential` intentionally does not implement `serde::Serialize`. There is no runtime test
    // for that -- the guarantee is enforced by review of this file (a `#[derive(Serialize)]`
    // added back here would compile cleanly; nothing here would fail), not by a tripwire. What
    // *does* fail at compile time is any downstream call site that tries to pass a `Credential`
    // through a `T: Serialize` bound, e.g. `tm_cli::render::Renderer::emit<T: Serialize>` -- see
    // `crates/tm-cli/src/auth.rs`, which resolves credentials only to check `.is_ok()` and never
    // hands one to `emit`.
}
