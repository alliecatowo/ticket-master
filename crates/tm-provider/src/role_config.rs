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

/// Price per 1,000,000 tokens, in micro-dollars (matching [`tm_types::Budget`]'s
/// `dollars_micros` unit) -- **not** per token (`critic-real-prices-and-price-unit`). A
/// per-token micro-dollar unit floor-divides any sub-$1/M-token rate to 0 once multiplied by a
/// single token's usage (e.g. $0.20/M = 0.2 micro-dollars/token truncates to 0 in an integer
/// micros-per-token field), which silently zeroes `tel-completion-cost-field`, `tm stats`
/// dollars, and budget tier-down for exactly the cheap models this project actually wants
/// costed. Per-million gives enough fixed-point precision for realistic model prices without
/// needing a float. [`crate::fabric::cost_micros`] divides back down by 1,000,000 against real
/// token counts.
///
/// Deserializes via [`PriceToml`], which also accepts the old (pre-`critic-real-prices-and-
/// price-unit`) `input_micros_per_token`/`output_micros_per_token` field names, scaled up by
/// 1,000,000 — so a `providers.toml`/test fixture written against the old unit still parses to
/// the same real price instead of erroring or silently misreading; see [`PriceToml`]'s own doc
/// comment. Serialization (round-tripping a parsed table back to TOML) always emits only the new,
/// per-million field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PriceToml")]
pub struct Price {
    /// Micro-dollars per 1,000,000 input tokens.
    pub input_micros_per_million_tokens: u64,
    /// Micro-dollars per 1,000,000 output tokens.
    pub output_micros_per_million_tokens: u64,
}

/// The on-the-wire shape of a `price` table: either the current per-million field names, or the
/// legacy per-token names this project used before `critic-real-prices-and-price-unit` (accepted
/// so an old `providers.toml`/fixture keeps parsing, scaled up by 1,000,000 to the same real
/// price rather than being silently reinterpreted at the wrong magnitude). Mixing the two families
/// — or omitting both — is a [`RoleConfigError::InvalidToml`], not a silent default, since a
/// partial price is worse than none.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PriceToml {
    #[serde(default)]
    input_micros_per_million_tokens: Option<u64>,
    #[serde(default)]
    output_micros_per_million_tokens: Option<u64>,
    #[serde(default)]
    input_micros_per_token: Option<u64>,
    #[serde(default)]
    output_micros_per_token: Option<u64>,
}

impl TryFrom<PriceToml> for Price {
    type Error = String;

    fn try_from(t: PriceToml) -> Result<Self, Self::Error> {
        match (
            t.input_micros_per_million_tokens,
            t.output_micros_per_million_tokens,
            t.input_micros_per_token,
            t.output_micros_per_token,
        ) {
            (Some(input), Some(output), None, None) => Ok(Price {
                input_micros_per_million_tokens: input,
                output_micros_per_million_tokens: output,
            }),
            (None, None, Some(input), Some(output)) => Ok(Price {
                input_micros_per_million_tokens: input.saturating_mul(1_000_000),
                output_micros_per_million_tokens: output.saturating_mul(1_000_000),
            }),
            _ => Err(
                "price must set exactly one of (input_micros_per_million_tokens, \
                 output_micros_per_million_tokens) or the legacy \
                 (input_micros_per_token, output_micros_per_token), never a mix of both \
                 families or only one field"
                    .to_string(),
            ),
        }
    }
}

/// `claude-opus-5-5`'s published per-1,000,000-token rate ($4 input / $20 output), used to seed
/// [`RoleTable::default_table_with`] so a project that never hand-edits `providers.toml` still
/// gets real dollar figures out of `tel-completion-cost-field`/`tm stats`/budget tier-down. A
/// real `providers.toml` can override it per candidate.
const OPUS_5_5_PRICE: Price = Price {
    input_micros_per_million_tokens: 4_000_000,
    output_micros_per_million_tokens: 20_000_000,
};

