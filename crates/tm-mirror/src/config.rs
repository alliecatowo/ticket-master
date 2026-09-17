//! `mirror.toml` parsing and validation.
//!
//! Owns: which adapters are attached (several may be attached at once — a project can mirror to
//! Linear for product and GitHub Issues for open-source contributors simultaneously), where each
//! adapter's credentials live (named environment variables *only* — a credential value is never
//! written to this file or held on [`AdapterConfig`]), per-adapter [`ProjectionOverrides`], and
//! the internal-state-to-external-state mapping table each adapter consults when its
//! [`crate::tracker::TrackerCapabilities::arbitrary_states`] is false.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tm_types::Result;

/// Which external system an adapter table talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdapterKind {
    /// GitHub Issues, REST v3.
    GitHub,
    /// Linear, GraphQL.
    Linear,
    /// Jira Cloud, REST v3.
    Jira,
    /// GitLab Issues, REST v4.
    GitLab,
    /// Discards everything; a table kept present but inert, or a safe explicit default.
    Null,
}

/// Per-adapter overrides to [`crate::projection::ProjectionPolicy`]'s defaults. `None`/empty
/// means "use the policy default".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionOverrides {
    /// Restrict mirroring to tickets under this milestone id, if set.
    #[serde(default)]
    pub milestone: Option<String>,
    /// Extra labels to apply to every issue this adapter creates, in addition to the policy's.
    #[serde(default)]
    pub extra_labels: Vec<String>,
    /// Force checklist rollup even when the adapter declares `parent_child = true`, for teams
    /// that prefer a single issue per milestone regardless of adapter capability.
    #[serde(default)]
    pub force_checklist_rollup: Option<bool>,
}

/// Which environment variable holds each credential field an adapter needs. Values here are
/// variable *names* (e.g. `"GITHUB_TOKEN"`), never secret values — `mirror.toml` never stores a
/// credential itself, only where to find one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialEnv {
    /// Credential field name (e.g. `"token"`, `"api_key"`, `"email"`) -> environment variable
    /// name to read it from.
    #[serde(flatten)]
    pub vars: BTreeMap<String, String>,
}

impl CredentialEnv {
    /// Read every named variable from the process environment in one pass. Fails closed: the
    /// first missing variable is an error naming both the credential field and the variable
    /// name, rather than a partially populated map.
    pub fn resolve(&self) -> Result<BTreeMap<String, String>> {
        let mut resolved = BTreeMap::new();
        for (field, var_name) in &self.vars {
            match std::env::var(var_name) {
                Ok(value) => {
                    resolved.insert(field.clone(), value);
                }
                Err(_) => {
                    return Err(tm_types::TmError::invariant(format!(
                        "mirror: env var {var_name} (credential field {field:?}) is not set"
                    )));
                }
            }
        }
        Ok(resolved)
    }
}

/// One `[adapters.<name>]` table in `mirror.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterConfig {
    /// Instance name (the TOML table key); unique among all adapters in one config. Stamped in
    /// by [`MirrorConfig::parse`] after deserialization, since a map key isn't itself a field
    /// serde can populate.
    #[serde(skip)]
    pub name: String,
    /// Which external system this instance talks to.
    pub kind: AdapterKind,
    /// Whether this adapter is active. A disabled table is kept, not removed, so its
    /// credentials/overrides stay documented without taking effect.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Named environment variables this adapter reads its credentials from.
    #[serde(default)]
    pub credentials: CredentialEnv,
    /// Projection overrides scoped to this adapter.
    #[serde(default)]
    pub projection: ProjectionOverrides,
    /// Internal `TicketState` name -> external state name, consulted by
    /// [`crate::projection::ProjectionPolicy::map_state`].
    #[serde(default)]
    pub state_mapping: BTreeMap<String, String>,
}

fn default_enabled() -> bool {
    true
}

/// The parsed and validated contents of `mirror.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorConfig {
    /// Every configured adapter instance, keyed by its TOML table name.
    #[serde(default, rename = "adapters")]
    pub adapters: BTreeMap<String, AdapterConfig>,
}

