# `bench/` — Ticketmaster's own benchmark suite

This directory is repository-local benchmark state, read by `tm-harness`'s `bench` module and by
`crates/xtask/src/bench_cross.rs`. Nothing here calls a real provider or the network on its own —
every mode that does is opt-in and named below.

- **`tasks/*.toml`** — one `BenchTask` per file (`tm_harness::bench::BenchTask::parse`): an `id`,
  a `task` instruction, a `fixture` (path under `fixtures/`, description, optional
  `test_command`/`setup_commands`), a `scoring` weight table, and an `expected` outcome
  (predicate plus cost/latency/tool-call/context ceilings). `tm bench list` prints every task
  found here.
- **`fixtures/<id>/`** — the starting-state files for task `<id>`. Copied into a fresh scratch
  directory before any run ever touches them — nothing in `fixtures/` is ever edited in place.
- **`solutions/<id>.patch`** — a reference fix for a hermetic bug-fix task, applied only by
  `tools/check-fixtures.sh`, never by a benchmark run itself (an agent under test never sees
  these). Kept outside `fixtures/` for exactly that reason.
- **`tools/check-fixtures.sh`** / **`tools/PROVENANCE.md`** — for each hermetic bug-fix task:
  copies the fixture to a fresh `mktemp` dir, asserts `test_command` fails before the patch and
  passes after it, and records that task's upstream provenance/license. Run by hand:
  `bash bench/tools/check-fixtures.sh`.

## Running the suite

- **`tm bench run`** — the default, deterministic path: replays each task's scripted transcript
  (`FixtureScriptProvider`) with no network call and no live model, scoring against the same
  fixture every time. A task whose fixture only sets `test_command` (no scripted steps) is
  "live-only" and silently skipped here — see `live-smoke.toml`'s own comment.
- **`tm bench run --live`** — drives each matched task through a real ticket run instead (D-032):
  copies the fixture into a scratch project, creates and activates a real worker ticket, runs it
  through the same dispatcher `tm run` uses, then scores by actually running `test_command`. Real
  cost/tool-calls/tokens replace the scripted path's byte-length proxy. Slower and
  non-deterministic; a real provider call unless `TM_TEST_MOCK_PROVIDER=1` is set; never part of
  `mise run verify`.
- **`tm bench report <report.json> [--out FILE]`** — renders a saved `BenchmarkReport` (from
  either mode's `--out`) as a markdown table.
- **`tm bench compare <a.json> <b.json>`** — the promotion-facing epoch-to-epoch comparison.

## Cross-tool comparison

**`cargo xtask bench-cross [--tools tm,opencode,codex,claude] [--task <filter>] [--out <dir>]
[--model <provider/model>] [--task-timeout <secs>] [--max-cost-usd <amount>]
[--real-claude-auth]`** (`mise run bench:cross` is the alias; see
`docs/decisions/D-033-cross-tool-benchmark.md`) runs this same task set identically through `tm`'s
own live path and through one or more external coding CLIs, and reports pass/fail, score, cost,
tool calls and wall time for every `(tool, task)` pair side by side. Also opt-in, also never part
of `mise run verify`/`hygiene`.

- `--tools` defaults to `tm` alone. Add `opencode`/`codex` freely — both already default to
  reading their own `OPENAI_API_KEY`-shaped credential from the environment for their normal use,
  so there's no extra credential wiring here.
- `claude` additionally requires `--real-claude-auth` — without it, `bench-cross` refuses to run
  rather than silently reaching a real, metered Anthropic API or an interactive Claude Code
  session by default.
- Requires the requested tools' own binaries (`tm`, `opencode`, `codex`, `claude`) to already be
  installed and authenticated on `$PATH`; `bench-cross` does not install or configure any of them.
- Every non-`tm` adapter now runs under a real edit/bash permission posture instead of a headless
  no-op: `claude -p --permission-mode bypassPermissions`, `codex exec --sandbox workspace-write`,
  `opencode run --auto`. `tm` needs no such flag — it always runs the ticket loop directly inside
  the scratch working dir. Each tool's actual posture is printed in the report header.
- `--model <provider/model>` (e.g. `anthropic/claude-sonnet-5`) pins every requested tool to the
  same model rather than each tool's own default/last-configured one — `--model` for
  claude/codex/opencode, and for `tm` a scratch `providers.toml` `coder.fast` role candidate
  written right after `tm init`.
- `--task-timeout <secs>` bounds each adapter's own subprocess work; a call that runs past it is
  killed (whole process group, not just the direct child) and the pair is reported as `TIMEOUT`
  rather than `pass`/`fail`.
- `--max-cost-usd <amount>` stops scheduling further `(tool, task)` pairs once cumulative reported
  spend reaches it (checked between pairs, not mid-pair — a pair already running still finishes).
- The report header also lists each requested tool's own `--version` output.

Example, comparing `tm` against `opencode` on the hermetic Python fixtures only:

```sh
cargo run -p xtask -- bench-cross --tools tm,opencode --task py- --out /tmp/tm-bench-cross-demo
```

Unit tests (`cargo test -p xtask`, part of `mise run verify`) exercise the harness's own scoring
and reporting logic against a scripted `FakeAdapter` — they never spawn `tm`, `opencode`, `codex`,
or `claude`, and never touch a network.

## Adding a task

Do not name a task `live-smoke` — that id is reserved for `bench-live-seeded-provider`'s own
plumbing-proof fixture (`tasks/live-smoke.toml`'s own comment explains why). For a hermetic
bug-fix task, follow `tools/PROVENANCE.md`'s existing entries and re-run
`bash bench/tools/check-fixtures.sh` before committing.
