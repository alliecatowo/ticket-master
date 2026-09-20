# D-010 — `tm run <ticket> --worktree`: per-run `git worktree` isolation

**Status:** accepted · **Date:** 2026-09-20 · **Supersedes:** nothing

## Context

`docs/audit-2026-09-18-fable.md`'s M-04 ("Ecosystem table stakes") bundles `--worktree` on `tm run`
together with checkpoints/OTel/notifications/redaction as one set of items competitor agent
harnesses ship and this project did not. Before this change, `git.worktree` existed only as a raw,
agent-invocable tool (`crates/tm-agent/src/tools.rs`, gated as `GitOp::Branch`) — a ticket's own
executor *could* ask to create a worktree if it thought to, but nothing gave a delegated worker the
same isolation guarantee automatically, and every `tm run`/`tm sched run` dispatch executed directly
against the project's main checkout. `.claude/skills/dispatch-background-agent/SKILL.md` already
documents this exact concept for Claude Code's own subagents working on *this* repository
(`isolation: 'worktree'` is mandatory there); this decision is the product-level equivalent for a
real Ticketmaster-delegated worker operating on a user's real git repository.

Two real design questions had to be settled before writing any code:

1. **Where does an isolated run's `fs.*`/`edit.*`/`git.*`/`shell.*` tool calls actually resolve
   paths against?** Tracing `tm-agent::AgentLoop::drive`/`resume`, every tool call's
   `tm_types::CallContext::root` came from a module-private `project_root()` —
   `std::env::current_dir()`, the *process's* own cwd, not anything carried per-run through
   `tm_core::ExecutorTask`/the `Executor` trait. This is a real, pre-existing simplification
   (worth noting plainly, not silently building on top of): every `BuiltinExecutor`-driven run in
   this codebase today resolves file/git tool calls against wherever the `tm` process itself was
   started, not `Project::root`. `--worktree` needed a way to override that per run without either
   (a) mutating process-global `std::env::set_current_dir` for the run's duration — unsound the
   moment more than one dispatched run is live in the same process, which `tm sched run` already
   allows — or (b) widening `tm_core::Executor`/`ExecutorTask` (implemented by both
   `BuiltinExecutor` and `AcpExecutor`) with a new field every current and future adapter would
   have to plumb through.