impl MirrorConfig {
    /// Parse `mirror.toml` source text into a validated config.
    pub fn parse(source: &str) -> Result<Self> {
        let mut config = toml::from_str::<MirrorConfig>(source)
            .map_err(|e| tm_types::TmError::parse(e.to_string()))?;
        for (key, adapter) in config.adapters.iter_mut() {
            adapter.name = key.clone();
        }
        config.validate()?;
        Ok(config)
    }

    /// Structural validation beyond what serde already enforces (well-formed TOML, valid
    /// `AdapterKind`, valid credential-map shape). Checks cross-field invariants: an enabled,
    /// non-`Null` adapter must declare at least one credential variable, since a real adapter
    /// with no way to authenticate is a config mistake, not a valid "not yet configured" state
    /// (use `enabled = false` or `kind = "null"` for that instead).
    pub fn validate(&self) -> Result<()> {
        for (name, adapter) in &self.adapters {
            if adapter.enabled
                && adapter.kind != AdapterKind::Null
                && adapter.credentials.vars.is_empty()
            {
                return Err(tm_types::TmError::invariant(format!(
                    "mirror: adapter {name:?} ({:?}) is enabled with no credential variables configured",
                    adapter.kind
                )));
            }
        }
        Ok(())
    }

    /// Adapters that are attached and enabled, in name order (`BTreeMap` iteration order).
    pub fn enabled_adapters(&self) -> Vec<&AdapterConfig> {
        self.adapters.values().filter(|a| a.enabled).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_happy_path() {
        std::env::set_var("TEST_TOKEN", "secret123");
        std::env::set_var("TEST_API_KEY", "key456");

        let mut creds = CredentialEnv::default();
        creds
            .vars
            .insert("token".to_string(), "TEST_TOKEN".to_string());
        creds
            .vars
            .insert("api_key".to_string(), "TEST_API_KEY".to_string());

        let resolved = creds.resolve().expect("should resolve all vars");
        assert_eq!(resolved.get("token").unwrap(), "secret123");
        assert_eq!(resolved.get("api_key").unwrap(), "key456");

        std::env::remove_var("TEST_TOKEN");
        std::env::remove_var("TEST_API_KEY");
    }

    #[test]
    fn resolve_missing_env_var_fails_closed() {
        // Remove the var if it exists
        std::env::remove_var("NONEXISTENT_VAR_12345");

        let mut creds = CredentialEnv::default();
        creds
            .vars
            .insert("token".to_string(), "NONEXISTENT_VAR_12345".to_string());

        let result = creds.resolve();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("mirror:"));
        assert!(err_msg.contains("NONEXISTENT_VAR_12345"));
        assert!(err_msg.contains("token"));
    }

    #[test]
    fn parse_valid_config() {
        std::env::set_var("GH_TOKEN", "ghtoken123");

        let toml_str = r#"
[adapters.github_prod]
kind = "github"
enabled = true
credentials = { token = "GH_TOKEN" }

[adapters.linear_staging]
kind = "linear"
enabled = false
"#;

        let config = MirrorConfig::parse(toml_str).expect("should parse valid config");
        assert_eq!(config.adapters.len(), 2);

        let github = config.adapters.get("github_prod").unwrap();
        assert_eq!(github.name, "github_prod");
        assert_eq!(github.kind, AdapterKind::GitHub);
        assert!(github.enabled);
        assert_eq!(github.credentials.vars.get("token").unwrap(), "GH_TOKEN");

        let linear = config.adapters.get("linear_staging").unwrap();
        assert_eq!(linear.name, "linear_staging");
        assert_eq!(linear.kind, AdapterKind::Linear);
        assert!(!linear.enabled);

        std::env::remove_var("GH_TOKEN");
    }

    #[test]
    fn parse_invalid_toml() {
        let toml_str = "this is not valid toml {{{";

        let result = MirrorConfig::parse(toml_str);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse:"));
    }

