//! Parsing and validation of `providers.toml` into a [`RoleTable`].
//!
//! `providers.toml` is project state (versioned alongside the repo, not secret) that maps each
//! [`tm_types::Role`] to an ordered list of [`RoleCandidate`]s to try in turn. This module owns
//! turning that TOML into a validated, queryable table and back; it does no I/O itself — callers
//! read the file and hand this module the bytes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::str::FromStr;
use tm_types::{Role, Tolerance};

use crate::providers::compat::DevPassProvider;

/// Rate/volume limits attached to one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// Requests per minute, if the provider caps it.
    #[serde(default)]
    pub requests_per_minute: Option<u32>,
    /// Tokens per minute, if the provider caps it.
    #[serde(default)]
    pub tokens_per_minute: Option<u32>,
    /// Requests per day, if capped (e.g. a spend-limited tier).
    #[serde(default)]
    pub requests_per_day: Option<u32>,
    /// Requests per month, if capped.
    #[serde(default)]
    pub requests_per_month: Option<u32>,
}

impl Limits {
    /// No limits at all; used for the built-in default table's primary candidates.
    pub fn unlimited() -> Self {
        Limits {
            requests_per_minute: None,
            tokens_per_minute: None,
            requests_per_day: None,
            requests_per_month: None,
        }
    }
}

/// Price per token, in micro-dollars, matching [`tm_types::Budget`]'s `dollars_micros` unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    /// Micro-dollars per input token.
    pub input_micros_per_token: u64,
    /// Micro-dollars per output token.
    pub output_micros_per_token: u64,
}

/// One entry in a role's ordered candidate list: a concrete `(provider, model)` plus its
/// operating envelope for this role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleCandidate {
    /// The provider slug, matching a [`crate::fabric::Fabric`] registration, e.g. `"anthropic"`.
    pub provider: String,
    /// The provider's model identifier, e.g. `"claude-sonnet-5"`.
    pub model: String,
    /// Maximum simultaneous in-flight requests for this candidate.
    pub max_concurrency: u32,
    /// Whether routing to this candidate counts as a degrade (see [`crate::route::RouteDecision`]).
    #[serde(default)]
    pub degraded_ok: bool,
    /// Price per token, if known. `None` means cost is not tracked for this candidate.
    #[serde(default)]
    pub price: Option<Price>,
    /// Rate/volume limits for this candidate.
    #[serde(default = "Limits::unlimited")]
    pub limits: Limits,
}

/// Intermediate TOML structure for deserialization: `{ role: { candidates: [...] } }`.
#[derive(Deserialize, Serialize)]
struct IntermediateRole {
    /// The ordered list of candidates for this role.
    candidates: Vec<RoleCandidate>,
}

/// The full role -> ordered-candidates mapping, validated on construction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoleTable {
    roles: BTreeMap<String, Vec<RoleCandidate>>,
}

/// A `providers.toml` document failed to parse or validate.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RoleConfigError {
    /// The TOML itself was malformed.
    #[error("invalid TOML: {0}")]
    InvalidToml(String),

    /// A role name in the document is not a known [`tm_types::Role`].
    #[error("unknown role: {0}")]
    UnknownRole(String),

    /// A role has zero candidates.
    #[error("role {0} has no candidates")]
    EmptyRole(String),

    /// `max_concurrency` was zero, which would make the candidate permanently unroutable.
    #[error("role {role} candidate {provider}/{model} has max_concurrency 0")]
    ZeroConcurrency {
        /// The role the candidate belongs to.
        role: String,
        /// The candidate's provider slug.
        provider: String,
        /// The candidate's model id.
        model: String,
    },
}

impl RoleTable {
    /// Parse and validate a `providers.toml` document.
    ///
    /// Deserializes into an intermediate structure with role keys mapping to candidate lists,
    /// validates that all role names are known, and normalizes keys to canonical form for stable
    /// lookups regardless of input spelling (`coder_fast` vs `coder.fast`).
    pub fn parse(toml_str: &str) -> Result<Self, RoleConfigError> {
        // Role keys are dotted (e.g. `coder.fast`), which TOML syntax represents as nested
        // tables (`[coder.fast]` is `coder = { fast = {...} }`), not a single string key. Parse
        // generically and walk the tree, rejoining nested table paths with `.` until we hit a
        // table carrying a `candidates` array, which marks a role's leaf.
        let value: toml::Value =
            toml::from_str(toml_str).map_err(|e| RoleConfigError::InvalidToml(e.to_string()))?;
        let top = value
            .as_table()
            .ok_or_else(|| RoleConfigError::InvalidToml("expected a table at top level".into()))?;

        let mut flat = BTreeMap::new();
        Self::collect_roles(top, "", &mut flat)?;

        // Process and validate each role
        let mut roles = BTreeMap::new();
        for (role_key, candidates) in flat {
            // Parse and validate role name
            let role = Role::from_str(&role_key)
                .map_err(|_| RoleConfigError::UnknownRole(role_key.clone()))?;

            // Store under canonical role key for stable lookups
            roles.insert(role.as_str().to_string(), candidates);
        }

        let table = RoleTable { roles };
        table.validate()?;
        Ok(table)
    }

