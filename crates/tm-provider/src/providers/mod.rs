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
    pub fn is_configured(&self) -> bool {
        self.env_vars
            .iter()
            .filter(|v| v.required)
            .all(|v| std::env::var(v.name).is_ok())
    }
}

/// Build a uniform [`ProviderError::AuthFailed`] for a missing required env var, so every
/// `from_env()` constructor across this module tree reports the same shape of error.
pub(crate) fn missing_env_var(name: &str) -> ProviderError {
    ProviderError::AuthFailed(format!("{name} is not set"))
}
