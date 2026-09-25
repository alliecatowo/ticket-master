# D-033 — A cross-tool benchmark harness: `tm` vs. opencode vs. Codex vs. Claude Code

**Status:** accepted · **Date:** 2026-09-25 · **Supersedes:** nothing

## Context

`docs/backlog.md`'s "A head-to-head benchmark: opencode vs. Codex vs. Claude Code vs.
Ticketmaster" section asked for this once its own stated precondition was met — a solid standalone
coding loop (D-003) and a zero-additional-cost credential (DevPass) to run it under without
quietly burning real API budget. Both are now true. The point, per that section, is not a
leaderboard for its own sake: running the same task set through all four tools on a schedule (or
after a significant harness/architecture change) is how a regression in *our own* product gets
caught before a person notices it by hand.

`bench-live-seeded-provider` (D-032) already gave `tm bench run --live` a genuine, real-ticket-run
path for `tm` itself; `bench-swe-lite-fixture-ingestion` vendored the hermetic bug-fix fixtures in
`bench/tasks/*.toml`/`bench/fixtures/*` this harness reuses unmodified; `bench-report-render` gave
`tm bench report` its markdown-table shape, which this harness's own render function follows for a
comparable look. This decision is the harness that drives the *same* fixture set through more than
one tool and puts the results next to each other.

## Decision

**`cargo xtask bench-cross [--tools tm,opencode,codex,claude] [--task <filter>] [--out <dir>]
[--model <provider/model>] [--task-timeout <secs>] [--max-cost-usd <amount>]
[--real-claude-auth]`** (`crates/xtask/src/bench_cross.rs`, wired into `crates/xtask/src/main.rs`;
`mise run bench:cross` is the `mise.toml` alias) — an `xtask` subcommand, not a `tm` subcommand,
since it drives `tm` itself as one of several *external* processes rather than running inside a
single project the way every other `tm bench` verb does.

The mechanism is one function, `run_comparison`, parameterized over a `ToolAdapter` trait so the
loop never special-cases `tm`:

1. Parse every `bench/tasks/*.toml` (via `tm_harness::BenchTask::parse`, reused as-is — no
   second task-file format invented).
2. For each `(tool, task)` pair: copy the task's fixture into a fresh, per-tool, per-task scratch
   directory under the run's own `--out` dir, give it a real git history (`git init` plus one
   commit — every one of the four tools expects a real repo), and run the fixture's own
   `setup_commands`.
3. Hand the task to that tool's `ToolAdapter::run`, timing the call.
4. Independently re-run the task's own `test_command` in the scratch directory afterward for
   pass/fail — never trust a tool's own self-report for the one number this harness must get
   right, matching `tm bench run --live`'s own `run_live_task` design (D-032).
5. Score the result with the task's own `ScoringSpec`/`ExpectedOutcome`, using the same
   ceiling-based subscore formula `tm_harness::bench`'s (private) `BenchRunner::run` uses,
   necessarily duplicated here — see "What this costs" below.

Four `ToolAdapter` implementations:

- **`TmAdapter`** shells the real `tm` binary: `tm init` first (`ticket dispatch`, unlike `ticket
  new`, needs an *existing* project — `tm-cli/src/main.rs`'s `dispatch` only auto-bootstraps one
  for `ticket new`, and that bootstrap is global-scope, not rooted at the scratch directory, so it
  would leave the ticket unable to see the fixture at all), then `tm ticket dispatch <task>` (draft
  → ready in one step), `tm run <ticket>`, then `tm stats --by ticket --ticket <ticket> --json` for
  the real `tm_harness::metrics::TicketMetrics` numbers `tm` itself already folds from that
  project's event log (D-030) — this *is* "store results as real ticket and event-log state in a
  scratch project" for the `tm` arm: no separate bookkeeping invented, just the CLI surfaces that
  already exist. Credentials follow whatever that scratch project's own environment and
  `providers.toml` resolve — DevPass when `DEVPASS_API_KEY`/`DEVPASS_BASE_URL` are exported in the
  calling environment, per D-005's `coder.fast`-role pattern, but this adapter does not itself pin
  a provider or copy a credential into the scratch directory.
