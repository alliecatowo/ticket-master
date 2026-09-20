# D-004 — DevPass as the default provider for interactive turns

**Status:** accepted · **Date:** 2026-09-19 · **Supersedes:** nothing

## Context

`crates/tm-provider/src/providers/compat.rs`'s `DevPassProvider` already existed as a thin,
fully env-driven (`DEVPASS_API_KEY`/`DEVPASS_BASE_URL`/`DEVPASS_MODEL`) wrapper over a generic
OpenAI-compatible backend, meant for zero-signup/cheap testing without touching real Anthropic
quota. Nothing preferred it: `RoleTable::default_table()` hardcoded `provider: "anthropic"` for
every role unconditionally, with no environment-awareness.

Tracing the actual runtime path for a real `tm` invocation (not just `default_table()` in
isolation) surfaced more layering than the surface request implies:

- `default_table()`'s role→candidate mapping is **not** the sole determinant of which provider or
  model a real interactive turn actually calls. `crates/tm-cli/src/agent.rs`'s own `build_fabric`
  — shared by the interactive/scriptable session (`tm`, `tm -p <prompt>`) and by the scheduler
  dispatcher (`tm run`/`tm sched run`, via `crates/tm-cli/src/dispatch.rs`) — historically
  constructed exactly one hardcoded `AnthropicProvider` (fixed model `AGENT_MODEL =
  "claude-sonnet-5"`, fixed slug `"anthropic"`) and registered only that, regardless of what
  `default_table()`'s candidates named. `Fabric::execute` routes a role to whichever *registered*
  provider matches the table's chosen candidate's `provider` slug; if the table names a slug that
  was never registered, that role hard-fails with `"provider not registered: <slug>"`.
  Consequently, changing only `default_table()` to prefer `devpass` for `coder.fast` — the
  role `AGENT_ROLE` binds the interactive/scheduler turn to — would have been either inert (if
  `build_fabric` still only ever registered `anthropic`) or an active regression (a hard routing
  failure for `coder.fast` the moment the three env vars were set, worse than today's behavior).
- `tm genesis`'s bootstrap stages (`vision.frontier`/`planner.frontier`/`architect.frontier`) go
  through a wholly separate resolution, `crates/tm-cli/src/project.rs`'s
  `resolve_genesis_provider`, which reads a candidate's `provider`/`model` fields into a
  `ModelId` but then unconditionally constructs an `AnthropicProvider` from it regardless of what
  the slug actually names — a latent bug, harmless today only because `default_table()` has
  always named `anthropic` for those roles. Pointing one of them at `devpass` would trigger that
  bug for real (an `AnthropicProvider` tagged with a DevPass `ModelId`, gated on
  `ANTHROPIC_API_KEY`, calling the wrong API entirely). Left unfixed here; noted as a real,
  separate defect.
- `tm provider list`/`status`/`test` (`crates/tm-cli/src/ops.rs`) resolve providers from a
  project's on-disk `harness.toml`, not from `default_table()` at all — a distinct config surface
  this change does not touch.

## Decision

Two coordinated, narrowly-scoped changes, gated on the same single check
(`DevPassProvider::preferred_model()`: `Some(model)` iff all three `DEVPASS_*` vars are set and
non-empty, `None` otherwise — a partial set never half-activates anything):

1. `RoleTable::default_table()` prefers `devpass`/`$DEVPASS_MODEL` as `Role::CoderFast`'s
   **primary** candidate when active; its existing Anthropic fallback candidate, and every other
   role's candidates, are left untouched. The construction itself is split into the pure
   `default_table_with(Option<&str>)` plus a thin env-reading `default_table()` wrapper, so the
   ~20 call sites across the workspace (several themselves tests) can't be contaminated by a test
   mutating real env state — only one function anywhere touches the real `DEVPASS_*` vars for
   this purpose ([`DevPassProvider::preferred_model`]), and it is the only thing tested against
   real env mutation, under its own lock.
2. `crates/tm-cli/src/agent.rs`'s `build_fabric` registers a `DevPassProvider` **instead of
   requiring** `ANTHROPIC_API_KEY` when active — the actual point, since the whole motivation is
   running a real `tm` session without an Anthropic credential. `AnthropicProvider::from_env` is
   still attempted best-effort in this branch (registered if it happens to succeed, silently
   skipped otherwise), so a ticket naming a different role still gets Anthropic service if a key
   happens to also be present; only `coder.fast`'s own default candidate actually moves.

Scope is deliberately just `Role::CoderFast`: it is the one role concretely, verifiably used to
drive an agent-loop turn for both the interactive session and the scheduler dispatcher
(`AGENT_ROLE` in `agent.rs`). Genesis's frontier roles are excluded because of the latent bug
above; `embedder` is excluded because it is background/non-interactive and because a
DevPass-testing model is not guaranteed to serve embeddings at all.

## Why

The alternative — changing only `default_table()`, as the literal ask could be read in isolation
— looks additive but is not: without also changing `build_fabric`, setting the three env vars
would actively break `coder.fast` (a routing failure) rather than doing nothing, since
`default_table()`'s candidate and `build_fabric`'s registered provider set would then disagree on
which slug serves that role. Fixing both together is the only way this is genuinely additive:
absent the env vars, every observable byte of behavior (the `RoleTable` and the constructed
`Fabric`) is identical to before.

## What this costs, stated plainly

- `build_fabric`'s DevPass branch swallows an `AnthropicProvider::from_env` failure silently
  (`if let Ok(..)`) rather than surfacing it — deliberate (surfacing it would defeat "no Anthropic
  credential needed"), but it means a misconfigured `ANTHROPIC_API_KEY` in DevPass mode fails
  silently for any non-`coder.fast` role rather than at `build_fabric` construction time; it
  surfaces later, if at all, as that role's own `"provider not registered: anthropic"` at
  execution time.
- The genesis-frontier-role latent bug (`resolve_genesis_provider` ignoring a candidate's
  `provider` slug) is documented here but not fixed. Extending DevPass preference to those roles
  in the future requires fixing that function first, not just adding a `default_table_with` arm.
- `tm provider list`/`status`/`test` still describe `harness.toml`'s on-disk candidates, not the
  live default; a project with a `harness.toml` on disk sees no change from this decision at all
  regardless of `DEVPASS_*`, since that path never consults `default_table()`.