/// `claude-sonnet-5`'s published per-1,000,000-token rate ($2 input / $10 output). See
/// [`OPUS_5_5_PRICE`]'s doc comment.
const SONNET_5_PRICE: Price = Price {
    input_micros_per_million_tokens: 2_000_000,
    output_micros_per_million_tokens: 10_000_000,
};

/// `claude-haiku-4-5`'s published per-1,000,000-token rate ($1 input / $5 output). See
/// [`OPUS_5_5_PRICE`]'s doc comment.
const HAIKU_4_5_PRICE: Price = Price {
    input_micros_per_million_tokens: 1_000_000,
    output_micros_per_million_tokens: 5_000_000,
};

/// One entry in a role's ordered candidate list: a concrete `(provider, model)` plus its
/// operating envelope for this role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Price per 1,000,000 tokens, if known. `None` means cost is not tracked for this
    /// candidate.
    #[serde(default)]
    pub price: Option<Price>,
    /// Rate/volume limits for this candidate.
    #[serde(default = "Limits::unlimited")]
    pub limits: Limits,
}

/// The on-the-wire shape of one candidate row in `providers.toml`: everything
/// [`RoleCandidate`] has, plus two fields that only mean anything on the `decider` role
/// (D-020) — `base_url`/`token_env`, for selecting and pointing a
/// [`crate::decide::DecisionProvider`] backend (`"mock"` or `"systemone"`/`"systemone-http"`) per
/// [`crate::providers::registry::Registry::build_decider`]'s `(slug, model, token, base_url)`
/// shape. Kept as a superset of `RoleCandidate` (not a `#[serde(flatten)]` wrapper around it,
/// which cannot combine with `#[serde(deny_unknown_fields)]`) so a typo in either a decider or a
/// non-decider candidate still surfaces as [`RoleConfigError::InvalidToml`] instead of silently
/// taking a default. [`RoleTable::parse`] rejects `base_url`/`token_env` set on any role other
/// than `decider`, and splits a valid decider row's two fields off into [`RoleTable`]'s
/// `decider_meta` side table rather than carrying them on [`RoleCandidate`] itself —
/// `RoleCandidate` is constructed by struct literal in several other crates, and adding fields to
/// it would be a breaking change to all of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateToml {
    /// The provider slug (`RoleCandidate::provider`), or for `decider` a
    /// [`crate::providers::registry::Registry::build_decider`] slug (`"mock"`,
    /// `"systemone"`/`"systemone-http"`).
    provider: String,
    /// The model id (`RoleCandidate::model`).
    model: String,
    /// `RoleCandidate::max_concurrency`.
    max_concurrency: u32,
    /// `RoleCandidate::degraded_ok`.
    #[serde(default)]
    degraded_ok: bool,
    /// `RoleCandidate::price`.
    #[serde(default)]
    price: Option<Price>,
    /// `RoleCandidate::limits`.
    #[serde(default = "Limits::unlimited")]
    limits: Limits,
    /// `decider`-only: overrides the [`crate::providers::systemone::SystemOneProvider`]'s
    /// default endpoint (e.g. a local jevmlx endpoint) instead of `SYSTEMONE_BASE_URL`/its
    /// built-in default. Rejected on any other role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// `decider`-only: the *name* of an environment variable to read the bearer token from at
    /// dispatch time — never the token itself. `providers.toml` is project state, versioned
    /// alongside the repo, not secret (see the module doc); a real credential never belongs in
    /// it. Rejected on any other role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_env: Option<String>,
}

/// Intermediate TOML structure for deserialization: `{ role: { candidates: [...] } }`.
#[derive(Deserialize, Serialize)]
struct IntermediateRole {
    /// The ordered list of candidates for this role.
    candidates: Vec<CandidateToml>,
}

