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
use crate::providers::{
    Availability, Capabilities, EnvVarRequirement, LocalProbe, ProviderInfo, LOCAL_PROVIDER_IDS,
};
use crate::role_config::{RoleCandidate, RoleTable};
use crate::types::{ModelId, ProviderError};

/// Pick the model id [`Registry::probe_local`] reports as `first_model`: the first entry in
/// `models` that doesn't look embedding-only (name containing `"embed"`, e.g. Ollama's own
/// `nomic-embed-text`), or the literal first entry if every one matches. A pure function, split
/// out of `probe_local` so this name heuristic is unit-testable with no network access. `models`
/// must be non-empty — callers already branch on emptiness before reaching this.
fn pick_completion_model(models: &[String]) -> String {
    models
        .iter()
        .find(|m| !m.to_lowercase().contains("embed"))
        .cloned()
        .unwrap_or_else(|| models[0].clone())
}

/// Autodetects and constructs this crate's provider fleet from the environment, and wires a
/// [`RoleTable`] to a ready-to-use [`Fabric`]. See the module docs for the full dispatch table
/// and the known `provider`-slug-collision gap.
pub struct Registry;

impl Registry {
    /// Every backend this crate knows how to construct, regardless of whether it is currently
    /// configured. Callers wanting only what's usable right now want [`Registry::autodetect`].
    pub fn known_providers() -> Vec<ProviderInfo> {
        vec![
            // Predates this module; no `info()` of its own, so synthesized here per the module
            // docs rather than editing `anthropic.rs`, which this agent does not own.
            ProviderInfo {
                id: "anthropic",
                display_name: "Anthropic",
                env_vars: &[EnvVarRequirement {
                    name: "ANTHROPIC_API_KEY",
                    required: true,
                    description: "Bearer API key for the Anthropic Messages API",
                }],
                capabilities: Capabilities {
                    completion: true,
                    embedding: false,
                    streaming: true,
                    tool_use: true,
                    vision: false,
                },
            },
            crate::providers::openai::OpenAiProvider::info(),
            crate::providers::openrouter::OpenRouterProvider::info(),
            crate::providers::openrouter::GithubModelsProvider::info(),
            crate::providers::fast::GroqProvider::info(),
            crate::providers::fast::CerebrasProvider::info(),
            crate::providers::frontier::DeepSeekProvider::info(),
            crate::providers::frontier::MistralProvider::info(),
            crate::providers::frontier::XaiProvider::info(),
            crate::providers::serverless::TogetherProvider::info(),
            crate::providers::serverless::FireworksProvider::info(),
            crate::providers::serverless::HuggingFaceProvider::info(),
            crate::providers::gemini::GeminiProvider::info(),
            crate::providers::local::OllamaProvider::info(),
            crate::providers::local::LmStudioProvider::info(),
            crate::providers::local::LlamaCppProvider::info(),
            crate::providers::cloud::AzureOpenAiProvider::info(),
            crate::providers::cloud::BedrockProvider::info(),
            crate::providers::cloud::VertexProvider::info(),
            crate::providers::compat::DevPassProvider::info(),
            crate::providers::cloudflare::CloudflareWorkersAiProvider::info(),
        ]
    }

    /// Every backend [`ProviderInfo::is_configured`] currently as present in the environment.
    pub fn autodetect() -> Vec<ProviderInfo> {
        Self::known_providers()
            .into_iter()
            .filter(|info| info.is_configured())
            .collect()
    }