2. **What does "cleaned up" mean for a run whose outcome isn't yet known when the ticket's own
   state looks finished?** `tm-scheduler::dispatch::ExecutorDispatcher::dispatch` returns as soon
   as a lease is acquired; the real run proceeds on a background task, and — critically — an
   executor that calls `Store::submit` mid-run (`tm-agent`'s `BuiltinExecutor` does, via its
   `ticket.submit` tool) can move the ticket to `Submitted` *before* `Executor::execute` itself
   returns, let alone before `tm_scheduler::dispatch::report_outcome` (patch storage, the D-008
   workspace snapshot, the executor's own session teardown) finishes reading the checkout. `tm
   run`'s existing polling loop (`RUN_TICKET_POLL_INTERVAL` = 500ms) only ever watched the
   ticket's own state — good enough when nothing downstream of `Executor::execute` returning still
   touches the filesystem, not good enough once cleanup itself is a filesystem operation on the
   same checkout.

## Decision

### `tm run <ticket> --worktree`

`crates/tm-cli/src/args.rs`'s `RunArgs` gains `--worktree: bool`. `crates/tm-cli/src/worktree.rs`
(new module) is the whole mechanism:

- **Preconditions, each a distinct named error** (task requirement: a clear error, not a confusing
  failure, outside a usable git repository): `project.scope` must be `Scope::Repo` (a repo-local
  `.tm/` must already exist — see "Where worktrees live" below for why), the workspace root must
  open as a real, non-bare `git2::Repository`, and `HEAD` must resolve to a real commit (an
  "unborn" HEAD — `git init` with zero commits — errors here instead of surfacing `git worktree
  add`'s own confusing "invalid reference: HEAD" stderr).
- **Where worktrees live:** `<project.state_dir>/worktrees/<ticket>-<pid>-<hex>/` — which, because
  `--worktree` requires `Scope::Repo`, is always exactly `<project.root>/.tm/worktrees/...`, the
  path the audit's own "done looks like" line names. Going through the already-resolved
  `state_dir` field rather than re-joining `.tm` onto `project.root` by hand is what keeps this
  honoring `docs/decisions/D-003-project-scope.md` (a global-scope project's workspace has no
  repo-local `.tm/` to nest a worktree under, and this must never silently create one — this also
  satisfies xtask's hygiene "no stray `.tm` literal" check by construction, not by allowlist).
  Gating on `Scope::Repo` in addition to "is a real git repo" is a deliberate narrowing: a
  workspace can be a real git repository while `tm` itself is still global-scoped (no `tm init`
  run there yet), and creating a `.tm/` as a side effect of `--worktree` in that case would be
  exactly the "auto-write `.tm/` into whatever directory happened to be current" product bug
  D-003 already fixed once.
- **The unique suffix** is `<pid>-<random_hex(6)>`, not a raw `IdSource::random_hex` draw alone:
  `CounterIds`'s RNG is seeded deterministically (`crate::project::open_at` calls
  `CounterIds::with_counters(counters, 0)` — seed `0`, always, in real usage) and *not* persisted
  across process restarts, only its counters are. Two separate `tm run --worktree` invocations
  that each happen to be the first caller of `random_hex` in their process would otherwise draw
  the identical hex string. Pairing it with the OS pid (`std::process::id()`, already this
  codebase's convention for a scratch path unique across processes — e.g.
  `tm-computer::linux.rs`'s test tempdirs) turns that into "would need a repeated pid *and* an
  identical draw," and neither `std::process::id()` nor `IdSource::random_hex` trips xtask's
  hygiene non-determinism check (`SystemTime::now`/`Instant::now`/`rand::thread_rng`/
  `rand::random`/`uuid::new_v4` are what it forbids).
- **Creation** is a real `git worktree add -b tm/run/<ticket>-<suffix> <path> HEAD`, shelled out
  (`std::process::Command`), matching `tm_scheduler::snapshot`'s own "why shell out, not `git2`"
  precedent (`docs/decisions/D-008-ticket-checkpoint-fork.md`) and `tm-agent`'s own
  agent-invocable `git.worktree` tool, which already does the same thing for the *opposite*
  direction (a ticket asking for a worktree of its own, from inside its own `fs.*`/`git.*` tool
  surface) — this decision does not touch that tool, it is the outer, CLI-level equivalent giving
  a whole run that isolation by default.
- **Cleanup** is `git worktree remove --force` (`--force` because the executor may have left
  untracked files a plain remove would refuse to delete), best-effort exactly like
  `capture_workspace_snapshot`: a failed cleanup is logged, never turned into a command failure.
  `git worktree remove` deletes only the checkout directory — **the branch it created is never
  deleted**, on a clean success or otherwise. This is deliberate, not an oversight: the branch,
  not the scratch directory, is the run's real durable output (its commits), so "removed" only
  ever means "the disk space is reclaimed," never "the work is gone" — `tm run`'s own success
  note names the branch for exactly this reason, and
  `crates/tm-cli/src/worktree.rs`'s isolation test asserts the commit is still reachable from the
  branch *after* `cleanup()` runs, not just before.

### Composing with the existing `Executor` abstraction, not bypassing it

`AgentLoop` (`tm-agent`) gains `root_override: Option<PathBuf>` plus a `with_root`/`root` builder
pair, mirroring the existing `with_oversight`/`oversight` shape M-16 already established. The two
production call sites that read `project_root()` (`resume`, `drive`) now read `self.root()`
instead — `root_override.unwrap_or_else(project_root)`, so the default (`None`) is byte-identical
to this loop's behavior before `--worktree` existed. `BuiltinExecutor` gains the matching
`root_override`/`with_root` builder and threads it into every `AgentLoop` it constructs in
`build()`. `AcpExecutor`'s config already carries an explicit `cwd: PathBuf`
(`AcpAgentConfig::cwd`) set once at construction, so no new field was needed there — only where
that value comes from.

`crates/tm-cli/src/dispatch.rs`'s `build_dispatcher` gains one parameter, `exec_root: Option<&Path>`
(`None` in every call site before this change, defaulting internally to `project.root`), which
reaches exactly three places and nowhere else:

1. `BuiltinExecutor::with_root` (when `exec_root != project.root`, i.e. a real override was asked
   for — not unconditionally, so "no override requested" and "override to `project.root`" stay
   distinguishable in the source, even though they currently behave the same).
2. `optional_acp_executor`'s `AcpAgentConfig::cwd` (the external agent's own working directory).
   `acp.toml` itself is still always read from `project.root` — it is human-authored,
   version-controlled project configuration, not something a per-run worktree carries its own
   copy of.
3. `ExecutorDispatcher::new`'s `repo_root` — the same field D-008's per-turn workspace snapshot
   already reads. This is a direct, positive side effect of this decision on that one: D-008's own
   "what this costs" section named exactly this gap ("a snapshot's attribution to one ticket is
   best-effort... a fully isolated-per-ticket snapshot would need per-ticket worktree isolation").
   A `--worktree` run's snapshot capture is now correctly scoped to just that ticket's checkout.

Deliberately **not** touched: `ProjectContextPackSource`'s `root`/`state_dir` (retrieval —
`CodeIntel`'s code-intelligence index) and `project.code_intel()`. A ticket's context pack is
compiled from the main checkout's index regardless of `--worktree`; re-scoping `CodeIntel::open_at`
per run would churn a persisted index under `state_dir` for no benefit (the worktree starts
identical to `HEAD`, and its own edits are exactly what the run is producing, not something it
needs to retrieve). This means retrieval and execution can diverge only if the main checkout has
uncommitted changes at dispatch time — an accepted, narrow caveat, not a bug.

### Two runs racing dispatch and cleanup, not just two runs racing files

`ExecutorDispatcher::dispatch` returns as soon as a lease is acquired; the run itself proceeds on a
background task, and `Store::submit` (via `BuiltinExecutor`'s `ticket.submit` tool) can move the
ticket to `Submitted` before that background task's own `report_outcome` (patch storage, the D-008
snapshot, session teardown) is done reading the checkout. So `ExecutorDispatcher` gains
`dispatch_with_completion`, additive next to `dispatch` (which now delegates to a shared private
`dispatch_inner`, unchanged for every existing caller/test): it returns the same `Vec<Event>` plus
a `tokio::sync::oneshot::Receiver<()>` that resolves only after the spawned task's *entire* body —
`run_and_report`, including `report_outcome` — has finished. `run_ticket` (`tm run`) uses this
overload only when `--worktree` is set; `sched_run` (`tm sched run`) is untouched and still calls
plain `dispatch`.

### The cleanup policy: kept unless a success is *confirmed*

`run_ticket`'s epilogue, once its existing state-polling loop exits:

- **Detached** (the loop hit `RUN_TICKET_MAX_WAIT` while the ticket was still `Leased`/`Running`):
  always **kept**. The background run may still be using the checkout; removing it would be
  actively unsafe, not just conservative.
- **Not detached**: await the completion receiver, bounded by a 30s grace period (generous —
  nothing downstream of `Executor::execute` returning does network I/O — but bounded, so a stuck
  background task cannot hang `tm run` forever). If that resolves *and* the ticket's own final
  state is one of `Submitted`/`Verifying`/`Auditing`/`Closed` (real forward progress, not merely
  "no longer `Leased`/`Running`" — a `RetryScheduled` failure moves the ticket back to `Ready`,
  which is not on this list), the worktree is **removed**. Otherwise — `Ready` after a scheduled
  retry, `Escalated`, `Cancelled`, `Rework`, `Replan`, or the completion receiver itself timing
  out — it is **kept**, and `tm run` prints its path plus the `git worktree remove --force`
  command to remove it by hand.
- **`dispatch_with_completion` itself returning `Err`** (no executor registered for the ticket's
  role, a lease-acquisition race, ...): the worktree is removed immediately, before the error
  propagates. Nothing ever ran against it — keeping an empty, never-touched checkout around would
  make "a kept worktree means there is something to inspect" a false statement.

This is a real product decision, not a default that fell out of the implementation: the asymmetric
default (keep unless success is *confirmed*, not remove unless failure is confirmed) mirrors this
same harness's own worktree convention (a Claude Code subagent's worktree "will be cleaned up
automatically if you made no changes, or preserved for review if you did") — evidence a human
would want to inspect is worth more disk space than tidiness, and a project that accumulates a few
kept worktrees under `.tm/worktrees/` after real failures is a much smaller problem than a project
that silently deleted the one piece of state that would have explained why a delegated run went
wrong.

## Why

- Overriding `AgentLoop`'s root via a builder (mirroring the already-established `with_oversight`
  pattern) rather than widening `ExecutorTask`/the `Executor` trait keeps every current and future
  executor adapter (`AcpExecutor`, and whatever B-12 adds next) unaffected by this change's
  existence — only `BuiltinExecutor`, which actually needed it, grew a new constructor seam.
- `dispatch_with_completion` as an additive method next to `dispatch` (both delegating to one
  private `dispatch_inner`) means every existing call site (`tm sched run`, `tm-scheduler::driver`,
  every existing test) is unaffected — `tm run --worktree` is the only caller that pays for the
  extra channel.
- Gating on `Scope::Repo` in addition to "is a real git repository" is what lets the worktrees
  directory be spelled as `state_dir.join("worktrees")` — reusing the already-D-003-sanctioned
  field — instead of hand-rolling `.tm` resolution a second time outside `crates/tm-cli/src/
  project.rs`, the one file xtask's hygiene check designates for that.
- The `<pid>-<random_hex>` suffix is the honest fix for a real determinism gap this decision's own
  investigation found (`CounterIds`'s RNG reseeding to the same fixed seed on every real process
  start): a smaller, targeted convention rather than either ignoring the collision risk or
  widening `tm_types::IdKind` with a new variant for something that isn't a domain identifier.

## What this costs, stated plainly

- **This does not fix `AgentLoop`'s pre-existing `project_root()` limitation for every other
  caller.** Without `--worktree`, `tm run`/`tm sched run` still resolve every tool call against
  the process's own current directory, not `Project::root` — a latent gap this decision's own
  investigation surfaced but did not repair (repairing it generally would need `ExecutorTask`
  itself to carry a root, the wider change this decision deliberately avoided for `--worktree`'s
  own sake — see "Composing with the existing `Executor` abstraction" above). A `tm run` invoked
  from outside the project root without `--worktree` behaves exactly as it did before this change,
  warts included.
- **The cleanup policy is a real, load-bearing judgment call, not a derived fact.** A ticket that
  ends in `Rework`/`Replan` (sent back for a bounded fix or child regeneration, not strictly a
  "failure") still keeps its worktree under this policy, on the reasoning that "confirmed forward
  progress" is a narrower, safer bar than "not an outright failure" — a future session that wants
  a different line should treat that as a real product decision to revisit, not a bug to
  silently patch.
- **`tm init` does not write a `.gitignore` entry for `.tm/`** (checked directly:
  `crates/tm-cli/src/project.rs` never touches `.gitignore` at all), so in a real user's repo
  `.tm/worktrees/<ticket>-<suffix>/` is untracked content sitting *inside* the main working tree —
  `git status` goes dirty the moment a worktree is created, and stays dirty for as long as it is
  kept (the common case: every retry, escalation, and detach keeps one). This is not new
  (`.tm/project.db`/`artifacts/` were already untracked, unignored content before this change);
  `--worktree` just adds more of it, and more visibly, since a kept worktree is a whole checkout,
  not one sqlite file. `crates/tm-cli/src/worktree.rs`'s own tests only pass a "main checkout
  untouched" assertion because their fixture repos add `.gitignore` with `.tm/` by hand — a real
  project without one does not get that for free. Making `tm init` write `.gitignore` itself is a
  separate, plausibly-already-in-flight change (not this decision's scope, and not something this
  change should reach into another track's territory to add).
- **`.tm/worktrees/` is not swept by anything today.** A project that runs `tm run --worktree`
  repeatedly against tickets that never cleanly submit accumulates kept worktrees indefinitely;
  there is no `tm worktree prune`/equivalent GC verb yet. `git worktree remove --force <path>` by
  hand (the exact command `tm run` itself prints) is the only cleanup path right now.
- **The end-to-end test coverage is deliberately decomposed, not one single "full agentic run"
  test.** A true model-turn-to-commit `tm run --worktree` integration test needs a provider;
  `TM_TEST_MOCK_PROVIDER=1` (this codebase's existing out-of-process test hook,
  `crates/tm-cli/src/agent.rs`) only scripts a single text-only turn that never calls a tool, so it
  cannot itself prove "the executor's file edits land on the worktree's branch." That claim is
  instead proven in three narrower, real pieces: `crates/tm-cli/src/worktree.rs`'s own tests
  (a real `git worktree add`/`remove` lifecycle, plus a test that commits a file *inside* the
  worktree and asserts it is invisible to both the main checkout's working tree and the main
  branch's own history); `crates/tm-agent/src/executor.rs`'s new `with_root` tests (a constructed
  `AgentLoop`'s `root()` really is the override, not silently the process cwd); and
  `crates/tm-cli/tests/worktree_run.rs` (the real compiled `tm` binary, `--worktree` end to end
  through argument parsing into a real worktree, and the "kept, not removed" policy exercised for
  real against the scripted non-submitting mock turn). No single test currently exercises "a real
  tool call, inside a real `tm run --worktree` process, commits to the worktree branch" in one
  pass — the three pieces above compose to the same guarantee without a live model.
- **`crates/tm-cli/tests/worktree_run.rs` activates its test ticket by calling `tm_core::Store::
  activate` directly**, not through any `tm` CLI verb — there is none today (`tm ticket new`
  leaves a ticket in `Draft`; nothing in `crates/tm-cli/src` ever calls `Store::activate`, only
  `tm-server`'s `TransitionRequest::Activate` route and test code do). This is a real, pre-existing
  gap this decision's own test-writing surfaced, orthogonal to `--worktree` itself and out of this
  change's scope to fix — noted here so it is not lost.