    #[test]
    fn validate_enabled_adapter_without_credentials_fails() {
        let mut config = MirrorConfig::default();
        let adapter = AdapterConfig {
            name: "bad_adapter".to_string(),
            kind: AdapterKind::GitHub,
            enabled: true,
            credentials: CredentialEnv::default(),
            projection: ProjectionOverrides::default(),
            state_mapping: BTreeMap::new(),
        };
        config.adapters.insert("bad_adapter".to_string(), adapter);

        let result = config.validate();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("mirror:"));
        assert!(err_msg.contains("bad_adapter"));
        assert!(err_msg.contains("no credential variables configured"));
    }

    #[test]
    fn validate_disabled_adapter_without_credentials_succeeds() {
        let mut config = MirrorConfig::default();
        let adapter = AdapterConfig {
            name: "disabled_adapter".to_string(),
            kind: AdapterKind::GitHub,
            enabled: false,
            credentials: CredentialEnv::default(),
            projection: ProjectionOverrides::default(),
            state_mapping: BTreeMap::new(),
        };
        config
            .adapters
            .insert("disabled_adapter".to_string(), adapter);

        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn validate_null_adapter_without_credentials_succeeds() {
        let mut config = MirrorConfig::default();
        let adapter = AdapterConfig {
            name: "null_adapter".to_string(),
            kind: AdapterKind::Null,
            enabled: true,
            credentials: CredentialEnv::default(),
            projection: ProjectionOverrides::default(),
            state_mapping: BTreeMap::new(),
        };
        config.adapters.insert("null_adapter".to_string(), adapter);

        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn parse_with_projection_overrides() {
        let toml_str = r#"
[adapters.gh]
kind = "github"
enabled = true
credentials = { token = "GH_TOKEN" }
projection.milestone = "v1.0"
projection.extra_labels = ["external", "mirror"]
projection.force_checklist_rollup = true
"#;

        std::env::set_var("GH_TOKEN", "token");

        let config = MirrorConfig::parse(toml_str).expect("should parse with overrides");
        let adapter = config.adapters.get("gh").unwrap();

        assert_eq!(adapter.projection.milestone.as_ref().unwrap(), "v1.0");
        assert_eq!(adapter.projection.extra_labels, vec!["external", "mirror"]);
        assert_eq!(adapter.projection.force_checklist_rollup, Some(true));

        std::env::remove_var("GH_TOKEN");
    }

    #[test]
    fn parse_with_state_mapping() {
        let toml_str = r#"
[adapters.gh]
kind = "github"
enabled = true
credentials = { token = "GH_TOKEN" }

[adapters.gh.state_mapping]
"InProgress" = "in_progress"
"Done" = "closed"
"#;

        std::env::set_var("GH_TOKEN", "token");

        let config = MirrorConfig::parse(toml_str).expect("should parse with state mapping");
        let adapter = config.adapters.get("gh").unwrap();

        assert_eq!(
            adapter.state_mapping.get("InProgress").unwrap(),
            "in_progress"
        );
        assert_eq!(adapter.state_mapping.get("Done").unwrap(), "closed");

        std::env::remove_var("GH_TOKEN");
    }

    #[test]
    fn enabled_adapters_filters_correctly() {
        std::env::set_var("TOKEN", "t");

        let toml_str = r#"
[adapters.enabled1]
kind = "github"
enabled = true
credentials = { token = "TOKEN" }

[adapters.disabled1]
kind = "github"
enabled = false
credentials = { token = "TOKEN" }

[adapters.enabled2]
kind = "linear"
enabled = true
credentials = { token = "TOKEN" }
"#;

        let config = MirrorConfig::parse(toml_str).expect("should parse");
        let enabled = config.enabled_adapters();

        assert_eq!(enabled.len(), 2);
        let names: Vec<_> = enabled.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"enabled1"));
        assert!(names.contains(&"enabled2"));
        assert!(!names.contains(&"disabled1"));

        std::env::remove_var("TOKEN");
    }

    #[test]
    fn parse_empty_config() {
        let toml_str = "";

        let config = MirrorConfig::parse(toml_str).expect("should parse empty config");
        assert!(config.adapters.is_empty());
    }

    #[test]
    fn credential_env_empty_resolves_to_empty_map() {
        let creds = CredentialEnv::default();
        let resolved = creds.resolve().expect("should resolve empty creds");
        assert!(resolved.is_empty());
    }
}
