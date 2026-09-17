//! Wires a [`crate::role_config::RoleTable`] to a live [`crate::fabric::Fabric`] by autodetecting
//! which of this crate's twenty backends are configured (via [`crate::providers::ProviderInfo`])
//! and constructing exactly those, generalizing the single-provider pattern
//! `crates/tm-cli/src/agent.rs::build_fabric` already hand-writes for `AnthropicProvider` alone:
//!
//! ```ignore
//! let table = RoleTable::default_table();
//! let fabric = Fabric::new(table, clock.clone());
//! let provider = AnthropicProvider::from_env(ModelId::new("anthropic", AGENT_MODEL), clock)?;
//! fabric.register_provider(Arc::new(provider));
//! ```
//!
//! This module is the one place in this crate that is allowed to match on a provider id string
//! and know about every sibling module by name — every other provider module stays ignorant of
//! its siblings.
//!
//! # IMPL: the full dispatch table
//!
//! [`Registry::known_providers`] must return one [`crate::providers::ProviderInfo`] per
//! constructible backend, i.e. exactly these twenty calls (every stub struct's `info()`, plus
//! `crate::anthropic::AnthropicProvider` — which predates this module and has no `info()` of its
//! own; synthesize a literal `ProviderInfo` for it here with `id: "anthropic"`, env var
//! `ANTHROPIC_API_KEY` required, capabilities `{ completion: true, embedding: false,
//! streaming: true, tool_use: true, vision: false }`, rather than editing `anthropic.rs`, which
//! this agent does not own):
//!
//! ```ignore
//! crate::anthropic::AnthropicProvider  (synthesized ProviderInfo, see above — no info() method)
//! crate::providers::openai::OpenAiProvider
//! crate::providers::openrouter::OpenRouterProvider
//! crate::providers::openrouter::GithubModelsProvider
//! crate::providers::fast::GroqProvider
//! crate::providers::fast::CerebrasProvider
//! crate::providers::frontier::DeepSeekProvider
//! crate::providers::frontier::MistralProvider
//! crate::providers::frontier::XaiProvider
//! crate::providers::serverless::TogetherProvider
//! crate::providers::serverless::FireworksProvider
//! crate::providers::serverless::HuggingFaceProvider
//! crate::providers::gemini::GeminiProvider
//! crate::providers::local::OllamaProvider
//! crate::providers::local::LmStudioProvider
//! crate::providers::local::LlamaCppProvider
//! crate::providers::cloud::AzureOpenAiProvider
//! crate::providers::cloud::BedrockProvider
//! crate::providers::cloud::VertexProvider
//! crate::providers::compat::DevPassProvider
//! crate::providers::cloudflare::CloudflareWorkersAiProvider
//! ```
//!
//! [`Registry::build_provider`] dispatches one [`crate::role_config::RoleCandidate::provider`]
//! slug to the matching struct's `from_env(ModelId::new(candidate.provider, candidate.model),
//! clock)`, using the same `id` strings as the `ProviderInfo::id` values above (`"anthropic"` ->
//! [`crate::anthropic::AnthropicProvider::from_env`], `"openai"` ->
//! [`crate::providers::openai::OpenAiProvider::from_env`], `"github-models"` ->
//! [`crate::providers::openrouter::GithubModelsProvider::from_env`], and so on for all twenty);
//! an unrecognized slug is a config error, not a panic — return
//! [`crate::types::ProviderError::InvalidRequest`] naming the unknown slug.
//!
//! [`Registry::build_fabric`] walks `Role::ALL` (`tm_types::Role::ALL`, the same array
//! `Fabric::register_provider` already iterates), collects the distinct `provider` slugs across
//! every role's candidates via [`crate::role_config::RoleTable::candidates_for`], calls
//! [`Registry::build_provider`] once per distinct slug (not once per candidate — see the note
//! below on why), and registers each successfully-built provider; a slug whose backend is not
//! configured (autodetection said no) is *skipped*, not an error, so a role table can list more
//! candidates than any one environment actually has credentials for and still route through
//! whichever ones are live.
//!
//! # IMPL: a real routing gap this agent should document, not silently paper over
//!
//! [`crate::fabric::Fabric::register_provider`] keys its registry solely by
//! [`crate::fabric::Provider::id`] (a single `&str`), and every stub struct in this crate's
//! `from_env` bakes in one fixed `model: ModelId` at construction — exactly matching
//! [`crate::anthropic::AnthropicProvider`]'s existing shape. If `providers.toml` ever lists two
//! candidates under the *same* `provider` slug with two *different* models (e.g. `"openai"` /
//! `"gpt-4o"` and `"openai"` / `"gpt-4o-mini"` for two different roles), only one of those models
//! can actually be registered: [`crate::fabric::Fabric::register_provider`] inserts by id into a
//! `BTreeMap`, so the second registration silently replaces the first, and *both* roles' traffic
//! ends up hitting whichever model was registered last — not a crash, a silent misroute. Fixing
//! this for real means changing [`crate::fabric::Fabric::register_provider`]'s keying, which is
//! out of scope for this file (`fabric.rs` belongs to a different owner). Until that lands,
//! [`Registry::build_fabric`] must at least detect the collision and refuse to build rather than
//! misroute silently: when two candidates share a `provider` slug but differ in `model`, return
//! [`crate::types::ProviderError::InvalidRequest`] naming both models and the shared slug, rather
//! than registering the second over the first.

