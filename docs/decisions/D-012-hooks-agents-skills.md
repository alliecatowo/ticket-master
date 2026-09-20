# D-012 — AGENTS.md conventions, SKILL.md progressive disclosure, and shell-only hooks.toml

**Status:** accepted · **Date:** 2026-09-20 · **Supersedes:** nothing

## Context

`docs/audit-2026-09-18-fable.md`'s M-04 ("Ecosystem table stakes") named three items as entirely
unimplemented, sharing the same tool-dispatch/context-pack layer:

- `AGENTS.md` read as a `SectionKind::Conventions` source, walking up from a ticket's claimed
  paths. `SectionKind::Conventions` already existed (last in `PRIORITY_ORDER`) but nothing
  populated it beyond an explicit caller-supplied `Vec<String>` every real call site passed as
  `&[]`.
- `SKILL.md` discovery under `.tm/skills/**`, metadata-only in the pack, body loaded on demand by
  a `skill.load` tool — the same progressive-disclosure shape this repo's own `CLAUDE.md`
  documents Claude Code itself using for skills.
- Hooks as a `hooks.toml` with `PreToolUse`/`PostToolUse`/`UserPromptSubmit`/`Stop`/
  `SessionStart` handlers that can `deny`/`rewrite`/`allow`, invoked inside
  `ToolRegistry::dispatch` before `Authority::permits`.

This doc covers all three together because they land in the same review, not because they are one
mechanism — each is independently useful and independently testable.

## Decision

### AGENTS.md → Conventions

`tm_context::sections::build_conventions(ticket, ci, extra)` (was `build_conventions(extra)`) now
computes each claimed path's starting directory (its own directory when the path names a real
directory with no wildcard; otherwise its parent, or the wildcard prefix's parent), walks that
directory up to `ci.project_root()` inclusive, and reads every `AGENTS.md` found along the way,
root-to-leaf, deduplicated across paths that share an ancestor. Content is appended after
`extra` (the existing explicit-conventions list, kept for a future `providers.toml`-shaped source
and to keep `compile_reports_no_drops_when_nothing_overflows`'s existing "use tabs" assertion
meaningful). No `compile()` signature change: `ci: &CodeIntel` was already a parameter, and
`CodeIntel::project_root()` was already a public accessor.

### SKILL.md discovery and `skill.load`

`tm_context::skills` discovers every `SKILL.md` under `<root>/.tm/skills/**` (recursive, symlinks
skipped, depth-bounded), parsing a minimal hand-rolled `key: value` frontmatter block (not a real
YAML parser — see that module's doc comment) for `name`/`description`. `discover_skills` returns
metadata only; `load_skill(root, name)` returns the full body, called by a new `skill.load` tool.
`sections::build_conventions` folds every discovered skill's `name`/description into the
Conventions section (never a body).

`skill.load` is **not** a new `tm_agent::tools::ToolName` variant. That private enum's cardinality
is load-bearing: `ToolRegistry::standard`'s own doc comment names the tests that assert exact
admitted-tool counts against `ToolName::ALL.len()` (`standard_registers_every_tool_name_exactly_once`,
several `tool_defs_for_*` `- N` counts), and a `grep -n "ALL.len()\|39"` over `tools.rs`/
`capability.rs` turned up seven `ALL.len()`-derived assertions plus four "39-tool" prose mentions —
well past the two-or-three-site threshold worth absorbing into one enum. `SkillCapability`
(`crates/tm-agent/src/skill_capability.rs`) is instead a standalone `CapabilityProvider`, the exact
seam `tm-browser`'s `BrowserCapability` and `tm-computer`'s `ComputerCapability` already use for
this reason (`docs/audit-2026-09-18-fable.md` A-01/B-02), registered via `ToolRegistry::
with_capabilities`'s `extra` argument at both real call sites: `tm-cli`'s `AgentSession::
run_turn_streaming` and `tm-agent`'s `BuiltinExecutor::build` (the scheduler-dispatched path —
`tm run`/`tm sched run` get `skill.load` too, not just the interactive session).

