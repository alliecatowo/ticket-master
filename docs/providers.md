# Providers

`tm-provider` can construct twenty-one backend implementations of the `Provider` trait
(`crates/tm-provider/src/fabric.rs`). `crates/tm-provider/src/providers/registry.rs`'s `Registry`
is the single place that knows all of their slugs, env vars, and capabilities:

- `Registry::known_providers()` — every backend this crate can build, regardless of whether it's
  configured right now.
- `Registry::autodetect()` — the subset of the above whose required env vars are actually set in
  the current environment. This never reads, logs, or returns any key's value — only whether
  `std::env::var(name).is_ok()`.
- `Registry::build_provider(candidate, clock)` — builds one live `Provider` for a
  `providers.toml` `(provider, model)` candidate.
- `Registry::build_fabric(table, clock)` — builds a full `Fabric`, registering every distinct
  `provider` slug a `RoleTable` references that is currently configured, skipping (not erroring
  on) any that isn't.

Run `tm provider detect` (add `--json` for machine-readable output) to see this list against your
own environment. `tm provider list` shows the role -> candidate routing from the current
project's `providers.toml` instead.

A `providers.toml` role candidate names any of the slugs below in its `provider` field:

```toml
[coder.fast]
candidates = [
    { provider = "openai", model = "gpt-4o-mini", max_concurrency = 20 },
]
```

Known gap: `Fabric::register_provider` keys its registry by provider slug alone, and every
backend below bakes in one fixed model at construction time. If two roles route to the same
`provider` slug under two *different* models, only one model actually gets registered and both
roles' traffic silently goes to whichever was registered last. `Registry::build_fabric` detects
this at build time and refuses to build (returns `ProviderError::InvalidRequest` naming both
models) rather than misrouting silently — it does not fix the underlying single-model-per-slug
limitation, which lives in `fabric.rs`.

## Anthropic

- Slug: `anthropic`
- Module: `crates/tm-provider/src/anthropic.rs` (predates the registry; no free tier)
- Env vars: `ANTHROPIC_API_KEY` (required)
- Base URL: `https://api.anthropic.com`
- Free tier: no
- Capabilities: completion, streaming, tool use

## OpenAI

- Slug: `openai`
- Module: `crates/tm-provider/src/providers/openai.rs`
- Env vars: `OPENAI_API_KEY` (required); `OPENAI_BASE_URL`, `OPENAI_ORGANIZATION`,
  `OPENAI_PROJECT` (optional)
