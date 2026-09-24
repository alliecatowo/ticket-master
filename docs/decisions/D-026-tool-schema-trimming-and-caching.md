# D-026 — Tool-schema trimming and Anthropic prompt caching

**Status:** accepted · **Date:** 2026-09-23 · **Supersedes:** nothing

## Context

`docs/backlog.md:617-620` records roughly 175k tokens per small turn and about 18KB of tool
schemas resent on every single step, which dominates both benchmark cost/score and every real
ticket run's budget — the direct cause of the "4 agents burning 200k tokens each" pattern the
owner flagged as unwanted. SPEC §30.1 ("admit by authority, not by availability") and §30.2
("every layer pays rent") already name the two levers: send fewer tool schemas per request, and
mark the stable prefix (system prompt + tool schemas) as prompt-cache-eligible wherever the
configured provider supports it.

## What was already in place

Auditing `crates/tm-agent/src/tools.rs` and `crates/tm-agent/src/agent_loop.rs` before touching
anything found lever 1 fully built, just missing one test that names the acceptance check's exact
authority pair:

- `ToolRegistry::admitted`/`tool_defs_for(&Authority)` (tools.rs) already filters by both the
  owning `CapabilityProvider::requires()` and each `ToolSchema::requires()` against the given
  authority — a denied tool is absent from the request's `tools` list entirely, not merely
  rejected at dispatch. `AgentLoop::drive` (agent_loop.rs:912) already calls
  `tool_defs_for(&effective_authority)`, not the untrimmed `tool_defs()`.
- Extensive existing coverage (`tool_defs_for_root_authority_admits_every_tool`,
  `tool_defs_for_shell_disabled_omits_every_shell_shaped_tool_entirely`,
  `..._write_disabled_...`, `..._read_disabled_...`, `..._none_authority_admits_nothing`, plus a
  computer-authority block further down) already asserted omission-not-denial for several
  authority shapes.