`to_action` maps `skill.load` to `Action::ReadPath { path: ".tm/skills/<name>/SKILL.md" }` —
"the closest existing governance switch, a safe (denies-by-default) choice rather than an exact
semantic match", the same phrase this module's own doc comment already uses for `ticket.comment`/
`decision.record`/`git.worktree`'s equivalent gaps. `.tm/skills/**` sits outside a ticket's
ordinary source-tree resource claims, so a project that wants skills broadly reachable needs to
grant read authority over `.tm/skills/**` explicitly (or run under `Authority::root()`, as both
current call sites' human/root-authority paths do); this is a known, narrow limitation, not a
bypass. `to_action` is pure per its trait contract, so it cannot resolve a skill name against
`discover_skills`'s real (possibly nested) path the way `invoke` does at call time; it assumes the
primary one-directory-deep layout, which only ever makes the authority check *stricter* than a
correctly-scoped grant needs, never looser.

### hooks.toml — shell-only, not "shell or in-process"

The audit's own phrasing names both as acceptable ("shell or in-process") and explicitly invites
picking the smaller cut. `tm_agent::hooks::HookConfig` supports shell hooks only: each configured
entry is `argv` (`command: Vec<String>`, spawned directly — `["/bin/sh", "-c", "..."]` for an
inline script), matched against a tool name via an optional `matcher` (exact string or `"*"`/
absent for "every tool"). An in-process hook would need a Rust trait object (or an embedded
scripting language) configured from TOML — real new surface for a feature this workspace has zero
callers of yet. A shell hook is one well-understood contract, trivially testable with
`/bin/sh -c '...'`, no temp file or executable bit required.

**Wire contract:** a hook process receives one JSON object on stdin —
`{"event": "PreToolUse" | "PostToolUse" | "UserPromptSubmit" | "SessionStart" | "Stop", ...}`
(`tool`/`input` for the two tool-scoped events, `prompt` for `UserPromptSubmit`, `session`/
`ticket` throughout where applicable). It may write nothing (silently `allow`), or a JSON object
on stdout: `{"decision": "allow" | "deny" | "rewrite", "reason": "...", "updated_input": <object>,
"updated_prompt": "..."}` — `updated_input` rewrites a tool call's input, `updated_prompt`
rewrites a submitted prompt; `reason` becomes the denial detail a model or human sees.

**Fail-closed.** A hook that cannot be spawned, times out (10s), or exits non-zero is treated as
`Deny` — never silently `Allow`. This matches the workspace's existing deny-by-default `Authority`
model: a broken hook script blocks the calls it is configured against rather than silently
granting them. A clean exit (status 0) with empty or non-JSON stdout is `Allow` — a hook whose only
job is a side effect (a log line, a notification) is not required to speak the structured
contract.

**Where each event fires, and why the split:**

- `PreToolUse`/`PostToolUse` are evaluated inside `tm_agent::tools::ToolRegistry::dispatch`
  (a new `hooks: Option<HookConfig>` field, set via a `with_hooks` builder so every existing
  constructor/call site is unaffected by default). `PreToolUse` runs first, before `to_action`/
  `Authority::permits` — it can deny, rewrite the call's input (threaded through subsequent
  matching entries, first deny short-circuits), or allow. `PostToolUse` runs after `invoke`
  completes and is **observational only**: the call already happened, so nothing it returns is
  consumed as a decision; a failure is logged, never surfaced as a denial of something already
  completed.
