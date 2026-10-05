+++
title = "Configuration"
weight = 2
description = "Where tm keeps its state and config: providers.toml, harness.toml, oversight.toml, hooks.toml, acp.toml, .env and TM_HOME."
+++

`tm` has two homes for configuration, and the split is deliberate.

- **Repo root**: human-authored files a team reviews together. These are version controlled.
- **State directory**: derived, per-checkout state: the event log, indexes and generated routing.

## Project state

`tm init` in a git repo creates `.tm/` (gitignored). Outside a repo, `tm` falls back to a global project
under `$TM_HOME/projects/` (default `$HOME/.tm`), and touches nothing in your working directory.

```sh
tm init              # create .tm/ in this directory
tm project           # show where this project's state lives
tm doctor            # invariants, hash-chain, index health, provider and permission probes
```

## Files

| File | Where | What it controls |
| --- | --- | --- |
| `providers.toml` | `.tm/` | which provider and model serves each role (`coder.fast`, `coder.deep`, ...) |
| `harness.toml` | `.tm/` | harness behavior; `tm harness set` stages a change and `tm harness promote` applies it |
| `oversight.toml` | repo root | action classes that need approval; see [Oversight](@/reference/oversight.md) |
| `hooks.toml` | repo root | commands run on events such as `session_start` and `Stop` |
| `acp.toml` | repo root | hand a role to an external ACP-speaking agent |
| `browser.toml` | repo root | headless browser settings |
| `.env` | current directory | environment variables, loaded automatically |

`oversight.toml`, `hooks.toml` and `acp.toml` are **trust-gated**: they are ignored in a fresh clone until
you run `tm trust`. See [the trust gate](@/reference/oversight.md#the-trust-gate).

## Providers in one minute

`tm init` writes a `providers.toml` that follows your environment: if `ANTHROPIC_API_KEY` is set,
`coder.fast` routes to Anthropic; if the `DEVPASS_*` trio is set, it routes to DevPass. A file you have
edited by hand is never touched. Regenerate it with `tm provider reset` (the old one is kept as
`providers.toml.bak`).

```sh
tm provider detect   # which of the 21 backends have credentials right now
tm provider list     # role -> candidate routing from providers.toml
tm provider test     # one tiny real (billed) completion through each
```

A role candidate names a provider slug and a model:

```toml
[coder.fast]
candidates = [
    { provider = "openai", model = "gpt-4o-mini", max_concurrency = 20 },
]
```

Pick a model in a session with `/model <provider>/<model>`. Every backend, its environment variables and
its free-tier status are listed in [Providers](@/reference/providers.md). If no usable provider is
configured, `tm doctor` says so and suggests the fastest zero-cost path (a local Ollama, or GitHub Models
with `GITHUB_TOKEN=$(gh auth token)`).

## Environment

| Variable | Effect |
| --- | --- |
| `TM_HOME` | where global state and the trust list live (default `$HOME/.tm`) |
| `TM_WEB_DIR` | the built web client for `tm serve` |
| `TM_NOTIFY=0` | no desktop notifications on approvals and escalations |
| `RUST_LOG` | `tracing` output on stderr, for example `RUST_LOG=tm_agent=debug` |
| `TM_AGENT_MAX_PROVIDER_CALLS` | stop a run before an additional provider call past this count |
