# D-032 — `tm bench run --live`: a genuine ticket run instead of a replayed script

**Status:** accepted · **Date:** 2026-09-25 · **Supersedes:** nothing

## Context

`tm bench run`'s default path (`tm-cli/src/ops.rs`'s `FixtureScriptProvider`, `tm-harness/src/
bench.rs`'s `BenchRunner`) replays a fixed, on-disk transcript per task: deterministic and free,
but it can only prove that a *scripted* transcript still attests the task's predicate — it never
actually asks a worker to do the task. `bench-swe-lite-fixture-ingestion` already vendored a
handful of real bug-fix fixtures with a `test_command`, marked "live-only" (skipped by the
scripted runner, since they carry no `script.txt`) specifically so a future live mode had
something real to run. This decision adds that mode.

## Decision

**`--live`** (`BenchRunArgs::live`, `tm-cli/src/args.rs`) switches `tm bench run` from
`FixtureScriptProvider` to `crate::bench_live::LiveSeededProvider` — an implementation of
`tm_harness::SeededProvider`, so it still runs through the same `BenchRunner::run_all` every
other `tm bench run` invocation does; only the provider that decides what a task's one "step"
means changes.

For each matched task, `LiveSeededProvider::step` (called exactly once per task —
`is_finished` reports done after step 0, since a live task has no further scripted turns):

1. Copies `bench/<fixture.path>` into a fresh scratch directory under the OS temp dir (never the
   primary checkout, never a `git worktree` of it — a bench fixture is not a checkout of this
   repo, so `D-012`'s worktree isolation doesn't apply; a plain directory copy is per-task
   isolation enough).
2. Gives that directory a real git history (`git init` plus one commit), so the dispatcher's
   code-intelligence history ingest has a valid `HEAD`.
3. Runs `fixture.setup_commands` there, failing the task outright if one exits non-zero.
4. Creates a worker ticket for `task.task` (`Authority::worker()`, the same defaults
   `tm ticket new`/`create_worker_ticket` use) in a fresh, throwaway project opened at that
   scratch directory, activates it, and drives it to completion through
   `crate::sched::run_ticket` — the same dispatcher/executor wiring `tm run` itself uses, never a
   reimplementation of it — recording a cassette (`--record`) into the *invoking* project's own
   `.tm/bench/live/<task-id>.cassette.jsonl`, alongside where that run's report lands.
5. Runs `fixture.test_command` (when set) in the scratch directory and reports
   `tests_pass:<suite>` in the step's returned text only when it exits zero — the one thing
   `BenchRunner`'s predicate evaluation looks for a `Predicate::TestsPass` leaf.
6. Folds the scratch project's own event log through `tm_harness::metrics::
   ticket_metrics_from_events` and stashes the real dollars/tool-calls/tokens for that task id.

After `BenchRunner::run_all` returns, `LiveSeededProvider::apply_live_metrics` overwrites each
live-run `TaskResult`'s `cost_micros`/`tool_calls`/`context_bytes` with those real numbers —
`BenchRunner::run`'s own accounting (`cost_micros`/`tool_calls`/`context_bytes` derived from the
scripted output's byte length and step count) is a proxy with no meaning once a task is actually
run. `context_bytes` carries the real `tokens_in + tokens_out` for a live task: `TaskResult` has
no dedicated tokens field, and adding one would mean changing `tm-harness`'s own public report
shape for every other caller (`tm bench compare`, `tm bench report`, promotion gating) — this
took the narrower path instead. `score` itself is left exactly as `BenchRunner::run` computed it;
recomputing it would mean duplicating `tm-harness`'s own (private) per-dimension scoring weights
in `tm-cli`, and `passed` — decided purely by the task's predicate against the step's returned
text — is what a live run is actually proving.

Provider selection is unchanged from every other `tm run`-shaped path:
`crate::dispatch::build_dispatcher_with_recording` (which `run_ticket --record` already uses)
resolves `TM_TEST_MOCK_PROVIDER` first, then the project's configured role table
(`build_fabric`/`build_fabric_for_project`) — `LiveSeededProvider` does not touch provider
selection itself, so `tm bench run --live` is offline-testable under
`TM_TEST_MOCK_PROVIDER=1` exactly like `tm run`.

**`bench/tasks/live-smoke.toml`** is a purpose-built task for this: `test_command = ["true"]`
(always exits zero) with no `script.txt` in its fixture, so it proves the `--live` plumbing
itself (ticket created, activated, run to completion; cassette recorded; metrics folded) rather
than a worker's ability to fix anything, and — carrying `test_command` with no scripted steps —
is silently skipped by the default, non-live path exactly like every other live-only fixture.

Async bridging: `SeededProvider::step` is synchronous (`tm-harness` has no async runtime
dependency by design), but driving a ticket to completion is inherently async
(`sched::run_ticket`). `LiveSeededProvider` cannot call `Handle::block_on` from inside `step`,
because `step` runs on the same worker thread already driving `tm bench run --live`'s own async
task — exactly the nested-runtime panic `sched.rs`'s `dispatch_sched` doc comment already
describes for `tm sched run`. Instead, `step` spawns a plain OS thread (`std::thread::scope`,
never `std::thread::spawn`, so borrowed `&Project`/`&Renderer` references don't need `'static`),
builds a fresh, throwaway multi-thread Tokio runtime there, and blocks on it — a thread with no
ambient Tokio context to collide with.

