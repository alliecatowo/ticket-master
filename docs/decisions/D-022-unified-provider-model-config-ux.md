# D-022: Provider and chat-model configuration UX

Status: accepted (implemented scope and known gaps recorded below)
Date: 2026-09-23
Supersedes: none (extends D-005 and D-021)

## Context

The provider overhaul exposed mismatches between the role table, registered backends, chat model
selection, and setup status. In particular, registering a local backend is not evidence that a
model is available, and a provider's presence in one role does not make it a candidate for every
role. Chat must report an effective route rather than imply that a backend can answer.

## Decision

1. **Generic fabric** (`agent.rs::build_fabric`, done): default table + appended fallback
   rows for every configured, tool-capable backend (`Registry::chat_default_model`,
   `env_default_model`, `RoleTable::with_fallback_candidate`), best-effort registration
   of the rest, first-registered preference when the primary is dead, plain-language
   error naming `/connect`/`tm auth`/`tm provider detect` when nothing is live.
2. **Turns respect the project table**: one CLI loader resolves `<state_dir>/providers.toml`
   (new, `RoleTable`) → legacy role-shaped `harness.toml` → default. Interactive sessions,
   `tm run`/`tm sched run`, provider list/status/test, and context-pack compilation all use this
   effective table; project-less callers use the built-in default. `harness.toml` stays
   `HarnessConfig`-only for newly initialized projects.
3. **`tm provider default [<spec>|clear]`** persists `<state_dir>/default-model.json`
   (`{"model": "provider/model"}`); resolution is session choice → file → table primary;
   the `/model` picker marks it.
4. **`/level fast|deep`** switches the chat role (`CoderFast`/`CoderDeep`), persisted in
   the session JSON (backward-compatible default), and reports an actionable error instead of
   silently falling back to another role when the selected one has no configured route.
   Reasoning-effort knobs are a non-goal: no provider exposes them uniformly. `/model` lists
   candidates across every role, not just the session's current one.
5. **A shared status source, not yet one identical vocabulary**: `tm provider status` reads the
   same project table as `list`. The CLI's `tm provider list`/`detect` render the full
   `tm_provider::Availability` (`ready`/`not-configured`/`unreachable` — a local backend's probe
   found nothing listening — and `unusable`, either a cloud backend whose credentials exist but
   cannot yet construct a usable provider at all, e.g. Bedrock before SigV4 signing exists, or a
   local backend that answered the probe with no model pulled yet). The chat's own
   `/connect` picker is a simpler, non-probing three-way status (`local, not checked`/
   `configured`/`not configured`) that never distinguishes `unusable` from `configured` — a known
   gap, not parity with the CLI's fuller `Availability`.
6. **`init` scaffolds, `set` explains**: `init` writes `harness.toml`
   (`HarnessConfig::default`) + `providers.toml` (serialized default table); `harness
   show` renders defaults with a note when missing; `set` scaffolds, validates, and persists a
   candidate configuration. `promote` applies the new harness epoch.
7. **CLI color**: tty-gated (pipes stay clean), honors `NO_COLOR` and `CLICOLOR=0`, table
   headers styled. `CLICOLOR_FORCE` is not yet honored (known gap below). TUI unchanged.
8. **Dispatch cwd**: worker tool execution runs at the project root, never the launcher
   cwd (the showcase leak), covered by a test.

### Integration clarification

The persisted path is `tm init` → edit `providers.toml` / `harness.toml` → select a chat model
with `/model` or `tm provider default` → start a new session or dispatch a ticket. Harness changes
from `tm harness set` are persisted as a candidate and take effect after `tm harness promote`. The
model default is project-scoped in `default-model.json`; the provider role table is shared by new
sessions and background workers. Provider status and list derive routes from the same loader. Local servers are
constructible without credentials, but their model names are not guessed: choose one explicitly.

## Known gaps

- Built-in role tables still contain provider/model defaults, and the provider registry still has
  static model defaults for some remote services. They can become stale; no live catalog is
  queried.
- Local model discovery/probing and choosing a pulled model from `/model` without first configuring
  a role route are not implemented.
- CLI color is only effective where output paths apply the renderer's color support. This decision
  does not claim every human table is colored; JSON and redirected output remain uncolored, and
  `CLICOLOR_FORCE` (forcing color onto a non-tty) is not yet handled.
- A full connection test is separate from environment configuration. Environment presence alone
  does not prove network reachability or account validity.

## What this costs, stated plainly

- Fast/deep is a role-table choice, not a provider-independent reasoning-effort control.
- Statically configured model IDs require maintenance, and a configured endpoint can still be
  unavailable or reject its model.
- Local backends need explicit model configuration; tm intentionally refuses to invent a name.