/// Endpoint override for one `Role::Decider` `(provider, model)` candidate, carried in
/// [`RoleTable`]'s `decider_meta` side table rather than on [`RoleCandidate`] (see
/// [`CandidateToml`]'s doc comment for why). `None` in either field means
/// [`crate::providers::registry::Registry::build_decider`]'s own default applies (the
/// `systemone` provider's `SYSTEMONE_BASE_URL`/`AI_GATEWAY_API_KEY`/`TYPESAFE_API_KEY` env
/// vars) — this type carries no default resolution logic itself, only what the config said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeciderEndpoint {
    /// A config-supplied base URL override, if any.
    pub base_url: Option<String>,
    /// The name of the environment variable holding the bearer token, if any.
    pub token_env: Option<String>,
}

/// The full role -> ordered-candidates mapping, validated on construction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoleTable {
    roles: BTreeMap<String, Vec<RoleCandidate>>,
    /// `decider`-only endpoint overrides, keyed by `(provider, model)`. Never touched by
    /// anything but the `decider` role — see [`CandidateToml`]'s doc comment. `#[serde(skip)]`
    /// keeps [`RoleTable`]'s derived `Serialize`/`Deserialize` `#[serde(transparent)]` (both are
    /// dead code paths in practice — [`RoleTable::parse`]/[`RoleTable::to_toml_string`] do the
    /// real (de)serialization by hand — but transparent still requires every non-primary field to
    /// be skipped and `Default`, which `BTreeMap` is).
    #[serde(skip)]
    decider_meta: BTreeMap<(String, String), DeciderEndpoint>,
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
        let mut decider_meta = BTreeMap::new();
        for (role_key, candidates) in flat {
            // Parse and validate role name
            let role = Role::from_str(&role_key)
                .map_err(|_| RoleConfigError::UnknownRole(role_key.clone()))?;

            // Split each wire candidate into a plain `RoleCandidate` (so `EmptyRole`/
            // `ZeroConcurrency` cover every role, `decider` included, for free) plus, for
            // `decider` only, a `decider_meta` entry for any `base_url`/`token_env` override.
            // Both fields are rejected outright on any other role — they'd otherwise be silently
            // ignored, which is exactly what `#[serde(deny_unknown_fields)]` elsewhere in this
            // file exists to prevent.
            let mut role_candidates = Vec::with_capacity(candidates.len());
            for c in candidates {
                if role != Role::Decider && (c.base_url.is_some() || c.token_env.is_some()) {
                    return Err(RoleConfigError::InvalidToml(format!(
                        "`base_url`/`token_env` are only valid on the decider role (found on `{role_key}`)"
                    )));
                }
                if c.base_url.is_some() || c.token_env.is_some() {
                    decider_meta.insert(
                        (c.provider.clone(), c.model.clone()),
                        DeciderEndpoint {
                            base_url: c.base_url.clone(),
                            token_env: c.token_env.clone(),
                        },
                    );
                }
                role_candidates.push(RoleCandidate {
                    provider: c.provider,
                    model: c.model,
                    max_concurrency: c.max_concurrency,
                    degraded_ok: c.degraded_ok,
                    price: c.price,
                    limits: c.limits,
                });
            }

            // Store under canonical role key for stable lookups
            roles.insert(role.as_str().to_string(), role_candidates);
        }

        let table = RoleTable {
            roles,
            decider_meta,
        };
        table.validate()?;
        Ok(table)
    }

    /// Recursively walk a TOML table, rejoining dotted-table paths, and collect each leaf table
    /// (one carrying a `candidates` array) into `out` keyed by its full dotted path.
    fn collect_roles(
        table: &toml::value::Table,
        prefix: &str,
        out: &mut BTreeMap<String, Vec<CandidateToml>>,
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
        // Convert to intermediate structure for serialization, re-attaching each decider
        // candidate's `base_url`/`token_env` from `decider_meta` (every other role's candidates
        // get `None`/`None`, which `#[serde(skip_serializing_if)]` on `CandidateToml` omits from
        // the output entirely, so a table with no decider overrides round-trips byte-identical
        // to before this field existed).
        let intermediate: BTreeMap<String, IntermediateRole> = self
            .roles
            .iter()
            .map(|(key, candidates)| {
                // Only the `decider` role ever has `decider_meta` entries, but `(provider,
                // model)` is not unique *across* roles — e.g. a `mock`/`m1` candidate on
                // `coder_fast` in a test fixture must never pick up a same-named `decider`
                // candidate's endpoint override, or this round-trips into an `InvalidToml`
                // rejection on reparse (`base_url`/`token_env` only valid on `decider`).
                let is_decider = key == Role::Decider.as_str();
                let wire_candidates = candidates
                    .iter()
                    .map(|c| {
                        let endpoint = if is_decider {
                            self.decider_endpoint(&c.provider, &c.model)
                        } else {
                            DeciderEndpoint::default()
                        };
                        CandidateToml {
                            provider: c.provider.clone(),
                            model: c.model.clone(),
                            max_concurrency: c.max_concurrency,
                            degraded_ok: c.degraded_ok,
                            price: c.price,
                            limits: c.limits,
                            base_url: endpoint.base_url,
                            token_env: endpoint.token_env,
                        }
                    })
                    .collect();
                (
                    key.clone(),
                    IntermediateRole {
                        candidates: wire_candidates,
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

    /// The config-supplied endpoint override for a `(provider, model)` decider candidate, if
    /// `providers.toml` set `base_url`/`token_env` for it — `Default::default()` (both `None`)
    /// for any candidate the config didn't set either field on, decider or not. This is the
    /// `(token, base_url)` half of [`crate::providers::registry::Registry::build_decider`]'s
    /// `(slug, model, token, base_url)` call shape; the caller still resolves `token_env` to an
    /// actual token by reading the named environment variable — this module does no I/O (see the
    /// module doc).
    pub fn decider_endpoint(&self, provider: &str, model: &str) -> DeciderEndpoint {
        self.decider_meta
            .get(&(provider.to_string(), model.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    /// Append `candidate` to the back of `role`'s list, unless the role already names the
    /// same provider (under any model) — the one-provider-per-slug rule in
    /// [`crate::providers::registry`]'s module docs means a second row for the same slug adds
    /// no routing information, only picker noise. Used to expose an autodetected provider the
    /// static table never mentions (`/model` across every configured backend, D-022): the
    /// table's own primaries stay first, so the default route never changes.
    pub fn with_fallback_candidate(&mut self, role: Role, candidate: RoleCandidate) {
        let candidates = self.roles.entry(role.as_str().to_string()).or_default();
        if candidates.iter().any(|c| c.provider == candidate.provider) {
            return;
        }
        candidates.push(candidate);
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
    /// fallback candidate with `degraded_ok = true`. Embedder gets a single embedding-model
    /// candidate. All candidates use unlimited rate/volume limits and real per-model pricing
    /// ([`OPUS_5_5_PRICE`]/[`SONNET_5_PRICE`]/[`HAIKU_4_5_PRICE`], plus a `text-embedding-3-small`
    /// rate for the embedder), so `tel-completion-cost-field`/`tm stats`/budget tier-down have
    /// real dollar figures for a project that never hand-edits `providers.toml`
    /// (`critic-real-prices-and-price-unit`). A project-scoped `providers.toml` can still
    /// override any of these per candidate.
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
                Role::Decider => {
                    // Default to the network-free mock decider, not an Anthropic completion
                    // model: `Role::Decider` candidates dispatch through
                    // `crate::providers::registry::Registry::build_decider` to a
                    // `crate::decide::DecisionProvider` (`"mock"`/`"systemone"`/
                    // `"systemone-http"`), a different trait from every other role's `Provider`,
                    // so an `anthropic`/`claude-*` row here would be inert at best (silently
                    // skipped by `Registry::build_fabric`) and misleading at worst (`tm doctor`
                    // would print a model this role never actually calls). `MockDecisionProvider`
                    // needs no credentials, so a fresh project's default table stays fully
                    // functional (shadow-classifying nothing, per D-020, until a real backend is
                    // configured) with zero setup.
                    vec![RoleCandidate {
                        provider: "mock".to_string(),
                        model: "mock-decider".to_string(),
                        max_concurrency: 1,
                        degraded_ok: false,
                        price: None,
                        limits: Limits::unlimited(),
                    }]
                }
                Role::Embedder => {
                    // Anthropic has no embedding endpoint. Use a provider whose declared
                    // capabilities match this role instead of advertising a fictitious model.
                    // OpenAI's published rate for `text-embedding-3-small` is $0.02 per
                    // 1,000,000 input tokens; embeddings have no output tokens to price.
                    vec![RoleCandidate {
                        provider: "openai".to_string(),
                        model: "text-embedding-3-small".to_string(),
                        max_concurrency: 100,
                        degraded_ok: false,
                        price: Some(Price {
                            input_micros_per_million_tokens: 20_000,
                            output_micros_per_million_tokens: 0,
                        }),
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
                        price: Some(HAIKU_4_5_PRICE),
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
                                price: Some(HAIKU_4_5_PRICE),
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
                        price: Some(HAIKU_4_5_PRICE),
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
                        price: Some(SONNET_5_PRICE),
                        limits: Limits::unlimited(),
                    };
                    if let Some(model) = devpass_model {
                        primary.provider = "devpass".to_string();
                        primary.model = model.to_string();
                        // DevPass's own rate isn't a published per-token price (it's a
                        // subscription pass-through), so leave it unpriced rather than
                        // misreporting Anthropic's direct-API rate for a different provider.
                        primary.price = None;
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
                                price: Some(HAIKU_4_5_PRICE),
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
                        price: Some(OPUS_5_5_PRICE),
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
                                price: Some(SONNET_5_PRICE),
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
                        price: Some(SONNET_5_PRICE),
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
                                price: Some(HAIKU_4_5_PRICE),
                                limits: Limits::unlimited(),
                            },
                        ]
                    }
                }
            };

            roles.insert(role.as_str().to_string(), candidates);
        }

        RoleTable {
            roles,
            decider_meta: BTreeMap::new(),
        }
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
    fn fallback_candidates_append_once_and_never_reorder_primaries() {
        let mut table = RoleTable::default_table_with(None);
        let primary = table.candidates_for(Role::CoderFast)[0].clone();
        table.with_fallback_candidate(
            Role::CoderFast,
            RoleCandidate {
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                max_concurrency: 10,
                degraded_ok: false,
                price: None,
                limits: Limits::unlimited(),
            },
        );
        table.with_fallback_candidate(
            Role::CoderFast,
            RoleCandidate {
                provider: "openai".to_string(),
                model: "gpt-4o".to_string(),
                max_concurrency: 10,
                degraded_ok: false,
                price: None,
                limits: Limits::unlimited(),
            },
        );
        let rows = table.candidates_for(Role::CoderFast);
        assert_eq!(rows[0], primary, "the primary never moves");
        assert_eq!(
            rows.iter().filter(|c| c.provider == "openai").count(),
            1,
            "one row per slug, the first model wins"
        );
        table.validate().expect("augmented table stays valid");
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
    fn embedder_default_uses_a_real_embedding_backend() {
        let table = RoleTable::default_table_with(None);
        let candidate = &table.candidates_for(Role::Embedder)[0];
        assert_eq!(candidate.provider, "openai");
        assert_eq!(candidate.model, "text-embedding-3-small");
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
    fn typoed_candidate_policy_is_rejected_instead_of_ignored() {
        let err = RoleTable::parse(
            r#"
[coder.fast]
candidates = [
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1, degraded_okk = true }
]
"#,
        )
        .expect_err("unknown policy keys must not silently take defaults");
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
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 10, degraded_ok = true, price = { input_micros_per_million_tokens = 1000000, output_micros_per_million_tokens = 2000000 } }
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

        // All other roles retain their Anthropic defaults; embeddings use OpenAI because
        // Anthropic does not expose the embedding capability, and the decider defaults to the
        // network-free mock backend (see `default_table_with`'s `Role::Decider` arm).
        for role in Role::ALL {
            if matches!(role, Role::Embedder | Role::Decider) {
                continue;
            }
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
            if matches!(role, Role::CoderFast | Role::Embedder | Role::Decider) {
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

    #[test]
    fn default_decider_is_the_mock_backend() {
        let table = RoleTable::default_table_with(None);
        let candidates = table.candidates_for(Role::Decider);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider, "mock");
        assert!(!candidates[0].degraded_ok);
        // No config-supplied endpoint override for the built-in default.
        let endpoint = table.decider_endpoint(&candidates[0].provider, &candidates[0].model);
        assert_eq!(endpoint, DeciderEndpoint::default());
    }

    #[test]
    fn decider_role_parses_a_mock_and_a_systemone_http_candidate_with_endpoint_overrides() {
        let toml = r#"
[decider]
candidates = [
  { provider = "mock", model = "mock-decider", max_concurrency = 1 },
  { provider = "systemone-http", model = "typesafe-ai/jev", max_concurrency = 2, base_url = "https://jevmlx.local", token_env = "JEV_TOKEN" }
]
"#;
        let table = RoleTable::parse(toml).expect("a decider role with endpoint overrides parses");
        let candidates = table.candidates_for(Role::Decider);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].provider, "mock");
        assert_eq!(candidates[1].provider, "systemone-http");
        assert_eq!(candidates[1].model, "typesafe-ai/jev");

        // The mock candidate got no endpoint override.
        assert_eq!(
            table.decider_endpoint("mock", "mock-decider"),
            DeciderEndpoint::default()
        );
        // The systemone-http candidate's override round-trips through the side table.
        let endpoint = table.decider_endpoint("systemone-http", "typesafe-ai/jev");
        assert_eq!(endpoint.base_url.as_deref(), Some("https://jevmlx.local"));
        assert_eq!(endpoint.token_env.as_deref(), Some("JEV_TOKEN"));
    }

    #[test]
    fn empty_decider_role_is_rejected_like_any_other_empty_role() {
        let toml = "[decider]\ncandidates = []\n";
        let err = RoleTable::parse(toml).unwrap_err();
        assert!(matches!(err, RoleConfigError::EmptyRole(role) if role == "decider"));
    }

    #[test]
    fn base_url_on_a_non_decider_role_is_rejected() {
        let toml = r#"
[coder.fast]
candidates = [
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 10, base_url = "https://example.invalid" }
]
"#;
        let err = RoleTable::parse(toml).unwrap_err();
        assert!(matches!(err, RoleConfigError::InvalidToml(_)));
    }

    #[test]
    fn token_env_on_a_non_decider_role_is_rejected() {
        let toml = r#"
[coder.fast]
candidates = [
  { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 10, token_env = "SOME_TOKEN" }
]
"#;
        let err = RoleTable::parse(toml).unwrap_err();
        assert!(matches!(err, RoleConfigError::InvalidToml(_)));
    }

    #[test]
    fn typoed_decider_endpoint_key_is_rejected_instead_of_ignored() {
        let toml = r#"
[decider]
candidates = [
  { provider = "systemone-http", model = "typesafe-ai/jev", max_concurrency = 1, base_urll = "https://jevmlx.local" }
]
"#;
        let err = RoleTable::parse(toml).expect_err("unknown candidate keys must not be ignored");
        assert!(matches!(err, RoleConfigError::InvalidToml(_)));
    }

    #[test]
    fn decider_endpoint_overrides_round_trip_through_toml() {
        let toml = r#"
[decider]
candidates = [
  { provider = "systemone-http", model = "typesafe-ai/jev", max_concurrency = 1, base_url = "https://jevmlx.local", token_env = "JEV_TOKEN" }
]
"#;
        let table = RoleTable::parse(toml).expect("valid decider TOML parses");
        let serialized = table.to_toml_string().expect("serializes");
        let reparsed = RoleTable::parse(&serialized).expect("reparses");
        assert_eq!(table, reparsed);
        let endpoint = reparsed.decider_endpoint("systemone-http", "typesafe-ai/jev");
        assert_eq!(endpoint.base_url.as_deref(), Some("https://jevmlx.local"));
        assert_eq!(endpoint.token_env.as_deref(), Some("JEV_TOKEN"));
    }

    #[test]
    fn decider_endpoint_override_does_not_leak_onto_a_same_named_candidate_on_another_role() {
        // Regression: `(provider, model)` is not unique across roles. A `mock`/`shared-name`
        // decider candidate with an endpoint override must not make `to_toml_string` write
        // `base_url`/`token_env` onto an unrelated `coder_fast` candidate that happens to share
        // the same provider/model strings — that would fail to reparse (those fields are
        // rejected on any role but `decider`).
        let toml = r#"
[decider]
candidates = [
  { provider = "mock", model = "shared-name", max_concurrency = 1, base_url = "https://jevmlx.local", token_env = "JEV_TOKEN" }
]

[coder.fast]
candidates = [
  { provider = "mock", model = "shared-name", max_concurrency = 10 }
]
"#;
        let table = RoleTable::parse(toml).expect("valid TOML with a cross-role name collision");
        let serialized = table.to_toml_string().expect("serializes");
        let reparsed = RoleTable::parse(&serialized)
            .expect("reparses without the endpoint override leaking onto coder_fast");
        assert_eq!(table, reparsed);
        assert_eq!(
            reparsed.decider_endpoint("mock", "shared-name"),
            DeciderEndpoint {
                base_url: Some("https://jevmlx.local".to_string()),
                token_env: Some("JEV_TOKEN".to_string()),
            }
        );
    }

    // ---- `Price`'s per-million unit and legacy per-token compat (critic-real-prices-and-price-unit) ----

    #[test]
    fn price_parses_the_legacy_per_token_field_names_scaled_up_by_a_million() {
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1, price = { input_micros_per_token = 42, output_micros_per_token = 84 } }]\n",
        )
        .expect("legacy per-token price field names still parse");
        let price = table.candidates_for(Role::CoderFast)[0]
            .price
            .expect("price present");
        assert_eq!(
            price,
            Price {
                input_micros_per_million_tokens: 42_000_000,
                output_micros_per_million_tokens: 84_000_000,
            },
            "a legacy per-token value scales up by 1,000,000 to the same real price"
        );
    }

    #[test]
    fn price_rejects_mixing_legacy_and_current_field_names() {
        let err = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1, price = { input_micros_per_token = 42, output_micros_per_million_tokens = 84000000 } }]\n",
        )
        .expect_err("mixing legacy and current price field names must not silently pick one");
        assert!(matches!(err, RoleConfigError::InvalidToml(_)));
    }

    #[test]
    fn price_serializes_only_the_current_per_million_field_names() {
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1, price = { input_micros_per_token = 42, output_micros_per_token = 84 } }]\n",
        )
        .expect("legacy per-token price field names still parse");
        let serialized = table.to_toml_string().expect("serializes");
        assert!(
            serialized.contains("input_micros_per_million_tokens"),
            "round-tripped TOML uses the current field name: {serialized}"
        );
        assert!(
            !serialized.contains("input_micros_per_token ")
                && !serialized.contains("input_micros_per_token="),
            "round-tripped TOML must not resurrect the legacy field name: {serialized}"
        );
        let reparsed = RoleTable::parse(&serialized).expect("round-tripped TOML reparses");
        assert_eq!(table, reparsed);
    }
}