    /// `GET /v1/models` against one of [`LOCAL_PROVIDER_IDS`], returning the model ids it lists.
    /// Dispatches by `id` to the matching struct's `from_env`/`list_models`, mirroring
    /// [`Registry::build_provider`]'s dispatch shape but scoped to the three local backends.
    /// `Err` for a non-local `id` (this is not a general "list models for any backend" method —
    /// only the three local backends have a cheap, key-free `/v1/models` this crate can call
    /// speculatively).
    pub async fn local_models(
        id: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Vec<String>, ProviderError> {
        match id {
            "ollama" => {
                crate::providers::local::OllamaProvider::from_env(ModelId::new(id, "probe"), clock)?
                    .list_models()
                    .await
            }
            "lm-studio" => {
                crate::providers::local::LmStudioProvider::from_env(
                    ModelId::new(id, "probe"),
                    clock,
                )?
                .list_models()
                .await
            }
            "llama-cpp" => {
                crate::providers::local::LlamaCppProvider::from_env(
                    ModelId::new(id, "probe"),
                    clock,
                )?
                .list_models()
                .await
            }
            other => Err(ProviderError::InvalidRequest(format!(
                "not a local backend with a reachability probe: {other}"
            ))),
        }
    }

    /// Short-timeout reachability probe for one [`LOCAL_PROVIDER_IDS`] backend, richer than
    /// [`Availability`]: distinguishes nothing listening from a live server with zero models
    /// pulled, which a caller such as `tm-cli`'s `genesis` command needs in order to pick an
    /// actually-usable `(provider, model)` pair rather than just reporting a yes/no.
    ///
    /// `first_model` prefers a listed model id that doesn't look embedding-only (a name
    /// containing `"embed"`, e.g. Ollama's own `nomic-embed-text` — see the fixture in
    /// `providers::local`'s tests) over the literal first entry: a caller picking a model for a
    /// *completion* request off this field would otherwise silently get an embedding model
    /// whenever one happens to sort first, and fail later with a confusing mid-run provider
    /// error instead of a clear signal here. This is a name heuristic, not real capability
    /// introspection (this crate has none for local backends), so it falls back to the literal
    /// first entry if every listed model matches — a filtered guess is still strictly better
    /// information than none, and there is nothing better to fall back to.
    pub async fn probe_local(id: &str, clock: Arc<dyn Clock>) -> LocalProbe {
        match Self::local_models(id, clock).await {
            Ok(models) if models.is_empty() => LocalProbe::ReachableNoModels,
            Ok(models) => LocalProbe::Ready {
                first_model: pick_completion_model(&models),
            },
            Err(_) => LocalProbe::Unreachable,
        }
    }

    /// The honest three-state [`Availability`] for one [`ProviderInfo`], probing reachability for
    /// the three [`LOCAL_PROVIDER_IDS`] backends (whose `is_configured()` is vacuously `true`
    /// with nothing listening, per `providers::local`'s module docs) and falling back to
    /// [`ProviderInfo::is_configured`] alone for every other backend — this crate has no general
    /// reachability probe for a paid third-party API, only for the three that can be probed for
    /// free with no key.
    pub async fn availability(info: &ProviderInfo, clock: Arc<dyn Clock>) -> Availability {
        let is_configured = info.is_configured();
        if !is_configured {
            return Availability::NotConfigured;
        }
        if LOCAL_PROVIDER_IDS.contains(&info.id) {
            let reachable = Self::probe_local(info.id, clock).await.reachable();
            Availability::derive(is_configured, Some(reachable))
        } else {
            Availability::derive(is_configured, None)
        }
    }

    /// Construct one [`Provider`] for `candidate`, dispatching on [`RoleCandidate::provider`].
    pub fn build_provider(
        candidate: &RoleCandidate,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<dyn Provider>, ProviderError> {
        let model = || ModelId::new(candidate.provider.clone(), candidate.model.clone());
        let provider: Arc<dyn Provider> =
            match candidate.provider.as_str() {
                "anthropic" => Arc::new(crate::anthropic::AnthropicProvider::from_env(
                    model(),
                    clock,
                )?),
                "openai" => Arc::new(crate::providers::openai::OpenAiProvider::from_env(
                    model(),
                    clock,
                )?),
                "openrouter" => Arc::new(
                    crate::providers::openrouter::OpenRouterProvider::from_env(model(), clock)?,
                ),
                "github-models" => Arc::new(
                    crate::providers::openrouter::GithubModelsProvider::from_env(model(), clock)?,
                ),
                "groq" => Arc::new(crate::providers::fast::GroqProvider::from_env(
                    model(),
                    clock,
                )?),
                "cerebras" => Arc::new(crate::providers::fast::CerebrasProvider::from_env(
                    model(),
                    clock,
                )?),
                "deepseek" => Arc::new(crate::providers::frontier::DeepSeekProvider::from_env(
                    model(),
                    clock,
                )?),
                "mistral" => Arc::new(crate::providers::frontier::MistralProvider::from_env(
                    model(),
                    clock,
                )?),
                "xai" => Arc::new(crate::providers::frontier::XaiProvider::from_env(
                    model(),
                    clock,
                )?),
                "together" => Arc::new(crate::providers::serverless::TogetherProvider::from_env(
                    model(),
                    clock,
                )?),
                "fireworks" => Arc::new(crate::providers::serverless::FireworksProvider::from_env(
                    model(),
                    clock,
                )?),
                "huggingface" => Arc::new(
                    crate::providers::serverless::HuggingFaceProvider::from_env(model(), clock)?,
                ),
                "gemini" => Arc::new(crate::providers::gemini::GeminiProvider::from_env(
                    model(),
                    clock,
                )?),
                "ollama" => Arc::new(crate::providers::local::OllamaProvider::from_env(
                    model(),
                    clock,
                )?),
                "lm-studio" => Arc::new(crate::providers::local::LmStudioProvider::from_env(
                    model(),
                    clock,
                )?),
                "llama-cpp" => Arc::new(crate::providers::local::LlamaCppProvider::from_env(
                    model(),
                    clock,
                )?),
                "azure-openai" => Arc::new(crate::providers::cloud::AzureOpenAiProvider::from_env(
                    model(),
                    clock,
                )?),
                "bedrock" => Arc::new(crate::providers::cloud::BedrockProvider::from_env(
                    model(),
                    clock,
                )?),
                "vertex" => Arc::new(crate::providers::cloud::VertexProvider::from_env(
                    model(),
                    clock,
                )?),
                // DevPass reads its own model from `DEVPASS_MODEL`, not `candidate.model` — it has no
                // per-candidate model parameter, see `compat.rs`'s `DevPassProvider::from_env`.
                "devpass" => Arc::new(crate::providers::compat::DevPassProvider::from_env(clock)?),
                "cloudflare" => Arc::new(
                    crate::providers::cloudflare::CloudflareWorkersAiProvider::from_env(
                        model(),
                        clock,
                    )?,
                ),
                other => {
                    return Err(ProviderError::InvalidRequest(format!(
                        "unknown provider: {other}"
                    )))
                }
            };
        Ok(provider)
    }

    /// Build a [`Fabric`] for `table`, registering every distinct `provider` slug the table
    /// references that is currently configured, and skipping (not erroring on) any that isn't.
    /// Errors only on the slug-collision case documented above or a `provider` slug this crate
    /// does not recognize at all (surfaced through [`Registry::build_provider`]).
    pub fn build_fabric(table: RoleTable, clock: Arc<dyn Clock>) -> Result<Fabric, ProviderError> {
        let mut distinct: std::collections::BTreeMap<String, RoleCandidate> =
            std::collections::BTreeMap::new();
        for role in tm_types::Role::ALL {
            for candidate in table.candidates_for(role) {
                if let Some(existing) = distinct.get(&candidate.provider) {
                    if existing.model != candidate.model {
                        return Err(ProviderError::InvalidRequest(format!(
                            "provider {} is routed to two different models ({} and {})",
                            candidate.provider, existing.model, candidate.model
                        )));
                    }
                } else {
                    distinct.insert(candidate.provider.clone(), candidate.clone());
                }
            }
        }

        let fabric = Fabric::new(table, clock.clone());

        let configured = Self::autodetect();
        for (slug, candidate) in &distinct {
            if !configured.iter().any(|info| info.id == slug) {
                continue;
            }
            let provider = Self::build_provider(candidate, clock.clone())?;
            fabric.register_provider(provider);
        }

        Ok(fabric)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::clock::FixedClock;

    // ---- pick_completion_model: pure, no network ----

    #[test]
    fn pick_completion_model_prefers_a_non_embedding_entry_even_when_it_sorts_second() {
        let models = vec!["nomic-embed-text".to_string(), "llama3:latest".to_string()];
        assert_eq!(pick_completion_model(&models), "llama3:latest");
    }

    #[test]
    fn pick_completion_model_keeps_the_literal_first_entry_when_it_is_already_fine() {
        let models = vec!["llama3:latest".to_string(), "nomic-embed-text".to_string()];
        assert_eq!(pick_completion_model(&models), "llama3:latest");
    }

    #[test]
    fn pick_completion_model_falls_back_to_the_first_entry_when_everything_looks_like_embeddings() {
        let models = vec![
            "nomic-embed-text".to_string(),
            "mxbai-embed-large".to_string(),
        ];
        assert_eq!(pick_completion_model(&models), "nomic-embed-text");
    }

    #[test]
    fn pick_completion_model_matches_embed_case_insensitively() {
        let models = vec!["Embed-Model".to_string(), "chat-model".to_string()];
        assert_eq!(pick_completion_model(&models), "chat-model");
    }

    /// Every `known_providers()` entry's `id` should be unique — a duplicate would mean two
    /// backends silently shadow each other in [`Registry::build_provider`]'s dispatch.
    #[test]
    fn known_provider_ids_are_unique() {
        let ids: Vec<&str> = Registry::known_providers().iter().map(|i| i.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            ids.len(),
            sorted.len(),
            "duplicate provider id in known_providers(): {ids:?}"
        );
    }

    /// [`Registry::build_provider`] must recognize every id [`Registry::known_providers`]
    /// advertises, or autodetection and dispatch would silently disagree about the fleet.
    #[test]
    fn every_known_provider_id_is_dispatchable() {
        // Anthropic requires ANTHROPIC_API_KEY; every other backend requires at least one env
        // var too. None are set in the test environment, so every dispatch should fail with
        // AuthFailed (proving the slug matched a real arm) rather than InvalidRequest("unknown
        // provider: ...") (which would mean the dispatch table is missing an id).
        for info in Registry::known_providers() {
            // Skip devpass and the local backends here: local backends have no required env
            // vars at all (see `providers::local`'s own doc comment) so `from_env` can succeed
            // with defaults, and devpass's dispatch match arm ignores `candidate.model`
            // entirely — neither breaks the "arm exists" assertion this test wants, so excluding
            // them just avoids asserting on a shape this test isn't about.
            if matches!(info.id, "devpass" | "ollama" | "lm-studio" | "llama-cpp") {
                continue;
            }
            let candidate = RoleCandidate {
                provider: info.id.to_string(),
                model: "test-model".to_string(),
                max_concurrency: 1,
                degraded_ok: false,
                price: None,
                limits: crate::role_config::Limits::unlimited(),
            };
            let clock = Arc::new(FixedClock::epoch());
            let err = Registry::build_provider(&candidate, clock)
                .err()
                .unwrap_or_else(|| panic!("{} unexpectedly built with no credentials", info.id));
            assert!(
                !matches!(err, ProviderError::InvalidRequest(ref m) if m.starts_with("unknown provider")),
                "{} is missing from Registry::build_provider's dispatch table: {err}",
                info.id
            );
        }
    }

    /// An id `build_provider` has never heard of is a config error naming the slug, not a panic.
    #[test]
    fn build_provider_rejects_unknown_slug() {
        let candidate = RoleCandidate {
            provider: "not-a-real-provider".to_string(),
            model: "whatever".to_string(),
            max_concurrency: 1,
            degraded_ok: false,
            price: None,
            limits: crate::role_config::Limits::unlimited(),
        };
        let clock = Arc::new(FixedClock::epoch());
        let err = match Registry::build_provider(&candidate, clock) {
            Ok(_) => panic!("unknown provider slug unexpectedly built"),
            Err(e) => e,
        };
        match err {
            ProviderError::InvalidRequest(msg) => assert!(msg.contains("not-a-real-provider")),
            other => panic!("expected InvalidRequest naming the unknown slug, got {other}"),
        }
    }

    /// `autodetect()` only ever returns backends `known_providers()` also lists, and never more
    /// than it.
    #[test]
    fn autodetect_is_a_subset_of_known_providers() {
        let known: Vec<&str> = Registry::known_providers().iter().map(|i| i.id).collect();
        for info in Registry::autodetect() {
            assert!(known.contains(&info.id));
        }
    }

    /// Two roles routed to the same provider slug under two different models must be refused,
    /// per the module docs' `register_provider` single-model-per-id gap, rather than silently
    /// misrouting one role's traffic onto the other role's model.
    #[test]
    fn build_fabric_rejects_same_provider_different_model_collision() {
        let toml = r#"
            [vision.frontier]
            candidates = [
                { provider = "openai", model = "gpt-4o", max_concurrency = 1 },
            ]

            [planner.frontier]
            candidates = [
                { provider = "openai", model = "gpt-4o-mini", max_concurrency = 1 },
            ]

            [architect.frontier]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [coder.deep]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [coder.fast]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [explorer.cheap]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [reviewer.semantic]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [auditor.semantic]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [synthesizer.long_context]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [summarizer.cheap]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [embedder]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [computer.use]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
        "#;
        let table = RoleTable::parse(toml).expect("valid providers.toml");
        let clock = Arc::new(FixedClock::epoch());
        let err = match Registry::build_fabric(table, clock) {
            Ok(_) => panic!("provider/model collision unexpectedly built a fabric"),
            Err(e) => e,
        };
        match err {
            ProviderError::InvalidRequest(msg) => {
                assert!(msg.contains("gpt-4o"));
                assert!(msg.contains("gpt-4o-mini"));
            }
            other => panic!("expected InvalidRequest naming the colliding models, got {other}"),
        }
    }

    /// A table with no collisions and no env vars configured should build a `Fabric` with no
    /// providers registered rather than erroring — every candidate's backend is simply skipped.
    ///
    /// Deliberately does *not* use [`RoleTable::default_table`]: that table routes frontier
    /// roles through `claude-opus-4-1` and non-frontier roles through `claude-sonnet-5`/
    /// `claude-haiku-3.5`, all under the single `"anthropic"` slug — exactly the same-slug,
    /// different-model shape [`build_fabric_rejects_same_provider_different_model_collision`]
    /// covers, and exactly the gap the module docs call out as real, not hypothetical. This test
    /// wants a table with *no* collision, to isolate the "unconfigured backend is skipped"
    /// behavior from that separately-tested collision behavior.
    #[test]
    fn build_fabric_skips_unconfigured_providers_without_erroring() {
        let toml = r#"
            [vision.frontier]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [planner.frontier]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [architect.frontier]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [coder.deep]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [coder.fast]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [explorer.cheap]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [reviewer.semantic]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [auditor.semantic]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [synthesizer.long_context]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [summarizer.cheap]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [embedder]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
            [computer.use]
            candidates = [ { provider = "anthropic", model = "claude-sonnet-5", max_concurrency = 1 } ]
        "#;
        let table = RoleTable::parse(toml).expect("valid providers.toml");
        let clock = Arc::new(FixedClock::epoch());
        // ANTHROPIC_API_KEY is not set in the test environment, so this should succeed with
        // nothing registered rather than erroring (autodetection said "not configured", which is
        // not a config error).
        let fabric = Registry::build_fabric(table, clock);
        assert!(
            fabric.is_ok(),
            "an unconfigured provider must be skipped, not error: {:?}",
            fabric.err()
        );
    }
}