## Why

- **A real result, not just a replay of one.** The scripted path is the right default (free,
  deterministic, fast enough for every `verify` run to afford), but it cannot answer "does a
  worker actually solve this task" — only `--live` can, and only `--live` was ever going to be
  able to.
- **Reuse `run_ticket`, don't reimplement it.** Every other "make a ticket actually do
  something" path in this crate (`tm run`, `tm sched run`, the TUI's in-process runner) goes
  through the same dispatcher/executor construction; a bench-specific reimplementation would be a
  second copy of that wiring to keep in sync, and would silently diverge from what `tm run` itself
  does the moment either one changed.
- **Per-task isolation via a scratch directory, not a git worktree.** `D-012`'s worktree
  isolation exists for a ticket running against *this* repository's own checkout; a bench
  fixture is an unrelated, vendored directory, so a plain `mktemp`-style copy plus its own throwaway
  `.tm/` project is the right unit of isolation here, matching this repo's own rule that ad hoc
  runs never touch the primary checkout.

## What this costs, stated plainly

- **Real cost, real time, real non-determinism.** `--live` calls a real (or `TM_TEST_MOCK_
  PROVIDER`-mocked) model per task; unlike the scripted path, two runs of the same epoch can
  score differently. This is why it stays opt-in and is never part of `mise run verify`.
- **`score` is stale relative to the real numbers `apply_live_metrics` just wrote in.** Because
  recomputing it would require duplicating `tm-harness`'s private scoring internals in `tm-cli`,
  a live task's `score` still reflects `BenchRunner::run`'s byte-length-of-output proxy, not the
  real cost/tool-call numbers sitting next to it in the same `TaskResult`. A future track that
  wants live scoring to be internally consistent should move that recomputation into
  `tm-harness` itself, where the weights already live.
- **Only `Predicate::TestsPass` is meaningfully live-checkable today.** `step`'s returned text is
  the whole channel `BenchRunner`'s predicate evaluation reads from, and a live task's own
  worker's edits land in a scratch directory the evaluator never inspects — a `FileExists`/
  `FileMatches`/`CommandSucceeds` predicate against a live task would need `step` to go re-check
  the scratch directory's real filesystem state itself and fold that into the returned text. Only
  `TestsPass` (via `test_command`'s real exit code) is wired up; the existing swe-lite fixtures'
  own predicates are `TestsPass` already, so this is not a gap for them, but a future live
  fixture using a different predicate kind would need that support added first.
- **A scratch directory and its throwaway `.tm/` project are left on disk per task run**,
  matching this repo's other ad hoc scratch dirs — `mise run disk:guard` reclaims
  `tm-bench-live-*` directories on its normal schedule, same as every other `tm-*` scratch dir.
- **No `--worktree`-style cleanup-on-success.** A `--live` run's scratch directory is never
  removed automatically even on a clean pass, unlike `tm run <ticket> --worktree`'s worktree.
  That asymmetry is deliberate for now (a bench run's scratch state is disposable by
  construction, not something worth inspecting after a clean pass the way a real ticket's
  worktree is), but it does mean disk pressure from a large `--live` suite run is real, not
  theoretical.
