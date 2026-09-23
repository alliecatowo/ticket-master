//! The provider fleet: one [`crate::fabric::Provider`] implementation per external backend.
//!
//! This module is the merge point for eleven agents working in parallel, each owning exactly one
//! sibling file below and never this one. Every backend module is declared here up front so no
//! implementing agent ever needs to touch this file to make their module reachable.
//!
//! ## The one shared contract: capability + env-var declaration
//!
//! [`crate::route`] and [`crate::fabric::Fabric`] only need a live [`crate::fabric::Provider`]
//! instance. [`registry::Registry`] autodetection needs more than that: it has to decide *which*
//! providers are even constructible — which env vars are present — before constructing anything,
//! and it has to describe what a provider can do (streaming? tool use? embeddings?) without
//! calling it. Neither question can be answered by an instance method, because answering them is
//! exactly the step that precedes deciding whether an instance can be built at all.
//!
//! So capability/env-var metadata is **not** a method on the [`crate::fabric::Provider`] trait
//! (which stays exactly as `fabric.rs` defines it — no provider module edits that trait). It is a
//! `pub fn info() -> ProviderInfo` **inherent associated function** that every provider
//! struct in this module tree exposes, callable with no instance, no I/O and no env access of its
//! own:
//!
//! ```ignore
//! impl OpenAiProvider {
//!     pub fn info() -> ProviderInfo { ProviderInfo { .. } }
//! }
//! ```
//!
//! A module that wraps more than one backend (e.g. [`fast`] wraps both Groq and Cerebras) defines
//! one struct per backend and one `info()` per struct — never a single `info()` covering several
//! backends, since [`registry::Registry`] autodetection keys its whole decision ("is Groq
//! configured?" vs "is Cerebras configured?") on one [`ProviderInfo`] per constructible thing.
//!
//! [`ProviderInfo::env_vars`] lists every environment variable the struct's `from_env()`
//! constructor reads, each marked [`EnvVarRequirement::required`] or not (an optional var such as
//! a base-URL override does not gate autodetection; at least one *required* var present is what
//! tells [`registry::Registry`] the backend is configured). [`ProviderInfo::capabilities`] is pure
//! data the registry and any future CLI/config surface can inspect without a network call.

use crate::types::ProviderError;

pub mod cloud;
pub mod cloudflare;
pub mod codex_chatgpt;
pub mod compat;
pub mod fast;
pub mod frontier;
pub mod gemini;
pub mod local;
pub mod openai;
pub mod openrouter;
pub mod registry;
pub mod serverless;

/// What a provider backend can do, as static data — never answered by making a network call.
///
/// `vision` is `false` everywhere today because [`crate::types::ContentBlock`] has no image
/// variant yet; the field exists so it flips to meaningful the moment that lands, without every
/// provider module needing a signature change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Can serve [`crate::fabric::Provider::complete`].
    pub completion: bool,
    /// Can serve [`crate::fabric::Provider::embed`].
    pub embedding: bool,
    /// Understands `stream: true` on [`crate::types::CompletionRequest`] (accumulated into a
    /// single [`crate::types::Completion`] either way — see `compat.rs` module docs).
    pub streaming: bool,
    /// Accepts [`crate::types::ToolDef`] / emits [`crate::types::ContentBlock::ToolUse`].
    pub tool_use: bool,
    /// Accepts image content blocks. Always `false` until `ContentBlock` grows one.
    pub vision: bool,
}

/// One environment variable a provider's `from_env()` constructor reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvVarRequirement {
    /// The variable name, e.g. `"OPENAI_API_KEY"`.
    pub name: &'static str,
    /// Whether `from_env()` fails without it. `false` means an optional override (base URL,
    /// org id, ...) with a hardcoded default.
    pub required: bool,
    /// One line on what it configures, for `--help`/diagnostic output.
    pub description: &'static str,
}