use std::sync::Arc;

use tm_types::Clock;

use crate::fabric::{Fabric, Provider};
use crate::providers::ProviderInfo;
use crate::role_config::{RoleCandidate, RoleTable};
use crate::types::ProviderError;

/// Autodetects and constructs this crate's provider fleet from the environment, and wires a
/// [`RoleTable`] to a ready-to-use [`Fabric`]. See the module docs for the full dispatch table
/// and the known `provider`-slug-collision gap.
pub struct Registry;

impl Registry {
    // IMPL: return one ProviderInfo per entry in the module docs' dispatch table (twenty total:
    // the nineteen stub structs' own `X::info()` plus one synthesized literal for
    // `crate::anthropic::AnthropicProvider`, per the module docs).
    /// Every backend this crate knows how to construct, regardless of whether it is currently
    /// configured. Callers wanting only what's usable right now want [`Registry::autodetect`].
    pub fn known_providers() -> Vec<ProviderInfo> {
        todo!("return the literal ProviderInfo list from the module doc comment's dispatch table")
    }

    // IMPL: `Self::known_providers().into_iter().filter(|info| info.is_configured()).collect()`.
    // Note the `local` module's caveat (its `info()`s are *always* "configured" by this
    // definition since every one of their env vars is optional) — callers that care whether a
    // local backend is actually reachable need `crate::providers::local`'s own doc comment about
    // probing, not this function.
    /// Every backend [`ProviderInfo::is_configured`] currently as present in the environment.
    pub fn autodetect() -> Vec<ProviderInfo> {
        todo!("filter Self::known_providers() by ProviderInfo::is_configured(), per the IMPL comment above")
    }

    // IMPL: match `candidate.provider.as_str()` against the twenty slugs in the module docs'
    // dispatch table, calling the matching struct's
    // `from_env(ModelId::new(candidate.provider.clone(), candidate.model.clone()), clock)`.
    // An unmatched slug -> `Err(ProviderError::InvalidRequest(format!("unknown provider: {}",
    // candidate.provider)))`.
    /// Construct one [`Provider`] for `candidate`, dispatching on [`RoleCandidate::provider`].
    pub fn build_provider(
        candidate: &RoleCandidate,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<dyn Provider>, ProviderError> {
        let _ = (candidate, clock);
        todo!("dispatch on candidate.provider to the matching struct's from_env, per the IMPL comment above")
    }

    // IMPL:
    // 1. Build an empty `BTreeMap<String, RoleCandidate>` keyed by provider slug, to detect the
    //    same-slug-different-model collision documented above while walking every role's
    //    candidates exactly once.
    // 2. For each `role in tm_types::Role::ALL`, for each `candidate in
    //    table.candidates_for(*role)`: if the map already has this `candidate.provider` with a
    //    *different* `model`, return the collision `ProviderError::InvalidRequest` described in
    //    the module docs; otherwise insert/overwrite (same model re-seen is fine, not a
    //    collision).
    // 3. `let fabric = Fabric::new(table.clone(), clock.clone());` (the table itself has already
    //    been consumed by reference above, and `Fabric::new` takes it by value — clone once here,
    //    or restructure step 1-2 to borrow `&table` throughout and move it into `Fabric::new`
    //    last; either is fine, this is not a design decision, just an ordering detail).
    // 4. For each distinct slug collected in step 1: skip it if
    //    `!Self::autodetect().iter().any(|info| info.id == slug)` (not configured — not an
    //    error, see module docs); otherwise `Self::build_provider(&candidate, clock.clone())?`
    //    and `fabric.register_provider(provider)`.
    // 5. Return the built `fabric`.
    /// Build a [`Fabric`] for `table`, registering every distinct `provider` slug the table
    /// references that is currently configured, and skipping (not erroring on) any that isn't.
    /// Errors only on the slug-collision case documented above or a `provider` slug this crate
    /// does not recognize at all (surfaced through [`Registry::build_provider`]).
    pub fn build_fabric(table: RoleTable, clock: Arc<dyn Clock>) -> Result<Fabric, ProviderError> {
        let _ = (table, clock);
        todo!("walk Role::ALL, detect slug/model collisions, register every configured distinct provider slug, per the IMPL comment above")
    }
}