- **`OpencodeAdapter`**/**`CodexAdapter`** shell `opencode run <task> --format json --auto` /
  `codex exec <task> --json --sandbox workspace-write`, each tool's own documented non-interactive
  mode plus the permission/sandbox flag that actually lets it edit files and run commands
  headlessly (`--auto` auto-approves any permission request not explicitly denied; `workspace-write`
  is the minimum sandbox posture Codex's default "never approve" policy needs to touch the
  fixture at all). Neither needs an opt-in gate: both already default to reading `OPENAI_API_KEY`
  (or an equivalent already-cheap credential) from the environment for their own normal use, per
  `docs/backlog.md`'s own reasoning — asking for them here costs nothing extra to wire.
- **`ClaudeAdapter`** shells `claude -p <task> --output-format json --permission-mode
  bypassPermissions` (not `--permission-mode dontAsk`, which denies rather than approves every
  prompt). `run()` refuses to construct it at all unless `--real-claude-auth` is passed
  explicitly — the one adapter here that can reach a real, metered Anthropic API (or an
  interactive subscription session) by its own default credential resolution, so it is never the
  accidental default of running this command.
- Without a real permission/sandbox flag, every non-`tm` adapter could not actually edit files or
  run commands headlessly, so `tm` "won" every comparison for a reason that had nothing to do with
  harness quality — an early version of this harness had exactly that bug (caught by
  `docs/audits/2026-09-25-bench-plan.md`'s "Required fixes" item 0, fixed the same day this
  decision was accepted). `CrossToolReport::header` now states every tool's permission posture,
  `--version` output, the pinned `--model` (if any), and the `--task-timeout`/`--max-cost-usd`
  caps (if any), so a reader of the rendered report doesn't have to trust that the comparison was
  fair — it's stated.
- **`FakeAdapter`** (private to `bench_cross.rs`'s own `mod tests`) returns a fixed, scripted
  `AdapterOutcome` and never spawns a coding tool — what unit tests drive `run_comparison` with,
  per `SPEC.md` §0's determinism rule. Those tests do still spawn `git` (to give each scratch
  fixture a real history, same as production) and run the fixture's own `test_command` (e.g.
  `true`/`false`) — neither a coding tool nor a network call, the two things every real
  `ToolAdapter`'s own `Command` calls are confined behind.

`--tools` defaults to `tm` alone when omitted, since it's the only adapter with a real credential
already wired end to end in this repository. `render_markdown` produces a `tm bench report`-shaped
table (`| Tool | Task | Result | Score | Cost | Tool calls | Wall time |`) with a leading `Tool`
column, since this report always compares more than one tool for the same task; the full
`CrossToolReport` also lands as `<out dir>/cross-report.json`.

## Why

- **One mechanism, not four.** A `ToolAdapter` trait plus one `run_comparison` loop means adding a
  fifth tool later is one new adapter, not a forked copy of the harness.
- **Never trust a tool's own pass/fail self-report.** Every arm — including `tm`'s — is scored by
  independently re-running `task.fixture.test_command` against the scratch directory afterward.
  A tool's own reported success is not the ground truth this harness measures against.
- **Reuse the real bench fixture set and the real `tm` CLI surfaces, not new ones.** `BenchTask`
  parsing, `tm stats`'s already-existing `TicketMetrics` JSON, and `tm bench report`'s table shape
  are all reused verbatim rather than re-invented for this one harness.
- **Real runs are opt-in and never part of `verify`/`hygiene`.** Shelling to a real external CLI
  against a real (possibly metered) provider is exactly the non-deterministic, network-touching
  call `SPEC.md` §0 keeps out of the deterministic gate; `cargo test -p xtask` only ever exercises
  `FakeAdapter`, never a real coding tool.

## What this costs, stated plainly

- **`TmAdapter`'s `--task-timeout` bounds each individual `tm` subprocess call (init/dispatch/
  run/stats), not one deadline across the whole sequence.** Tracking remaining budget across four
  separate calls was out of scope; what this does guarantee is that no single `tm` call can hang
  forever, at the cost of a `tm` arm's total wall time being able to reach up to roughly `4 x
  --task-timeout` in the worst case rather than being hard-capped at exactly `--task-timeout`.
- **`--max-cost-usd` is checked between `(tool, task)` pairs, not mid-pair.** A pair already
  running when the cap is reached still finishes and its cost still counts toward the cap; only
  pairs not yet started are skipped. There is no way to abort a single tool call mid-flight based
  on its own eventual cost, since cost is only known once that call reports it.
- **With `--model`, the `tm` arm's pinned `coder.fast` candidate has no `price`.**
  `TmAdapter::pin_model` writes a bare `{provider, model, max_concurrency}` candidate, not the
  full `docs/providers.md`-documented shape that also carries a `price`. `tm`'s own cost
  accounting (D-030) reports `0` for an unpriced candidate rather than erroring, so a `--model`
  run's `tm` row always shows `$0` regardless of real spend, and that `$0` never contributes
  toward `--max-cost-usd`'s cumulative check. A follow-up wanting real cost parity for the `tm`
  arm under `--model` needs to look up (or accept as a flag) that model's real price and include
  it in the written candidate.
- **Scoring logic is duplicated, not shared.** `tm_harness::bench`'s ceiling-subscore formula is
  private, so `bench_cross.rs` re-derives its own copy (`ceiling_subscore`/`score_task`), matching
  this repo's own established convention (`tm-cli/src/bench_live.rs`'s `copy_dir_recursive` doc
  comment) rather than making that function `pub` for one caller. A future change to
  `tm-harness`'s real scoring weights will not automatically propagate here.
- **External tools' `tool_calls`/`tokens`/`cost_micros` are best-effort, not verified against any
  pinned schema.** `parse_best_effort_usage` scrapes a handful of commonly-named JSON fields from
  each tool's own documented output shape; none of those three tools' exact output schema is a
  dependency this crate pins or tests against a real installed binary (this task's own budget did
  not include installing and driving real `opencode`/`codex`/`claude` binaries end to end) — a
  shape any of them doesn't recognize silently degrades to "ran, but no metrics reported" rather
  than erroring, and a future drift in any of those tools' own JSON shape needs a human to notice
  and update the field-name list.
- **Only the `tm` arm stores its run as real tm ticket/event state.** `TmAdapter` does, via the
  scratch project `tm ticket dispatch`/`tm run` themselves create — that state is real and
  auditable by construction. The three external-tool arms do not additionally record a matching
  `tm` ticket for their own run: forcing an already-completed external run through `tm`'s ticket
  state machine (which assumes a single dispatcher-driven attempt reaching `Submitted`) risks
  misrepresenting how the work actually happened, and this task's budget did not include verifying
  such a path against a real `Store` with a build in this environment. A follow-up that wants full
  parity here should add a narrow, explicitly "externally observed" ticket-recording path rather
  than reusing the dispatcher-shaped one.
- **A scratch directory per `(tool, task)` pair is left on disk, and `disk:guard` does not
  reclaim it.** `disk:guard`'s tm-scratch-dir recognition only matches `$TMPDIR/tmp.*` and
  `/tmp/tm-*` *at that directory's own root* looking like tm's own (a `.tm` dir, a `project.db`,
  a bare `target/`-shaped dir, or a git repo directly at that root). The default `--out` root
  (`$TMPDIR/tm-bench-cross-<pid>`, not `/tmp/tm-*` on macOS where `$TMPDIR` is a per-user
  `/var/folders/...` path) holds only `<tool>/<task>/` subdirectories and a `cross-report.json`
  file at its own root, not a `.tm` dir or a git repo there — so today `mise run disk:guard`
  leaves it alone entirely. A follow-up should extend `scripts/disk-guard.sh`'s recognition (or
  have `bench-cross` write a `.tm`-shaped marker at its own `--out` root) so this doesn't need
  manual `rm -rf` cleanup after a run.
- **Dispatch/run/stats, not `tm bench run --live` itself.** `TmAdapter` drives `tm` through the
  same three CLI surfaces the "Decision" section above lists, not through `tm bench run --live`
  end to end — `--live` resolves `bench/tasks`/`bench/fixtures` under the *invoking* project's own
  root (`LiveSeededProvider::new`'s `project.root.join("bench")`), which is this harness's own
  scratch directory, not this repository's real `bench/` tree, so pointing `--live` at it would
  need the whole task suite copied in twice. Driving one ticket directly keeps `TmAdapter`
  symmetric with the other three adapters (one task, one process invocation per tool) instead of
  reusing a whole-suite command for a single task.
- **Adding `tm-harness` as a real dependency makes `xtask` no longer a near-instant compile.**
  `mise run hygiene`/`mise run fmt` (both wired through `cargo run -p xtask`) now pull in
  `tm-events`, `rusqlite`, and everything else `tm-harness` depends on before either check even
  starts scanning — `CLAUDE.md` currently calls `hygiene` "the fast standalone hygiene scan"; that
  description no longer holds for a from-clean build (a warm build is unaffected, same as any
  other crate).