/// Static metadata for one constructible provider backend, returned by that backend struct's
/// `pub fn info() -> ProviderInfo` inherent associated function. See the module docs above
/// for why this is a free function and not a [`crate::fabric::Provider`] trait method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderInfo {
    /// The slug this backend registers under with [`crate::fabric::Fabric::register_provider`],
    /// matching a `providers.toml` candidate's `provider` field, e.g. `"openai"`.
    pub id: &'static str,
    /// Human-readable name for diagnostics/CLI output, e.g. `"OpenAI"`.
    pub display_name: &'static str,
    /// Every env var `from_env()` reads. [`registry::Registry`] autodetection treats a backend as
    /// configured when every entry with `required: true` is present in the environment.
    pub env_vars: &'static [EnvVarRequirement],
    /// What this backend can do.
    pub capabilities: Capabilities,
}

impl ProviderInfo {
    /// Whether every `required` env var in [`ProviderInfo::env_vars`] is currently set, i.e.
    /// whether [`registry::Registry`] autodetection should offer to construct this backend.
    ///
    /// Reads the environment (this is the one place in this module tree that is allowed to,
    /// since it is explicitly a detection probe, not a hidden non-determinism leak into request
    /// shaping or retry logic).
    ///
    /// This is env-var presence only, never a network reachability check — see [`Availability`]
    /// for the three-state answer that also accounts for reachability where it matters (the
    /// three [`local`] backends, whose env vars are all optional so this method alone is
    /// vacuously `true` for them regardless of whether a server is actually listening).
    pub fn is_configured(&self) -> bool {
        // GitHub Models supports a namespaced alias so an unrelated GitHub CLI token does not
        // shadow it. Keep discovery/status consistent with the provider constructor.
        if self.id == "github-models" {
            let primary = std::env::var("GITHUB_TOKEN").ok();
            let alias = std::env::var("GITHUB_MODELS_TOKEN").ok();
            return openrouter::github_models_token(primary.as_deref(), alias.as_deref()).is_some();
        }
        // Gemini's constructor deliberately accepts a common alternate credential name too. Keep
        // detection in sync so a valid alias is not reported as missing configuration.
        if self.id == "gemini" && std::env::var("GOOGLE_API_KEY").is_ok_and(|v| !v.trim().is_empty())
        {
            return true;
        }
        self.env_vars
            .iter()
            .filter(|v| v.required)
            .all(|v| std::env::var(v.name).is_ok_and(|value| !value.trim().is_empty()))
    }
}

/// The three [`local`] backend ids: the only ones where [`ProviderInfo::is_configured`] can be
/// vacuously `true` with nothing actually listening, per that module's doc comment. Named here
/// once so [`registry::Registry`] and any CLI caller test membership the same way rather than
/// re-listing the three ids in more than one place.
pub const LOCAL_PROVIDER_IDS: [&str; 3] = ["ollama", "lm-studio", "llama-cpp"];

/// Whether a provider backend is actually safe to route traffic to right now — a strictly more
/// honest answer than [`ProviderInfo::is_configured`] alone, which is only env-var presence.
///
/// For every backend except the three named in [`LOCAL_PROVIDER_IDS`], `is_configured() == true`
/// is the whole story: a present API key is the only thing this crate can check without making a
/// speculative network call to a paid third-party API on every `tm provider detect`, so
/// `Ready`/`NotConfigured` is exactly `is_configured()`'s boolean, just spelled as a named state
/// for a uniform CLI surface. For the three local backends, every env var is optional with a
/// working default (see `providers::local`'s module docs), so `is_configured()` is always `true`
/// regardless of whether a server is listening — [`registry::Registry::availability`] closes that
/// gap with a short-timeout `GET /v1/models` probe, and `ConfiguredButUnreachable` is the state
/// that probe existing to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// No required env var is set ([`ProviderInfo::is_configured`] is `false`).
    NotConfigured,
    /// Configured (or vacuously so, for a local backend), but a reachability probe found nothing
    /// listening. Only reachable for the three [`LOCAL_PROVIDER_IDS`] backends today — every
    /// other backend has no probe, so it can never land here.
    ConfiguredButUnreachable,
    /// Configuration exists, but this backend cannot currently construct a usable provider.
    /// Used by integrations for explicitly stubbed backends such as Bedrock before SigV4 exists.
    Unusable,
    /// Configured, and reachable when a probe applies.
    Ready,
}