- `UserPromptSubmit`/`SessionStart`/`Stop` are not tool-scoped at all, so they don't belong inside
  `dispatch`. Per the task's own scoping, they fire at `tm-cli`'s `AgentSession`'s lifecycle
  points instead (`crates/tm-cli/src/agent.rs`): `SessionStart` once at the top of
  `run_interactive`'s loop and once at the top of `run_prompt` (the `-p` one-shot form never
  reaches the interactive loop, so it needs its own firing point); `UserPromptSubmit` at the top
  of `run_turn_streaming`, before ticket resolution, with deny/rewrite honored (a deny returns
  `AgentOutcome::Failed { class: FailureClass::Other, .. }`, not an `Err` — an in-band agent
  outcome, not an infrastructure failure, matching that method's own documented contract); `Stop`
  at the end of `run_turn_streaming`, since that single method is what `run_turn`, `run_prompt`
  and the TUI's turn driver all call through, so one wiring point covers every front end. Only
  `SessionStart`/`Stop` are observational; `UserPromptSubmit` is the third decision-consuming
  event, alongside `PreToolUse`.
- `hooks.toml` is also loaded and attached in `tm-agent`'s `BuiltinExecutor::build` (the
  scheduler-dispatched path), for `PreToolUse`/`PostToolUse` only — that method's signature is
  infallible (`Executor::execute`'s trait contract), so unlike `tm-cli`'s interactive path (which
  fails the turn loudly on a malformed `hooks.toml`, mirroring `load_oversight`'s existing
  precedent for `oversight.toml`) a parse failure here logs a warning and falls back to no hooks
  configured rather than requiring a broader signature change to fix "properly".

**Required test, verified:** a real `hooks.toml`-shaped `PreToolUse` hook (`/bin/sh -c 'printf
"{\"decision\":\"deny\",...}"'`) that denies a specific tool call actually blocks it through the
real `ToolRegistry::dispatch` path (`dispatch_pre_tool_use_hook_denies_a_matching_call`), and one
that allows does not (`dispatch_pre_tool_use_hook_allow_does_not_block`) — both in
`crates/tm-agent/src/tools.rs`, exercising the real dispatch method against `Authority::root()` so
a `Denied` outcome can only be the hook, not authority. `crates/tm-agent/src/hooks.rs`'s own test
module covers the same deny/allow/rewrite/matcher/fail-closed contract at the `HookConfig` level
directly, plus `UserPromptSubmit`/`SessionStart`/`Stop`.

## What this costs, stated plainly

- **`.tm/skills/**` authority is a real, if narrow, rough edge.** A ticket scoped to
  `crates/tm-foo/**` read authority cannot call `skill.load` today without a project explicitly
  widening that grant to include `.tm/skills/**` — there is no bypass, by design (see "closest
  existing governance switch" above), but it does mean skills are not "just usable" for a
  narrowly-scoped worker out of the box the way the Conventions section's metadata listing implies
  they might be.
- **Hook timeouts and fail-closed-on-broken-hook are a real availability tradeoff.** A `hooks.toml`
  with a buggy `PreToolUse` entry (wrong path, missing interpreter, an unhandled exception before
  it ever writes JSON) blocks every matching tool call until someone fixes the config — not a
  silent pass-through. This is the intentional, safer default, but it means a hooks misconfiguration
  is a hard stop, not a warning.
- **PostToolUse/SessionStart/Stop cannot deny anything, by design.** If a future need arises for
  `Stop` to block ending a turn (real Claude Code's own hook system supports exactly this), that is
  a new, separate mechanism this change deliberately does not build — the audit's own "obvious
  lifecycle points" framing did not ask for it, and threading a denial back out of an
  already-in-flight `run_turn_streaming` call is materially more invasive than the observational
  wiring landed here.
- **The SKILL.md frontmatter parser is not YAML.** A `SKILL.md` using anything beyond flat
  `key: value` scalars (nested maps, lists, multi-line strings) for `name`/`description` will not
  parse as intended — acceptable for two fields, not a general-purpose frontmatter reader.
- **`AGENTS.md` discovery re-reads the filesystem on every `compile()` call** (once per claimed
  path's ancestor chain), with no caching — consistent with every other section builder's "pure
  function of `(ticket, view, ci)`" contract (`CodeIntel`'s own index is likewise consulted fresh
  per call), but a project with many claimed paths sharing few ancestors pays a proportional
  number of small `read_to_string` calls per pack compile.
