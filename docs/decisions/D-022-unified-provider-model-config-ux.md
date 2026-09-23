# D-022: Unified provider/model/config UX (levels, defaults, honest errors)

Status: accepted
Date: 2026-09-23
Supersedes: none (extends D-005's DevPass default, D-021's chat commands)

## Context

External research (Claude Code, OpenCode, Pi, Hermes, Goose, Aider, Codex/Gemini CLIs)
converges: one `/connect` flow, `provider/model` defaults in one file with
global→project override, tiered models (fast vs deep + thinking levels), env-var keys
never shown. Our audit found the opposite: `build_fabric` hardcoded to Anthropic with a
DevPass override (an ollama-only user got "anthropic key not set"); `/model` listed two
providers max; no persisted default; three vocabularies (`auth`/`provider`/`harness`);
`harness.toml` parsed as two incompatible schemas by two commands; `harness set`
validates but never writes while `promote` reads disk; `init` scaffolds no settings;
CLI monochrome by design; workers execute in the launcher cwd instead of the project.

## Decision

1. **Generic fabric** (`agent.rs::build_fabric`, done): default table + appended fallback
   rows for every configured, tool-capable backend (`Registry::chat_default_model`,
   `env_default_model`, `RoleTable::with_fallback_candidate`), best-effort registration
   of the rest, first-registered preference when the primary is dead, plain-language
   error naming `/connect`/`tm auth`/`tm provider detect` when nothing is live.
2. **Turns respect the project table**: `build_fabric_for(table, clock)` core;
   `build_fabric(clock)` wrapper keeps default-table behavior for project-less callers.
   Session path loads `<state_dir>/providers.toml` (new, `RoleTable`) → legacy
   `harness.toml`-as-roles → default. `harness.toml` stays `HarnessConfig`-only.
3. **`tm provider default [<spec>|clear]`** persists `<state_dir>/default-model.json`
   (`{"model": "provider/model"}`); resolution is session choice → file → table primary;
   the `/model` picker marks it.
4. **`/level fast|deep`** switches the chat role (`CoderFast`/`CoderDeep`), persisted in
   the session JSON (backward-compatible default). Reasoning-effort knobs are a non-goal:
   no provider exposes them uniformly.
5. **One vocabulary**: `ready`/`not-configured`/`local (unprobed)` in chat and CLI alike;
   `tm provider status` reads the same project table as `list`.
6. **`init` scaffolds, `set` explains**: `init` writes `harness.toml`
   (`HarnessConfig::default`) + `providers.toml` (serialized default table); `harness
   show` renders defaults with a note when missing; `set` scaffolds-then-validates and
   keeps its validate-only shape, stated in its own output (fixing epoch persistence is
   out of scope).
7. **CLI color**: tty-gated (pipes stay clean), honors `NO_COLOR`/`CLICOLOR`/
   `CLICOLOR_FORCE`, table headers styled. TUI unchanged.
8. **Dispatch cwd**: worker tool execution runs at the project root, never the launcher
   cwd (the showcase leak), covered by a test.

## What this costs, stated plainly

- `chat_default_model` literals rot as vendors rename models; `/model <slug>/<name>`
  always overrides, and a failure names the fix. A live catalog (models.dev-style) is
  the real answer and is out of scope (network + caching design of its own).
- `providers.toml` vs `harness.toml`: two files because one filename already meant two
  schemas; `list` keeps legacy-harness fallback so existing projects don't break.
- Local backends register without picker rows (no honest static model): reachable via
  explicit `/model <slug>/<pulled-model>`, with `/connect` printing how to list tags.
- `codex-chatgpt` stays invisible to detect/list (no `info()`, D-016's adapter is
  session-based) — a separate wiring task.
- Linux CI stays red (`tm-computer` doesn't compile there): unrelated, pre-existing.