    /// Recursively walk a TOML table, rejoining dotted-table paths, and collect each leaf table
    /// (one carrying a `candidates` array) into `out` keyed by its full dotted path.
    fn collect_roles(
        table: &toml::value::Table,
        prefix: &str,
        out: &mut BTreeMap<String, Vec<RoleCandidate>>,
    ) -> Result<(), RoleConfigError> {
        for (key, val) in table {
            let full_key = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };

            let sub_table = val.as_table().ok_or_else(|| {
                RoleConfigError::InvalidToml(format!("expected a table at `{full_key}`"))
            })?;

            if sub_table.contains_key("candidates") {
                let role_config: IntermediateRole = val.clone().try_into().map_err(|e| {
                    RoleConfigError::InvalidToml(format!("invalid role `{full_key}`: {e}"))
                })?;
                out.insert(full_key, role_config.candidates);
            } else {
                Self::collect_roles(sub_table, &full_key, out)?;
            }
        }
        Ok(())
    }

    /// Re-serialize to canonical TOML, e.g. after `default_table()` to seed a new project.
    ///
    /// Produces a pretty-printed TOML representation with error handling for serialization failures.
    pub fn to_toml_string(&self) -> Result<String, RoleConfigError> {
        // Convert to intermediate structure for serialization
        let intermediate: BTreeMap<String, IntermediateRole> = self
            .roles
            .iter()
            .map(|(key, candidates)| {
                (
                    key.clone(),
                    IntermediateRole {
                        candidates: candidates.clone(),
                    },
                )
            })
            .collect();

        toml::to_string_pretty(&intermediate)
            .map_err(|e| RoleConfigError::InvalidToml(e.to_string()))
    }

    /// Validate structural invariants: every role known, every role non-empty, every candidate's
    /// `max_concurrency` nonzero.
    ///
    /// Returns the first violation found in deterministic BTreeMap order.
    pub fn validate(&self) -> Result<(), RoleConfigError> {
        for (role_key, candidates) in &self.roles {
            // Defensive check that role is known (should already be validated by parse)
            let _role = Role::from_str(role_key)
                .map_err(|_| RoleConfigError::UnknownRole(role_key.clone()))?;

            // Check that role has at least one candidate
            if candidates.is_empty() {
                return Err(RoleConfigError::EmptyRole(role_key.clone()));
            }

            // Check that all candidates have nonzero max_concurrency
            for candidate in candidates {
                if candidate.max_concurrency == 0 {
                    return Err(RoleConfigError::ZeroConcurrency {
                        role: role_key.clone(),
                        provider: candidate.provider.clone(),
                        model: candidate.model.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// The ordered candidate list for `role`, empty if the role is absent from the table.
    pub fn candidates_for(&self, role: Role) -> &[RoleCandidate] {
        self.roles
            .get(role.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Put `provider`/`model` first in `role`'s candidate list, so routing tries it before
    /// anything else (`/model` in a session). An existing entry for the same pair moves to the
    /// front; a new one takes the current primary's concurrency, but not its price or limits,
    /// which belong to the other model. The rest of the list stays behind it, in order, as the
    /// fallback path.
    pub fn prefer(&mut self, role: Role, provider: &str, model: &str) {
        let candidates = self.roles.entry(role.as_str().to_string()).or_default();
        let existing = candidates
            .iter()
            .position(|c| c.provider == provider && c.model == model);
        let preferred = match existing {
            Some(i) => candidates.remove(i),
            None => RoleCandidate {
                provider: provider.to_string(),
                model: model.to_string(),
                max_concurrency: candidates.first().map_or(10, |c| c.max_concurrency),
                degraded_ok: false,
                price: None,
                limits: Limits::unlimited(),
            },
        };
        candidates.insert(
            0,
            RoleCandidate {
                degraded_ok: false,
                ..preferred
            },
        );
    }

    /// The built-in default table, used when no `providers.toml` exists yet.
    ///
    /// Provides one primary Anthropic candidate per role (frontier roles use Opus, cheap/fast
    /// roles use Haiku, standard roles use Sonnet). For non-Strict-by-default roles, adds a
    /// fallback candidate with `degraded_ok = true`. Embedder gets a single embedding-model candidate.
    /// All candidates use unlimited rate/volume limits and no pricing (pricing is configured
    /// separately in a real `providers.toml`).
    ///
    /// **DevPass default preference:** when [`DevPassProvider::preferred_model`] returns
    /// `Some(model)` (all three `DEVPASS_*` env vars set and non-empty), [`Role::CoderFast`]'s
    /// primary candidate is `devpass`/`model` instead of `anthropic`/`claude-sonnet-5` —
    /// `CoderFast` is the one role `tm-cli`'s interactive/scriptable session and its scheduler
    /// dispatcher actually drive an agent-loop turn as (see `crates/tm-cli/src/agent.rs`'s
    /// `AGENT_ROLE`), so this is the one role where "run `tm` for real without burning Anthropic
    /// quota" bites. Every other role is untouched by this check, absent or present. See
    /// `docs/providers.md`'s "DevPass" section and `docs/decisions/D-005-devpass-default-provider.md`.
    ///
    /// This is a thin env-reading wrapper around [`Self::default_table_with`], which is pure and
    /// does the actual construction — kept separate so tests can exercise both the
    /// DevPass-preferred and DevPass-absent shapes deterministically without touching real
    /// process env vars (this function has roughly twenty call sites across the workspace,
    /// several themselves in tests, so a test that mutated real env here could contaminate an
    /// unrelated concurrent test's call to this same function).
    pub fn default_table() -> Self {
        Self::default_table_with(DevPassProvider::preferred_model().as_deref())
    }

    /// Pure core of [`Self::default_table`], parameterized on whether DevPass should be
    /// preferred as [`Role::CoderFast`]'s primary candidate (`Some(model)`) or not (`None`).
    fn default_table_with(devpass_model: Option<&str>) -> Self {
        let mut roles = BTreeMap::new();

        for role in Role::ALL {
            let is_frontier = role.is_frontier();
            let tolerance = role.default_tolerance();

            let candidates = match role {
                Role::Embedder => {
                    // Embedding model: single candidate, no fallback
                    vec![RoleCandidate {
                        provider: "anthropic".to_string(),
                        model: "claude-embed-v1".to_string(),
                        max_concurrency: 100,
                        degraded_ok: false,
                        price: None,
                        limits: Limits::unlimited(),
                    }]
                }
                Role::ExplorerCheap => {
                    // Cheap exploration: primary on Haiku
                    let primary = RoleCandidate {
                        provider: "anthropic".to_string(),
                        model: "claude-haiku-4-5".to_string(),
                        max_concurrency: 50,
                        degraded_ok: false,
                        price: None,
                        limits: Limits::unlimited(),
                    };

                    if tolerance == Tolerance::Strict {
                        vec![primary]
                    } else {
                        // Fallback on same model with lower concurrency (cache-friendly)
                        vec![
                            primary,
                            RoleCandidate {
                                provider: "anthropic".to_string(),
                                model: "claude-haiku-4-5".to_string(),
                                max_concurrency: 25,
                                degraded_ok: true,
                                price: None,
                                limits: Limits::unlimited(),
                            },
                        ]
                    }
                }
                Role::SummarizerCheap => {
                    // Cheap summarization: primary on Haiku
                    let primary = RoleCandidate {
                        provider: "anthropic".to_string(),
                        model: "claude-haiku-4-5".to_string(),
                        max_concurrency: 100,
                        degraded_ok: false,
                        price: None,
                        limits: Limits::unlimited(),
                    };

                    // No fallback candidate: same list regardless of tolerance.
                    vec![primary]
                }
                Role::CoderFast => {
                    // Fast coding: primary on Sonnet, fallback on Haiku — unless DevPass is
                    // configured as the default (see `default_table`'s doc comment), in which
                    // case the primary becomes DevPass; the Anthropic fallback is left in place
                    // untouched, so a project that also happens to have `ANTHROPIC_API_KEY` set
                    // still gets a degrade path if DevPass becomes unavailable.
                    let mut primary = RoleCandidate {
                        provider: "anthropic".to_string(),
                        model: "claude-sonnet-5".to_string(),
                        max_concurrency: 20,
                        degraded_ok: false,
                        price: None,
                        limits: Limits::unlimited(),
                    };
                    if let Some(model) = devpass_model {
                        primary.provider = "devpass".to_string();
                        primary.model = model.to_string();
                    }

                    if tolerance == Tolerance::Strict {
                        vec![primary]
                    } else {
                        vec![
                            primary,
                            RoleCandidate {
                                provider: "anthropic".to_string(),
                                model: "claude-haiku-4-5".to_string(),
                                max_concurrency: 50,
                                degraded_ok: true,
                                price: None,
                                limits: Limits::unlimited(),
                            },
                        ]
                    }
                }
                _ if is_frontier => {
                    // Frontier roles: strong Opus primary + Sonnet fallback if not strict
                    let primary = RoleCandidate {
                        provider: "anthropic".to_string(),
                        model: "claude-opus-5-5".to_string(),
                        max_concurrency: 10,
                        degraded_ok: false,
                        price: None,
                        limits: Limits::unlimited(),
                    };

                    if tolerance == Tolerance::Strict {
                        vec![primary]
                    } else {
                        vec![
                            primary,
                            RoleCandidate {
                                provider: "anthropic".to_string(),
                                model: "claude-sonnet-5".to_string(),
                                max_concurrency: 20,
                                degraded_ok: true,
                                price: None,
                                limits: Limits::unlimited(),
                            },
                        ]
                    }
                }
                _ => {
                    // Standard roles: Sonnet primary + Haiku fallback if not strict
                    let primary = RoleCandidate {
                        provider: "anthropic".to_string(),
                        model: "claude-sonnet-5".to_string(),
                        max_concurrency: 20,
                        degraded_ok: false,
                        price: None,
                        limits: Limits::unlimited(),
                    };

                    if tolerance == Tolerance::Strict {
                        vec![primary]
                    } else {
                        vec![
                            primary,
                            RoleCandidate {
                                provider: "anthropic".to_string(),
                                model: "claude-haiku-4-5".to_string(),
                                max_concurrency: 50,
                                degraded_ok: true,
                                price: None,
                                limits: Limits::unlimited(),
                            },
                        ]
                    }
                }
            };

            roles.insert(role.as_str().to_string(), candidates);
        }

        RoleTable { roles }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_table_round_trips_through_toml() {
        let table = RoleTable::default_table();
        let s = table.to_toml_string().expect("default table serializes");
        let parsed = RoleTable::parse(&s).expect("default table's own TOML reparses");
        assert_eq!(table, parsed);
    }

    #[test]
    fn prefer_moves_a_known_candidate_first_and_adds_an_unknown_one() {
        let mut table = RoleTable::default_table_with(None);
        let before: Vec<_> = table
            .candidates_for(Role::CoderFast)
            .iter()
            .map(|c| c.model.clone())
            .collect();
        assert_eq!(before, ["claude-sonnet-5", "claude-haiku-4-5"]);

        table.prefer(Role::CoderFast, "anthropic", "claude-haiku-4-5");
        let after = table.candidates_for(Role::CoderFast);
        assert_eq!(after.len(), 2, "moved, not duplicated");
        assert_eq!(after[0].model, "claude-haiku-4-5");
        assert!(!after[0].degraded_ok, "the chosen model is not a degrade");
        assert_eq!(after[1].model, "claude-sonnet-5");

        table.prefer(Role::CoderFast, "devpass", "muse");
        let after = table.candidates_for(Role::CoderFast);
        assert_eq!(after.len(), 3);
        assert_eq!(
            (after[0].provider.as_str(), after[0].model.as_str()),
            ("devpass", "muse")
        );
        assert_eq!(
            after[0].max_concurrency, 50,
            "takes the old primary's concurrency"
        );
        assert!(after[0].price.is_none());
        table.validate().expect("still a valid table");
    }

    #[test]
    fn default_table_covers_every_role() {
        let table = RoleTable::default_table();
        for role in Role::ALL {
            assert!(
                !table.candidates_for(role).is_empty(),
                "{role} has no candidates"
            );
        }
    }

    #[test]
    fn unknown_role_is_rejected() {
        let err = RoleTable::parse("[not_a_role]\ncandidates = []\n").unwrap_err();
        assert!(matches!(err, RoleConfigError::UnknownRole(_)));
    }

    #[test]
    fn parse_valid_single_role() {
        let toml = r#"
[coder.fast]
candidates = [
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 10 }
]
"#;
        let table = RoleTable::parse(toml).expect("valid TOML parses");
        let role = Role::CoderFast;
        let candidates = table.candidates_for(role);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider, "anthropic");
        assert_eq!(candidates[0].model, "claude-sonnet-5");
        assert_eq!(candidates[0].max_concurrency, 10);
    }

    #[test]
    fn parse_multiple_candidates_for_one_role() {
        let toml = r#"
[coder.fast]
candidates = [
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 10 },
  { provider = "anthropic", model = "claude-haiku-4-5", max_concurrency = 20, degraded_ok = true }
]
"#;
        let table = RoleTable::parse(toml).expect("valid TOML with multiple candidates");
        let candidates = table.candidates_for(Role::CoderFast);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].max_concurrency, 10);
        assert!(!candidates[0].degraded_ok);
        assert_eq!(candidates[1].max_concurrency, 20);
        assert!(candidates[1].degraded_ok);
    }

    #[test]
    fn empty_role_candidates_rejected() {
        let toml = r#"
[coder.fast]
candidates = []
"#;
        let err = RoleTable::parse(toml).unwrap_err();
        assert!(matches!(err, RoleConfigError::EmptyRole(_)));
    }

    #[test]
    fn zero_max_concurrency_rejected() {
        let toml = r#"
[coder.fast]
candidates = [
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 0 }
]
"#;
        let err = RoleTable::parse(toml).unwrap_err();
        assert!(matches!(err, RoleConfigError::ZeroConcurrency { .. }));
    }

    #[test]
    fn malformed_toml_rejected() {
        let toml = "[invalid toml";
        let err = RoleTable::parse(toml).unwrap_err();
        assert!(matches!(err, RoleConfigError::InvalidToml(_)));
    }

    #[test]
    fn candidates_for_absent_role_returns_empty() {
        let table = RoleTable::default_table();
        // All default roles should have candidates, but absent role should return empty
        let mut roles_in_table = std::collections::HashSet::new();
        for role in Role::ALL {
            if !table.candidates_for(role).is_empty() {
                roles_in_table.insert(role);
            }
        }
        // Default table should cover all roles
        assert_eq!(roles_in_table.len(), Role::ALL.len());
    }

    #[test]
    fn role_key_normalization_to_canonical_form() {
        let toml = r#"
[coder.fast]
candidates = [
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 10 }
]
"#;
        let table = RoleTable::parse(toml).expect("valid TOML");
        // Round-trip should produce normalized keys
        let serialized = table.to_toml_string().expect("serializes");
        let reparsed = RoleTable::parse(&serialized).expect("reparses");
        assert_eq!(table, reparsed);
    }

    #[test]
    fn parse_multiple_roles() {
        let toml = r#"
[coder.fast]
candidates = [
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 10 }
]

[vision.frontier]
candidates = [
  { provider = "anthropic", model = "claude-opus-5-5", max_concurrency = 5 }
]
"#;
        let table = RoleTable::parse(toml).expect("valid TOML with multiple roles");
        assert_eq!(table.candidates_for(Role::CoderFast).len(), 1);
        assert_eq!(table.candidates_for(Role::VisionFrontier).len(), 1);
    }

    #[test]
    fn serialization_includes_all_fields() {
        let toml = r#"
[coder.fast]
candidates = [
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 10, degraded_ok = true, price = { input_micros_per_token = 1, output_micros_per_token = 2 } }
]
"#;
        let table = RoleTable::parse(toml).expect("valid TOML");
        let serialized = table.to_toml_string().expect("serializes");
        let reparsed = RoleTable::parse(&serialized).expect("reparses");
        assert_eq!(table, reparsed);
        assert!(reparsed.candidates_for(Role::CoderFast)[0].price.is_some());
    }

    #[test]
    fn default_table_every_role_has_at_least_one_candidate() {
        let table = RoleTable::default_table();
        for role in Role::ALL {
            let candidates = table.candidates_for(role);
            assert!(
                !candidates.is_empty(),
                "role {} has no candidates",
                role.as_str()
            );
            // All candidates must have nonzero max_concurrency
            for candidate in candidates {
                assert!(
                    candidate.max_concurrency > 0,
                    "role {} candidate has zero max_concurrency",
                    role.as_str()
                );
            }
        }
    }

    #[test]
    fn default_table_primary_candidates_not_degraded() {
        let table = RoleTable::default_table();
        for role in Role::ALL {
            let candidates = table.candidates_for(role);
            // First candidate should never be marked as degraded
            assert!(
                !candidates[0].degraded_ok,
                "role {} primary candidate marked degraded",
                role.as_str()
            );
        }
    }

    #[test]
    fn candidates_for_queries_by_role() {
        let table = RoleTable::default_table();
        let coder_fast = table.candidates_for(Role::CoderFast);
        assert!(!coder_fast.is_empty());
        let vision = table.candidates_for(Role::VisionFrontier);
        assert!(!vision.is_empty());
        // Verify they're different
        if coder_fast.len() == 1 && vision.len() == 1 {
            // If each has only one candidate, they should differ
            assert_ne!(coder_fast[0].model, vision[0].model);
        }
    }

    // ---- default_table_with: DevPass default preference (pure, no env vars touched) ----
    //
    // `default_table_with` takes the DevPass-preferred model as a plain `Option<&str>` argument
    // rather than reading `DEVPASS_*` env vars itself, specifically so these tests can assert
    // both branches deterministically without mutating real process env state — `default_table`
    // (the public, env-reading wrapper) has roughly twenty call sites across this workspace,
    // several themselves in `#[cfg(test)]` code in this same crate, and any test here that set
    // real `DEVPASS_*` vars could race one of those and make it non-deterministic. See
    // `DevPassProvider::preferred_model`'s own tests (`providers/compat.rs`) for the one place
    // this crate actually exercises the env-reading half.

    #[test]
    fn default_table_with_none_matches_the_unchanged_anthropic_only_default() {
        let table = RoleTable::default_table_with(None);

        // The specific regression this guards: CoderFast's primary must still be exactly what
        // it was before DevPass preference existed.
        let coder_fast = table.candidates_for(Role::CoderFast);
        assert_eq!(coder_fast[0].provider, "anthropic");
        assert_eq!(coder_fast[0].model, "claude-sonnet-5");
        assert_eq!(coder_fast[0].max_concurrency, 20);
        assert!(!coder_fast[0].degraded_ok);
        assert_eq!(coder_fast[1].provider, "anthropic");
        assert_eq!(coder_fast[1].model, "claude-haiku-4-5");
        assert!(coder_fast[1].degraded_ok);

        // Broader invariant: with no DevPass model, *every* candidate of *every* role is still
        // Anthropic — nothing else about `default_table` moved.
        for role in Role::ALL {
            for candidate in table.candidates_for(role) {
                assert_eq!(
                    candidate.provider, "anthropic",
                    "role {role} has a non-anthropic candidate with DevPass unconfigured"
                );
            }
        }
    }

    #[test]
    fn default_table_with_devpass_model_prefers_devpass_for_coder_fast_primary_only() {
        let table = RoleTable::default_table_with(Some("devpass-test-model"));

        let coder_fast = table.candidates_for(Role::CoderFast);
        assert_eq!(coder_fast[0].provider, "devpass");
        assert_eq!(coder_fast[0].model, "devpass-test-model");
        assert_eq!(coder_fast[0].max_concurrency, 20);
        assert!(!coder_fast[0].degraded_ok);
        assert_eq!(coder_fast[0].price, None);

        // The Anthropic fallback is left in place, untouched, so a project that also has
        // `ANTHROPIC_API_KEY` set still has a degrade path if DevPass becomes unavailable.
        assert_eq!(coder_fast[1].provider, "anthropic");
        assert_eq!(coder_fast[1].model, "claude-haiku-4-5");
        assert!(coder_fast[1].degraded_ok);

        // Every other role is completely unaffected: still all-Anthropic.
        for role in Role::ALL {
            if role == Role::CoderFast {
                continue;
            }
            for candidate in table.candidates_for(role) {
                assert_eq!(
                    candidate.provider, "anthropic",
                    "role {role} unexpectedly changed when only CoderFast should prefer DevPass"
                );
            }
        }
    }

    #[test]
    fn default_table_with_devpass_model_still_validates() {
        // The devpass-preferred table must still satisfy `RoleTable::validate`'s invariants
        // (every role non-empty, every candidate's max_concurrency nonzero) — nothing about
        // swapping the primary's provider/model should be able to produce an invalid table.
        let table = RoleTable::default_table_with(Some("devpass-test-model"));
        table
            .validate()
            .expect("devpass-preferred default table is still structurally valid");
    }
}