- Base URL: `https://api.openai.com/v1`
- Free tier: no (new-account trial credit only, not a standing free tier)
- Capabilities: completion, embedding, streaming, tool use
- Known gap: reasoning-model families (`o1`/`o3`/`gpt-5`-style) require `max_completion_tokens`
  instead of `max_tokens`; the shared compat core always sends `max_tokens`, so those specific
  models are not correctly reachable through this backend yet (see the module's own doc comment).

## OpenRouter

- Slug: `openrouter`
- Module: `crates/tm-provider/src/providers/openrouter.rs`
- Env vars: `OPENROUTER_API_KEY` (required); `OPENROUTER_BASE_URL`, `OPENROUTER_HTTP_REFERER`,
  `OPENROUTER_APP_TITLE` (optional)
- Base URL: `https://openrouter.ai/api/v1`
- Free tier: yes — several `:free`-suffixed routed models
- Capabilities: completion, streaming, tool use

## GitHub Models

- Slug: `github-models`
- Module: `crates/tm-provider/src/providers/openrouter.rs`
- Env vars: `GITHUB_TOKEN` (required — a GitHub PAT or Actions token scoped for GitHub Models, or
  `GITHUB_MODELS_TOKEN`); `GITHUB_MODELS_BASE_URL` (optional)
- Base URL: `https://models.github.ai/inference`
- Free tier: yes — free within GitHub-imposed per-model rate limits
- Capabilities: completion, streaming, tool use

## Groq

- Slug: `groq`
- Module: `crates/tm-provider/src/providers/fast.rs`
- Env vars: `GROQ_API_KEY` (required); `GROQ_BASE_URL` (optional)
- Base URL: `https://api.groq.com/openai/v1`
- Free tier: yes — free developer tier with rate limits
- Capabilities: completion, streaming, tool use

## Cerebras

- Slug: `cerebras`
- Module: `crates/tm-provider/src/providers/fast.rs`
- Env vars: `CEREBRAS_API_KEY` (required); `CEREBRAS_BASE_URL` (optional)
- Base URL: `https://api.cerebras.ai/v1`
- Free tier: yes — free tier with daily request/token caps
- Capabilities: completion, streaming, tool use

## DeepSeek

- Slug: `deepseek`
- Module: `crates/tm-provider/src/providers/frontier.rs`
- Env vars: `DEEPSEEK_API_KEY` (required); `DEEPSEEK_BASE_URL` (optional)
- Base URL: `https://api.deepseek.com`
- Free tier: no (pay-as-you-go only)
- Capabilities: completion, streaming, tool use

## Mistral

- Slug: `mistral`
- Module: `crates/tm-provider/src/providers/frontier.rs`
- Env vars: `MISTRAL_API_KEY` (required); `MISTRAL_BASE_URL` (optional)
- Base URL: `https://api.mistral.ai/v1`
- Free tier: yes — a rate-limited free "La Plateforme" experiment tier
- Capabilities: completion, embedding, streaming, tool use

## xAI

- Slug: `xai`
- Module: `crates/tm-provider/src/providers/frontier.rs`
- Env vars: `XAI_API_KEY` (required); `XAI_BASE_URL` (optional)
- Base URL: `https://api.x.ai/v1`
- Free tier: no (new-account trial credit only)
- Capabilities: completion, streaming, tool use

## Together AI

- Slug: `together`
- Module: `crates/tm-provider/src/providers/serverless.rs`
- Env vars: `TOGETHER_API_KEY` (required); `TOGETHER_BASE_URL` (optional)
- Base URL: `https://api.together.xyz/v1`
- Free tier: yes — a handful of always-free serverless models
- Capabilities: completion, embedding, streaming, tool use

## Fireworks AI

- Slug: `fireworks`
- Module: `crates/tm-provider/src/providers/serverless.rs`
- Env vars: `FIREWORKS_API_KEY` (required); `FIREWORKS_BASE_URL` (optional)
- Base URL: `https://api.fireworks.ai/inference/v1`
- Free tier: yes — new-account free credit, not a standing free tier
- Capabilities: completion, embedding, streaming, tool use

## Hugging Face

- Slug: `huggingface`
- Module: `crates/tm-provider/src/providers/serverless.rs`
- Env vars: `HF_TOKEN` (required); `HF_BASE_URL` (optional)
- Base URL: `https://router.huggingface.co/v1`
- Free tier: yes — rate-limited free inference for many hosted models
- Capabilities: completion, streaming (no tool use through this router)

## Google Gemini

- Slug: `gemini`
- Module: `crates/tm-provider/src/providers/gemini.rs`
- Env vars: `GEMINI_API_KEY` (required, falls back to `GOOGLE_API_KEY` if unset);
  `GOOGLE_API_KEY` (optional fallback name); `GEMINI_BASE_URL` (optional)
- Base URL: `https://generativelanguage.googleapis.com/v1beta`
- Free tier: yes — a free tier with per-model rate limits
- Capabilities: completion, embedding, streaming, tool use

## Ollama (local)

- Slug: `ollama`
- Module: `crates/tm-provider/src/providers/local.rs`
- Env vars: `OLLAMA_HOST` (optional, all env vars optional — see caveat below)
- Base URL: `http://localhost:11434`
- Free tier: yes — runs entirely locally, no API key
- Capabilities: completion, embedding, streaming, tool use
- Caveat: since every one of this backend's env vars is optional, `ProviderInfo::is_configured()`
  always reports it as "configured", regardless of whether an Ollama server is actually reachable
  at the configured host. Autodetection here means "constructible", not "reachable"; confirming
  reachability needs an actual probe (`tm provider test`), not `Registry::autodetect()`.

## LM Studio (local)

- Slug: `lm-studio`
- Module: `crates/tm-provider/src/providers/local.rs`
- Env vars: `LM_STUDIO_HOST` (optional)
- Base URL: `http://localhost:1234`
- Free tier: yes — runs entirely locally, no API key
- Capabilities: completion, embedding, streaming, tool use
- Same "configured means constructible, not reachable" caveat as Ollama above.

## llama.cpp (local)

- Slug: `llama-cpp`
- Module: `crates/tm-provider/src/providers/local.rs`
- Env vars: `LLAMA_CPP_HOST` (optional); `LLAMA_CPP_API_KEY` (optional, only needed if
  `llama-server` was started with `--api-key`)
- Base URL: `http://localhost:8080`
- Free tier: yes — runs entirely locally, no API key required by default
- Capabilities: completion, embedding, streaming, tool use
- Same "configured means constructible, not reachable" caveat as Ollama above.

## Azure OpenAI

- Slug: `azure-openai`
- Module: `crates/tm-provider/src/providers/cloud.rs`
- Env vars: `AZURE_OPENAI_API_KEY` (required); `AZURE_OPENAI_ENDPOINT` (required, e.g.
  `https://<resource>.openai.azure.com`, no path); `AZURE_OPENAI_DEPLOYMENT` (required, the
  deployment name, not the underlying model id); `AZURE_OPENAI_API_VERSION` (optional, default
  `2024-10-21`)
- Free tier: no (Azure subscription required; some free credit programs exist but no standing
  free tier)
- Capabilities: completion, embedding, streaming, tool use

## AWS Bedrock

- Slug: `bedrock`
- Module: `crates/tm-provider/src/providers/cloud.rs`
- Env vars: `AWS_ACCESS_KEY_ID` (required); `AWS_SECRET_ACCESS_KEY` (required);
  `AWS_SESSION_TOKEN` (optional, for temporary credentials); `AWS_REGION` (required, e.g.
  `us-east-1`); `BEDROCK_MODEL_ID` (required, Bedrock's own model id string)
- Free tier: no (AWS subscription required)
- Capabilities: completion, streaming (no tool use, no embedding through this module)

## Google Vertex AI

- Slug: `vertex`
- Module: `crates/tm-provider/src/providers/cloud.rs`
- Env vars: `VERTEX_ACCESS_TOKEN` (required — a pre-minted OAuth2 bearer token; service-account
  token minting is not implemented by this module); `VERTEX_PROJECT` (required, GCP project id);
  `VERTEX_LOCATION` (required, e.g. `us-central1`)
- Free tier: no (GCP subscription required; GCP's general free-credit programs are not a
  Vertex-specific standing free tier)
- Capabilities: completion, embedding, tool use — **not** streaming (this module's `complete`
  always sends a non-streaming request; see the module's own doc comment for why)

## Cloudflare Workers AI

- Slug: `cloudflare`
- Module: `crates/tm-provider/src/providers/cloudflare.rs`
- Env vars: `CF_API_TOKEN` (required, scoped for Workers AI); `CF_ACCOUNT_ID` (required, part of
  the URL path)
- Base URL: `https://api.cloudflare.com/client/v4/accounts/<CF_ACCOUNT_ID>/ai/v1` (built from
  `CF_ACCOUNT_ID`, not overridable — there is no `CF_BASE_URL`)
- Free tier: yes — a daily free-usage allotment per Cloudflare account
- Capabilities: completion, embedding, streaming, tool use

## DevPass (generic OpenAI-compatible)

- Slug: `devpass`
- Module: `crates/tm-provider/src/providers/compat.rs` (`DevPassProvider`, a thin named wrapper
  over `CompatProvider::from_env_prefix`)
- Env vars: `DEVPASS_API_KEY` (required); `DEVPASS_BASE_URL` (required, no trailing slash);
  `DEVPASS_MODEL` (required — read directly from the environment, **not** from a
  `providers.toml` candidate's `model` field, since this backend has no per-candidate model
  parameter)
- Free tier: depends entirely on the operator-controlled gateway this points at
- Capabilities: completion, embedding, streaming, tool use
- `CompatProvider::from_env_prefix` itself (not registered under its own slug) is the generic
  half of this backend: any other bespoke OpenAI-compatible endpoint can reuse it with a new env
  var prefix without writing a new module.

### DevPass as the default provider

When `DEVPASS_API_KEY`, `DEVPASS_BASE_URL` and `DEVPASS_MODEL` are **all** set to non-empty
values, DevPass becomes the default provider for `coder.fast` — the one role `tm`'s interactive
session (bare `tm`, `tm -p <prompt>`) and its scheduler dispatcher (`tm run`/`tm sched run`)
actually drive an agent-loop turn as. Concretely:

- `RoleTable::default_table`'s `coder.fast` primary candidate becomes `devpass`/`$DEVPASS_MODEL`
  instead of `anthropic`/`claude-sonnet-5`; the existing Anthropic fallback candidate is left in
  place, untouched.
- `crates/tm-cli/src/agent.rs`'s `build_fabric` registers a `DevPassProvider` **instead of
  requiring** `ANTHROPIC_API_KEY` — this is the point of the feature: a real end-to-end `tm`
  session can run against a cheap/free OpenAI-compatible backend without touching Anthropic quota
  or needing an Anthropic account at all. `AnthropicProvider::from_env` is still attempted
  best-effort in this mode (registered if it happens to succeed, silently skipped if not), so any
  *other* role a ticket names still gets Anthropic service if a key also happens to be present.
- A **partial** set of the three env vars (e.g. only `DEVPASS_API_KEY`) does not activate this
  preference at all — it falls straight through to the unchanged, Anthropic-only default. There
  is no half-activated state.
- This does not touch any other role: `vision.frontier`/`planner.frontier`/`architect.frontier`
  (`tm genesis`'s bootstrap stages, resolved through a separate, Anthropic-only provider-selection
  path — see `crates/tm-cli/src/project.rs`'s `resolve_genesis_provider`) and `embedder`
  (background indexing) are all unaffected, on purpose: genesis's own resolution hardcodes
  `AnthropicProvider` regardless of what a candidate's `provider` slug names, so pointing a
  frontier role's default at `devpass` there would silently misconstruct a provider rather than
  actually use DevPass; embedding needs a model that actually serves embeddings, which a
  DevPass-testing model is not guaranteed to.
- Absent all three env vars, behavior is byte-for-byte unchanged from before this feature existed.

See `docs/decisions/D-004-devpass-default-provider.md` for the reasoning and what this does not
cover.

## Free-tier summary

| Slug | Free tier |
| --- | --- |
| `anthropic` | no |
| `openai` | no |
| `openrouter` | yes |
| `github-models` | yes |
| `groq` | yes |
| `cerebras` | yes |
| `deepseek` | no |
| `mistral` | yes |
| `xai` | no |
| `together` | yes |
| `fireworks` | yes (new-account credit) |
| `huggingface` | yes |
| `gemini` | yes |
| `ollama` | yes (local) |
| `lm-studio` | yes (local) |
| `llama-cpp` | yes (local) |
| `azure-openai` | no |
| `bedrock` | no |
| `vertex` | no |
| `cloudflare` | yes |
| `devpass` | depends on the operator's gateway |

Free-tier claims above are as of this document's writing and are not verified against each
vendor's live terms by this crate; treat them as a starting point for choosing a role's fallback
candidates, not a billing guarantee.
