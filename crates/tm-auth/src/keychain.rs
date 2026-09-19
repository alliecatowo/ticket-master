//! [`KeychainApiKey`]: OS keychain-backed credential storage.
//!
//! # Scope decision: shape only, integration left as a follow-up
//!
//! `SPEC.md` §28.2 says interactive login should store "to the OS keychain by default", and the
//! audit names `security-framework` (macOS) / `secret-service` (Linux) as candidates, flagging
//! it explicitly as "needs a dependency decision". Neither crate is a workspace dependency today
//! (checked `Cargo.toml`'s `[workspace.dependencies]` before writing this module).
//!
//! Adding `security-framework` is a real new-dependency decision, not a mechanical one: it links
//! against macOS's Security framework, has no story on Linux/Windows (this workspace ships a CLI
//! that presumably needs to run on more than one OS), and a `secret-service` alternative pulls in
//! its own D-Bus stack on Linux. Per this task's own instructions, guessing at that call without
//! a human isn't the right move — so this module builds the adapter's trait/type shape (matching
//! [`crate::EnvApiKey`]'s constructor ergonomics) and every method returns
//! [`crate::AuthError::NotImplemented`] rather than either (a) silently falling back to an
//! insecure store, or (b) picking a dependency unreviewed. Wiring the real macOS Keychain
//! integration behind `#[cfg(target_os = "macos")]` (with a clear "not supported on this
//! platform" error elsewhere, never a silent insecure fallback) is the documented follow-up.

use async_trait::async_trait;

use crate::{AuthAdapter, AuthError, Credential, CredentialKind};

/// OS keychain-backed API key storage, addressed by `service`/`account` the way every native
/// keychain API (macOS Keychain Services, Linux secret-service, Windows Credential Manager)
/// already does. Trivially constructible, matching [`crate::EnvApiKey::new`]'s ergonomics.
///
/// # Current status
///
/// Every method returns [`AuthError::NotImplemented`] on every platform right now — see the
/// module docs for why the macOS integration is deliberately not wired up in this change. This
/// is not a stub that silently degrades to reading plaintext or an environment variable; a
/// caller that reaches for keychain storage today gets a loud, typed error instead.
#[derive(Debug, Clone)]
pub struct KeychainApiKey {
    id: String,
    service: String,
    account: String,
}

impl KeychainApiKey {
    /// Build an adapter addressing `service`/`account` in the platform keychain, using
    /// `"{service}/{account}"` as the adapter id.
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        let service = service.into();
        let account = account.into();
        let id = format!("{service}/{account}");
        KeychainApiKey {
            id,
            service,
            account,
        }
    }

    /// The keychain service name this adapter addresses.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// The keychain account name this adapter addresses.
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Not yet implemented on any platform. See the module docs for the dependency decision this
    /// is blocked on.
    fn not_implemented(&self) -> AuthError {
        AuthError::NotImplemented(format!(
            "OS keychain storage ({}/{}): macOS Keychain integration via `security-framework` \
             is a deliberate follow-up, not implemented here — see crates/tm-auth/src/keychain.rs",
            self.service, self.account
        ))
    }
}

#[async_trait]
impl AuthAdapter for KeychainApiKey {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> CredentialKind {
        CredentialKind::ApiKey
    }

    async fn credential(&self) -> Result<Credential, AuthError> {
        Err(self.not_implemented())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_composes_a_readable_id() {
        let adapter = KeychainApiKey::new("ticket-master", "anthropic");
        assert_eq!(adapter.id(), "ticket-master/anthropic");
        assert_eq!(adapter.service(), "ticket-master");
        assert_eq!(adapter.account(), "anthropic");
        assert_eq!(adapter.kind(), CredentialKind::ApiKey);
    }

    #[tokio::test]
    async fn credential_reports_not_implemented_not_a_silent_fallback() {
        let adapter = KeychainApiKey::new("ticket-master", "anthropic");
        let err = adapter.credential().await.expect_err("not implemented yet");
        assert!(matches!(err, AuthError::NotImplemented(_)));
    }
}
