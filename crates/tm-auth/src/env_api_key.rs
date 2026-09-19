//! [`EnvApiKey`]: the base-case [`crate::AuthAdapter`] — a named environment variable, read once
//! per call, behind the trait. This is what most of the ~18 raw `std::env::var` credential reads
//! across `tm-provider`/`tm-mirror`/`tm-server` are meant to migrate to.

use async_trait::async_trait;

use crate::{AuthAdapter, AuthError, Credential, CredentialKind};

/// Reads a credential from a named environment variable. Trivially constructible with just the
/// variable name — [`EnvApiKey::new`] takes only that, and uses it as both the adapter's `id`
/// and the variable to read, since one name is all a caller usually has (`ANTHROPIC_API_KEY`,
/// `OPENAI_API_KEY`, ...). Use [`EnvApiKey::with_id`] when the adapter needs a distinct id (e.g.
/// two adapters reading the same variable under different routing names).
#[derive(Debug, Clone)]
pub struct EnvApiKey {
    id: String,
    var_name: String,
}

impl EnvApiKey {
    /// Build an adapter that reads `var_name`, using `var_name` itself as the adapter id.
    pub fn new(var_name: impl Into<String>) -> Self {
        let var_name = var_name.into();
        EnvApiKey {
            id: var_name.clone(),
            var_name,
        }
    }

    /// Build an adapter with an id distinct from the environment variable it reads.
    pub fn with_id(id: impl Into<String>, var_name: impl Into<String>) -> Self {
        EnvApiKey {
            id: id.into(),
            var_name: var_name.into(),
        }
    }

    /// The environment variable this adapter reads.
    pub fn var_name(&self) -> &str {
        &self.var_name
    }

    /// Resolve the credential synchronously. A plain environment read never actually needs to
    /// `.await` anything; this is the sync entry point call sites that already run inside a sync
    /// function (most provider `from_env` constructors) should use, and
    /// [`AuthAdapter::credential`] below is a thin async wrapper over it for callers that only
    /// hold a `dyn AuthAdapter`.
    pub fn resolve(&self) -> Result<Credential, AuthError> {
        std::env::var(&self.var_name)
            .map(Credential::api_key)
            .map_err(|_| AuthError::MissingEnvVar(self.var_name.clone()))
    }
}

#[async_trait]
impl AuthAdapter for EnvApiKey {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> CredentialKind {
        CredentialKind::ApiKey
    }

    async fn credential(&self) -> Result<Credential, AuthError> {
        self.resolve()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hygiene (`crates/xtask/src/hygiene.rs`'s `check_network_in_tests`) flags any test that
    // reads a real provider env var name like `ANTHROPIC_API_KEY` via `var(`/`set_var`, since
    // that's the "test depends on a real credential being present" hazard it exists to catch.
    // Every var name below is synthetic and unique per test (env vars are process-global state
    // shared across every test thread in this binary; a shared name would race).

    #[test]
    fn missing_var_reports_missing_env_var() {
        let adapter = EnvApiKey::new("TM_AUTH_TEST_ENV_API_KEY_MISSING_CASE");
        std::env::remove_var("TM_AUTH_TEST_ENV_API_KEY_MISSING_CASE");
        let err = adapter.resolve().expect_err("var is not set");
        assert_eq!(
            err,
            AuthError::MissingEnvVar("TM_AUTH_TEST_ENV_API_KEY_MISSING_CASE".to_string())
        );
    }

    #[test]
    fn present_var_resolves_to_its_value() {
        let var = "TM_AUTH_TEST_ENV_API_KEY_PRESENT_CASE";
        // SAFETY (concurrency, not memory): unique var name per test avoids the cross-test race
        // `set_var`/`remove_var` would otherwise create against other tests in this binary.
        std::env::set_var(var, "test-only-resolved-value");
        let adapter = EnvApiKey::new(var);
        let cred = adapter.resolve().expect("var is set");
        assert_eq!(cred.expose_secret(), "test-only-resolved-value");
        std::env::remove_var(var);
    }

    #[test]
    fn with_id_keeps_id_and_var_name_distinct() {
        let var = "TM_AUTH_TEST_ENV_API_KEY_WITH_ID_CASE";
        std::env::set_var(var, "distinct-id-value");
        let adapter = EnvApiKey::with_id("anthropic-env", var);
        assert_eq!(adapter.id(), "anthropic-env");
        assert_eq!(adapter.var_name(), var);
        assert_eq!(
            adapter.resolve().expect("var is set").expose_secret(),
            "distinct-id-value"
        );
        std::env::remove_var(var);
    }

    #[tokio::test]
    async fn async_trait_method_matches_sync_resolve() {
        let var = "TM_AUTH_TEST_ENV_API_KEY_ASYNC_CASE";
        std::env::set_var(var, "async-path-value");
        let adapter: Box<dyn AuthAdapter> = Box::new(EnvApiKey::new(var));
        let cred = adapter.credential().await.expect("var is set");
        assert_eq!(cred.expose_secret(), "async-path-value");
        std::env::remove_var(var);
    }
}
