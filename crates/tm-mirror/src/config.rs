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
        // IMPL: for (field, var_name) in &self.vars (BTreeMap iteration is already
        // deterministically ordered, so error messages are stable across runs), call
        // std::env::var(var_name); on the first Err, return
        // TmError::invariant(format!("mirror: env var {var_name} (credential field {field:?}) is
        // not set")). Reading the process environment here is fine under the crate's
        // determinism rule — that rule targets wall-clock/random sources, not a fixed process
        // environment — but must never be memoized across calls, since tests set/unset env vars
        // between cases.
        todo!("resolve every named credential env var, failing closed on the first missing one")
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
        // IMPL: toml::from_str::<MirrorConfig>(source).map_err(|e| TmError::parse(e.to_string()))?;
        // then, since serde can't populate a BTreeMap key into a field, iterate
        // `config.adapters` and set `adapter.name = key.clone()` for each; then call
        // `config.validate()?` and return `config`.
        todo!("parse mirror.toml, stamp adapter names from their table keys, and validate")
    }

    /// Structural validation beyond what serde already enforces (well-formed TOML, valid
    /// `AdapterKind`, valid credential-map shape). Checks cross-field invariants: an enabled,
    /// non-`Null` adapter must declare at least one credential variable, since a real adapter
    /// with no way to authenticate is a config mistake, not a valid "not yet configured" state
    /// (use `enabled = false` or `kind = "null"` for that instead).
    pub fn validate(&self) -> Result<()> {
        // IMPL: for (name, adapter) in &self.adapters, if adapter.enabled && adapter.kind !=
        // AdapterKind::Null && adapter.credentials.vars.is_empty(), return
        // TmError::invariant(format!("mirror: adapter {name:?} ({:?}) is enabled with no
        // credential variables configured", adapter.kind)). Extend with further cross-field
        // rules as they arise; this function is the single place they should live so
        // `MirrorConfig::parse` only has to call one thing.
        todo!("validate cross-field invariants across configured adapters")
    }

    /// Adapters that are attached and enabled, in name order (`BTreeMap` iteration order).
    pub fn enabled_adapters(&self) -> Vec<&AdapterConfig> {
        self.adapters.values().filter(|a| a.enabled).collect()
    }
}