- This repo has no `Authority::admin()` constructor (only `root()`, `worker()`, `none()`), so the
  acceptance check's literal wording is unsatisfiable as written. `tool_defs_for_worker_authority_
  omits_tools_root_authority_admits` (new, tools.rs) fills the one gap: it asserts
  `Authority::worker()` — which SPEC's own doc comment says grants "no desktop control" — omits
  `computer.click`'s schema entirely while `Authority::root()` admits it, and that worker's total
  admitted tool count is strictly smaller than root's.

Lever 2 (provider-side caching) was **not** built, despite looking built at a glance:
`crates/tm-provider/src/anthropic.rs`'s `build_headers` already sent the
`anthropic-beta: prompt-caching-2024-07-31` header unconditionally, and `crates/tm-agent/src/
agent_loop.rs` already carried a `PromptCacheState { system_prompt_fingerprint }` field with a
`cache_state()` getter — but nothing ever wrote to `self.cache`, and the Messages API request body
itself carried no `cache_control` breakpoint on any content block. Anthropic only caches a prefix
when a `cache_control` marker sits on the block ending it; the beta header alone enables the
feature, it does not request it. So despite the header, zero tokens were ever actually cached —
`cache_read_tokens`/`cache_write_tokens` (already modeled in `tm-provider`'s `Usage`) would have
stayed zero on every real call.

## Decision

1. **Tool trimming**: no code change; the existing `tool_defs_for`/`admitted` mechanism already
   does what §30.1 asks. Only the acceptance-check-shaped test was added (see above).
2. **Prompt caching**: `crates/tm-provider/src/anthropic.rs`'s `build_wire_request_with_names`
   now marks the **last tool definition** in the request with `cache_control: {"type":
   "ephemeral"}` (new `WireTool::cache_control: Option<CacheControl>` field). Anthropic's Messages
   API caches everything up to and including the block carrying the marker, so one breakpoint on
   the last tool covers the whole tool-schema block — the part of the request most likely to be
   byte-identical across a ticket's consecutive steps, since it depends only on `effective_
   authority` and `ToolRegistry::standard`'s registration, not on per-step conversation state.
   `system` stays a single concatenated `String` (Anthropic's Messages API also accepts a system
   block array with its own `cache_control`, which would let the system prompt be cached too, but
   that requires changing `WireRequest::system`'s wire shape and is left as a follow-up rather
   than risking the request-shaping tests that assert on the current string form).
3. This is a minimal, surgical `anthropic.rs` change reported via this batch's `shared_edits`
   (that file is outside this task's assigned file list, `crates/tm-agent/src/tools.rs` +
   `crates/tm-provider/src/fabric.rs` + this doc) rather than skipped, because the actual
   provider-side caching gap SPEC §30.2 asks about lives entirely in the wire-shaping layer:
   `crates/tm-provider/src/fabric.rs`'s `execute`/`execute_priced` already pass a
   provider-agnostic `CompletionRequest` straight through to `provider.complete(req)` with no
   marking of any kind (correctly — the wire-independent `CompletionRequest` has no concept of a
   provider-specific cache breakpoint), so there was no way to land this lever without touching
   the one file that maps `CompletionRequest` onto Anthropic's actual wire JSON.
4. `PromptCacheState`/`AgentLoop::cache_state()` are left as-is: they are consulted by no caller
   today, and wiring them into a real "skip resending an identical system prompt" decision would
   require deciding whether that's still correct once `system` also needs a cache marker
   (follow-up, not blocking this task's acceptance).

## Before/after measurement

**Not run in this batch.** This task's environment forbids `cargo build`/`cargo test`/running the
real `tm` binary (per this batch's own no-build-or-test instruction), so no real multi-step
ticket run's `cache_read_tokens` could be captured before or after this change. What can be stated
without running anything:

- **Tool-schema bytes per step**: `ToolRegistry::tool_surface_cost_for(&Authority)` (already
  existed) computes exactly the byte/token cost `tool_defs_for` would send. The method to measure
  the lever-1 saving on a real project: `tool_surface_cost_for(&Authority::root())`'s summed bytes
  (the old, untrimmed baseline — `AgentLoop` never actually sent `tool_defs()` in production, but
  it's the right "no trimming" comparison point) minus `tool_surface_cost_for(&effective_
  authority)`'s summed bytes for a real ticket's worker/reviewer authority, repeated per step of a
  scripted multi-step run.
- **Cache-read tokens per step**: once lever 2 lands, `Completion::usage.cache_read_tokens` on
  step 2+ of any multi-step run against the real Anthropic API (this repo's mock provider always
  reports zero, so this genuinely needs a live call, not a unit test) should be nonzero and close
  to the cached tool-schema block's token count, confirmed via `anthropic-beta`'s actual billing
  behavior (Anthropic bills cache reads at roughly 10% of the base input-token rate) — this is the
  real token-burn reduction §30's backlog entry asks to see, and is the natural first thing to run
  by hand with `mise run test:live-codex-auth`-style live-provider access once that's available in
  this environment, or by inspecting `tm events` after a real `tm run <ticket>` against Anthropic.

## What this costs, stated plainly

- The `cache_control` breakpoint only helps when consecutive steps' tool lists are byte-identical.
  If `effective_authority` narrows mid-run (a real, supported case — see `agent_loop.rs`'s
  authority-narrowing comments near `AgentLoop::drive`), the tool block changes and the cache
  entry misses on the next step, same as an ordinary uncached request; this is a correctness-safe
  degradation (never a stale/wrong cache), just not a savings guarantee on every turn.
- The system prompt itself is still sent uncached every step; only the tool-schema block benefits
  from this change. Caching the system prompt too needs `WireRequest::system` to become a block
  array, which is out of scope here (see "Decision" item 2).
- No non-Anthropic provider gets anything from this: `crates/tm-provider/src/providers/` also has
  `openai.rs`, `gemini.rs`, `cloudflare.rs`, `openrouter.rs`, etc., none of which were touched.
  Several OpenAI-compatible gateways cache identical prefixes automatically server-side with no
  request-shape change required, which is a real mitigant but was not verified against any
  specific provider's actual behavior in this batch.
- The measurement §30.2 asks for ("An unattributed context is a bug") is still not run against a
  live provider — see "Before/after measurement" above. This decision doc states the method, not
  a number; closing that gap is the natural next step once a live-provider run is available in
  this environment.
