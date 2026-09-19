//! [`AuthError`]: every failure mode an [`crate::AuthAdapter`] can report.
//!
//! Every variant here is constructed from identifiers (an env var name, an adapter id, a
//! provider slug) and human-authored strings that never touch credential material — none of
//! this crate's adapters build an `AuthError` from `Credential::expose_secret`'s output. That
//! makes `Display`ing an `AuthError` safe by construction, unlike `Credential` (`SPEC.md` §28.2:
//! "a refresh failure degrades to a routing decision ... rather than an exception" — the
//! `Display` text is exactly what a caller is expected to surface as that routing decision).

use thiserror::Error;

/// A credential could not be resolved, or a resolved one could not be refreshed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The named environment variable holding a credential is not set.
    #[error("{0} is not set")]
    MissingEnvVar(String),

    /// A feature this adapter's *shape* supports but whose implementation is not yet wired up —
    /// [`crate::KeychainApiKey`]'s current state on every platform. Distinguished from
    /// [`AuthError::Unavailable`] so a caller can tell "this will never work here" apart from
    /// "this isn't built yet".
    #[error("{0}: not yet implemented")]
    NotImplemented(String),

    /// The device authorization / token endpoint rejected the request, or the HTTP call to it
    /// failed. `detail` is server- or transport-provided prose, never credential material.
    #[error("oauth flow failed: {detail}")]
    OAuthFailed {
        /// What went wrong, safe to display (an HTTP status, an OAuth `error` field, a
        /// connection failure message — never a token).
        detail: String,
    },

    /// The device code expired before the user completed the authorization step.
    #[error("device code expired before authorization completed")]
    DeviceCodeExpired,

    /// The user (or an out-of-band policy) denied the authorization request.
    #[error("authorization was denied")]
    AccessDenied,

    /// A refresh token was needed but none is held.
    #[error("no refresh token available")]
    NoRefreshToken,

    /// The credential resolved but this adapter/platform combination cannot provide it — e.g.
    /// [`crate::KeychainApiKey`] on a non-macOS target.
    #[error("{0}")]
    Unavailable(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_text_never_needs_a_credential_value_to_be_meaningful() {
        // Every variant is constructed here from identifiers/prose only, proving by
        // construction that `AuthError` cannot carry a `Credential`'s secret: there is no
        // variant field typed to accept one.
        let cases = [
            AuthError::MissingEnvVar("SOME_KEY".to_string()),
            AuthError::NotImplemented("keychain".to_string()),
            AuthError::OAuthFailed {
                detail: "http 400: invalid_grant".to_string(),
            },
            AuthError::DeviceCodeExpired,
            AuthError::AccessDenied,
            AuthError::NoRefreshToken,
            AuthError::Unavailable("no keychain on this platform".to_string()),
        ];
        for case in cases {
            let rendered = format!("{case}");
            assert!(!rendered.is_empty());
        }
    }
}
