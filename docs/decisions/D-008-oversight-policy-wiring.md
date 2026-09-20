# D-008 — `oversight.toml`: loading and wiring `Oversight::review` at a real effect boundary

**Status:** accepted · **Date:** 2026-09-20 · **Supersedes:** nothing

## Context

`docs/audit-2026-09-18-fable.md`'s M-16 (part of M-04's "Ecosystem table stakes" review):
`Oversight` (`crates/tm-types/src/action.rs`) — the policy mapping an action class to "ask a
human first" — has existed since before this audit, with a conservative default
(`Oversight::conservative()`) and a full contract (`Oversight::review(&self, action, base:
Decision) -> Decision`: never softens a `Deny`, escalates an `Allow` to `NeedsApproval` when the
action's class matches `approval_required` by exact name or dotted-prefix, or when an
`Action::Spend` exceeds `spend_over_micros`). Every existing test exercised that contract
directly; nothing in the workspace loaded `oversight.toml` from disk, and nothing called
`Oversight::review` at a point where a tool call was actually about to dispatch. The struct's own
doc comment said so explicitly.

Two questions had to be settled before writing any code:

1. **Where does `Authority::permits` sit relative to `Oversight::review`, and where does the
   suspend-for-approval machinery already live?** `Authority::permits(&self, action) -> Decision`
   (`crates/tm-types/src/authority.rs`) never itself returns `NeedsApproval` — only `Allow`/
   `Deny` — a fact both `crates/tm-agent/src/tools.rs::ToolRegistry::dispatch` and
   `crates/tm-acp/src/permission.rs::evaluate` already document defensively. The suspend/resume
   machinery already exists in full: `crates/tm-agent/src/agent_loop.rs`'s `drive` calls
   `self.tools.to_action(name, input)` then checks the result against `effective_authority.
   permits(&action)` for `Decision::NeedsApproval` *before* ever calling `self.tools.dispatch`
   — appending an `approval.requested` event (`ApprovalRequestedPayload`, the same event kind
   D-007's desktop notifications already watch for) and returning
   `AgentOutcome::AwaitingApproval`, which `AgentLoop::resume` later re-drives once a human
   decides. This is real, already-tested machinery — `crates/tm-cli/src/agent.rs`'s interactive
   session and `crates/tm-cli/src/tui.rs` already consume `AwaitingApproval`/call `resume`. The
   only thing missing was a caller that could ever *produce* `Decision::NeedsApproval` in the
   first place: `Authority::permits` structurally cannot (it has no `Oversight` to consult), so
   that `if let Decision::NeedsApproval` branch in `drive` was dead code until this track.
2. **Where does `oversight.toml` live — a project's `root` or its `state_dir`?** D-003 (one day
   before the audit that flagged M-16) formalized the split: `root` is the git workspace —
   version-controlled, human-authored configuration a team reviews together (`browser.toml`,
   `templates.toml`, and B-12's `acp.toml`, all resolved against `project.root`) — while
   `state_dir` (`<root>/.tm` in repo scope, gitignored) is durable-but-derived project state
   (`harness.toml`, `mirror.toml`, `project.db`, ...). The audit's own M-16 recommendation
   ("parse `.tm/oversight.toml`") predates D-003 and used the pre-split mental model where
   everything lived under `.tm`. `oversight.toml` is squarely the first kind: a human-authored
   security policy a team wants in code review and `git log`, not per-checkout derived state that
   would silently differ between two clones of the same repository — the same reasoning D-003
   itself gives for `browser.toml` staying at `root`. `acp.toml`, added after D-003, already
   chose `root` for exactly this reason.

## Decision

- **`oversight.toml` resolves against `project.root`, not `state_dir`.** A new
  `crates/tm-cli/src/dispatch.rs::load_oversight(&Project) -> Result<Oversight>` reads
  `project.root.join("oversight.toml")`, parses it with `toml::from_str::<Oversight>` (the whole
  file *is* an `Oversight` value — `approval_required`/`spend_over_micros` at the top level, no
  wrapping table, since `Oversight` already derives `Deserialize` with the right shape), and
  returns `Oversight::default()` (identical to `Oversight::autonomous()` — asks nothing) when the
  file is absent. This mirrors `crate::drive::optional_browser_wiring`'s and this same module's
  `optional_acp_executor`'s "absent config file means today's behavior, unchanged" shape exactly.
- **`AgentLoop` gained an `oversight: Oversight` field**, defaulting to `Oversight::autonomous()`
  in `AgentLoop::new` (so every existing call site keeps its exact prior behavior unless it opts
  in) and settable via a new builder, `AgentLoop::with_oversight`, following the same pattern as
  `with_max_steps`/`with_max_events_per_ticket`. `AgentLoop::drive`'s tool-dispatch loop now
  computes `self.oversight.review(&action, effective_authority.permits(&action))` instead of
  calling `permits` alone — the one-line change that turns the loop's already-built
  `NeedsApproval` branch from dead code into the real gate. Because `review` only ever escalates
  an `Allow` (never softens a `Deny`), this composes safely with the existing authority check
  without changing what an authority denial looks like.
- **`BuiltinExecutor` (`crates/tm-agent/src/executor.rs`) gained an `oversight: Oversight`
  constructor parameter**, threaded into every `AgentLoop` it builds via `.with_oversight(..)` —
  the same "always present, never `Option`" shape `ComputerWiring` already uses (a project with
  no `oversight.toml` still has a concrete, autonomous policy, not a branch to skip).
- **Two real callers, both real effect boundaries, both wired the same way:**
  `crates/tm-cli/src/dispatch.rs::build_dispatcher` (the `tm run`/`tm sched run` path, via
  `BuiltinExecutor`) and `crates/tm-cli/src/agent.rs` (bare `tm`'s interactive session and
  `tm -p <prompt>`, which builds its own `AgentLoop` directly) both call `load_oversight` and
  thread the result through. `load_oversight` is `pub(crate)` specifically so the second call
  site can reuse it instead of a second loader growing independently.
- No new event kind, no new suspension mechanism: `Decision::NeedsApproval` continues to produce
  exactly the `approval.requested` event / `AgentOutcome::AwaitingApproval` / `AgentLoop::resume`
  flow that already existed and was already consumed by `tm-cli`'s interactive session, the TUI,
  and (transitively, via the event) D-007's desktop notifications.
- **`Oversight` gained `#[serde(deny_unknown_fields)]`.** A human-edited `oversight.toml` is a
  security control, not a display-only config: before this, a typo'd key (e.g.
  `approval_requird`) would silently deserialize into an *empty* policy — the file's author would
  believe an action class was gated when nothing was. This matches `BrowserToml::parse`'s own
  `validate()` step and `tm-harness/src/config.rs`'s documented "strict validation," and is
  covered by both a type-level test (`crates/tm-types/src/action.rs`) and a loader-level one
  (`crates/tm-cli/src/dispatch.rs`).