impl Availability {
    /// Combine env-var presence with an optional reachability probe result into one
    /// [`Availability`]. `probed` is `None` for a backend with no reachability probe (every
    /// backend outside [`LOCAL_PROVIDER_IDS`]) — env-var presence alone decides those. `Some(_)`
    /// is the probe's own success/failure for a backend that has one.
    ///
    /// A free function of two booleans-ish inputs rather than a method that reads the network
    /// itself, so this decision table is unit-testable with no env access and no mock HTTP
    /// client (which `xtask hygiene` forbids in test code anyway).
    pub fn derive(is_configured: bool, probed: Option<bool>) -> Availability {
        if !is_configured {
            return Availability::NotConfigured;
        }
        match probed {
            Some(false) => Availability::ConfiguredButUnreachable,
            Some(true) | None => Availability::Ready,
        }
    }
}

/// The richer result of probing one [`LOCAL_PROVIDER_IDS`] backend, beyond the plain
/// [`Availability`] three-state: a caller that wants to actually *use* a local server (not just
/// report on it, e.g. `tm-cli`'s `genesis` command picking a fallback provider) needs to know not
/// just "is something listening" but "does it have a model loaded",
/// since a reachable server with zero models pulled still can't serve a completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalProbe {
    /// Nothing answered `GET /v1/models` within the short probe timeout.
    Unreachable,
    /// Something answered, but listed zero models.
    ReachableNoModels,
    /// Something answered and listed at least one model.
    Ready {
        /// The first model id listed (not necessarily the "best" one — just *a* usable model id
        /// for a caller that needs one).
        first_model: String,
    },
}

impl LocalProbe {
    /// Whether *something* answered at all, collapsing [`LocalProbe::ReachableNoModels`] and
    /// [`LocalProbe::Ready`] together — the distinction [`Availability::derive`]'s `probed`
    /// parameter wants (a server with zero models pulled is still "reachable": the operator has
    /// work to do, but it is not the "nothing is listening" case).
    pub fn reachable(&self) -> bool {
        !matches!(self, LocalProbe::Unreachable)
    }
}

/// Build a uniform [`ProviderError::AuthFailed`] for a missing required env var, so every
/// `from_env()` constructor across this module tree reports the same shape of error.
pub(crate) fn missing_env_var(name: &str) -> ProviderError {
    ProviderError::AuthFailed(format!("{name} is not set"))
}

#[cfg(test)]
mod availability_tests {
    use super::*;

    #[test]
    fn not_configured_wins_regardless_of_probe() {
        assert_eq!(
            Availability::derive(false, None),
            Availability::NotConfigured
        );
        assert_eq!(
            Availability::derive(false, Some(true)),
            Availability::NotConfigured
        );
        assert_eq!(
            Availability::derive(false, Some(false)),
            Availability::NotConfigured
        );
    }

    #[test]
    fn configured_with_no_probe_is_ready() {
        // Every backend outside `LOCAL_PROVIDER_IDS`: env-var presence alone is the whole story.
        assert_eq!(Availability::derive(true, None), Availability::Ready);
    }

    #[test]
    fn configured_and_probed_reachable_is_ready() {
        assert_eq!(Availability::derive(true, Some(true)), Availability::Ready);
    }

    #[test]
    fn configured_but_probed_unreachable_is_the_honest_third_state() {
        assert_eq!(
            Availability::derive(true, Some(false)),
            Availability::ConfiguredButUnreachable
        );
    }

    #[test]
    fn local_probe_reachable_is_true_for_anything_but_unreachable() {
        assert!(!LocalProbe::Unreachable.reachable());
        assert!(LocalProbe::ReachableNoModels.reachable());
        assert!(
            LocalProbe::Ready {
                first_model: "llama3".to_string()
            }
            .reachable()
        );
    }

    #[test]
    fn local_provider_ids_names_exactly_the_three_zero_signup_backends() {
        assert_eq!(LOCAL_PROVIDER_IDS, ["ollama", "lm-studio", "llama-cpp"]);
    }
}