## Why

- Reusing the existing `NeedsApproval` → `approval.requested` → `AwaitingApproval` → `resume`
  path is strictly additive: zero new event kinds, zero new CLI surface, zero changes to how a
  human already answers a pending approval. The only thing this track adds is a real producer of
  `Decision::NeedsApproval`.
- Chaining `Oversight::review` immediately after `Authority::permits` (rather than, say, having
  `permits` itself grow an `Oversight` parameter) keeps the two concerns exactly as separate as
  `crates/tm-types/src/action.rs`'s own tests already assumed (`oversight_never_softens_a_denial`
  particularly) — authority answers "is this possible at all", oversight answers "does a human
  need to see it first", and the existing contract that a denial is final was never in question.
- Root, not `state_dir`, keeps `oversight.toml` in the same category as the two TOML configs
  added since D-003 (`acp.toml`, `browser.toml`): reviewable in a pull request, identical across
  every clone of the repository, and never silently different from what `git log` shows for it —
  the property that actually matters for something whose entire job is "a human already decided
  this needs another human."

## What this costs, stated plainly

- **Only the two `AgentLoop`-driving call sites in `tm-cli` are wired.**
  `crates/tm-acp/src/permission.rs::evaluate` — the *client*-side check this workspace applies to
  an external ACP-speaking agent's tool calls — still only calls `Authority::permits` and, per its
  own doc comment, treats a hypothetical `NeedsApproval` as a `Deny` rather than escalating,
  because it is a synchronous callback with no suspension mechanism to escalate through. Wiring
  `Oversight` there for real would mean giving that callback an async path to a pending-approval
  state analogous to `AgentOutcome::AwaitingApproval`, which is a real architecture change this
  track did not make. Today this is a difference in fidelity, not correctness: with no
  `oversight.toml` configured (the only case this track's defaults exercise), that path's behavior
  is unchanged either way.
- **`Oversight::conservative()`'s `git.push`/`git.merge`/`git.force_push` entries still gate
  nothing reachable through this loop.** `crates/tm-agent/src/tools.rs` only exposes
  `git.status`/`git.diff`/`git.log`/`git.commit`/`git.branch`/`git.worktree` as agent-invocable
  tools (its own doc comment: "the only real `git push` in this workspace happens inside
  `tm-mirror::Tracker`, wrapped separately by `tm-cli`'s `mirror_push` call site") — a pre-existing
  gap this track neither created nor closed. A project that wants approval before a commit lands
  should name `git.commit` explicitly in `approval_required`, which this track's own regression
  test (`run_suspends_for_approval_when_oversight_requires_it_for_the_dispatched_action`) exercises
  directly, precisely because `conservative()`'s own default set does not include it.
- **`ApprovalRequestedPayload::note` is the bare action class string** (e.g. `"git.commit"`, or
  `"spend over 10000000 micros"`), whatever `Oversight::review`'s existing contract already
  produced — no new human-facing copy was written for this, so a CLI/TUI rendering it verbatim is
  exactly as terse as it was in every test that predates this track.
- **`spend_over_micros` is parsed and tested, but not enforceable through this boundary today.**
  `Oversight::review`'s `Action::Spend` branch only ever fires for an `Action::Spend` value, and
  nothing in `crates/tm-agent/src/tools.rs`'s `to_action` maps any agent-invocable tool call to
  `Action::Spend` — spend is tracked separately, by `AgentLoop::record_usage`/`Budget::try_spend`
  debiting a running total, never as a discrete dispatched action. So a real `oversight.toml`
  setting this field parses correctly (this track's own test asserts that) and is inert at the
  wired effect boundary: nothing ever calls `review` with an `Action::Spend`. Making it real would
  need a per-call spend estimate turned into an `Action::Spend` and checked before the call that
  would exceed it — the same "refuse to start what it cannot finish" shape `SPEC.md` §31.2/audit
  B-10 already gives budget headroom, just not routed through `Oversight` yet. Out of this track's
  scope; noted here so the field does not read as more wired than it is.
- **Two `AwaitingApproval` consumers that predate this track, and were therefore never exercised
  against a real `NeedsApproval` before now, are live as of this change** (still only when a
  project actually configures `oversight.toml` — the default remains a no-op): `tui.rs`'s turn
  driver auto-denies every suspension (`Ok(false)`, with a note streamed into the pane — "the TUI
  does not support interactive approval yet"), and `agent.rs`'s plain/`-p` loop blocks on a real
  stdin read (`prompt_approval_decision`) that already, by design, treats EOF as a denial rather
  than hanging — the right behavior for a scripted `tm -p` run piped from a closed or exhausted
  stdin, and unchanged by this track, just newly reachable. Both fail closed, which is the safe
  direction to have been wrong in by default; neither got a UI improvement here.
