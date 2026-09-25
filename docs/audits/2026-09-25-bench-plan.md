# Benchmarking `tm` vs Claude Code / OpenCode / Codex CLI — plan

Research only; nothing here has been run. Owner: allisonemilycoleman@gmail.com. Repo: `/Users/allie/Develop/ticket-master`.

## 0. What tm already has (reuse, don't rebuild)

- `bench/` — repo-local suite: `bench/tasks/*.toml` (`BenchTask`), `bench/fixtures/<id>/`,
  `bench/solutions/<id>.patch` (reference fix, never shown to the agent),
  `bench/tools/check-fixtures.sh` (proves each hermetic fixture fails-then-passes).
  Currently 3–5 small (few-MB) permissively-licensed bug-fix tasks (Python/stdlib `unittest`,
  no network) — explicitly **not** the real SWE-bench corpus (`bench/tools/PROVENANCE.md`: "the
  full SWE-bench corpus... is explicitly out of scope").
- `crates/tm-harness/src/bench.rs` — `tm bench run` (scripted replay, deterministic, free) and
  `tm bench run --live` (D-032: real ticket run + real `test_command`, real cost/tokens).
  `tm bench report` / `tm bench compare` render/diff a `BenchmarkReport`.
- `crates/xtask/src/bench_cross.rs` (D-033) — **already the cross-tool harness the owner is
  asking for**: `cargo xtask bench-cross --tools tm,opencode,codex,claude [--task <filter>]
  [--out <dir>] [--real-claude-auth]` (alias `mise run bench:cross`). One `ToolAdapter` trait,
  four adapters (`TmAdapter` shells `tm init` → `tm ticket dispatch` → `tm run` → `tm stats
  --json`; `OpencodeAdapter` shells `opencode run <task> --format json`; `CodexAdapter` shells
  `codex exec <task> --json`; `ClaudeAdapter` shells `claude -p <task> --output-format json`,
  gated behind `--real-claude-auth` since it's the only one that hits a metered API by default).
  Scores every arm by independently re-running the fixture's own `test_command` — never trusts a
  tool's self-report. Unit tests use a `FakeAdapter`, no network, part of `mise run verify`.
  Known gaps (from D-033 "What this costs"): external tools' cost/tokens are best-effort JSON
  field scraping, not pinned to a real installed binary's schema; scratch dirs under
  `$TMPDIR/tm-bench-cross-<pid>` aren't cleaned by `disk:guard`; only `tm`'s arm gets real
  ticket/event-log state.

**Implication for Track A**: don't invent a second comparison harness. Point A's dataset(s) at
`bench-cross`'s existing `--task <filter>` + `bench/tasks/*.toml` plumbing, and extend the fixture
set (Track A's own action item below) rather than writing a new runner.

**Read `crates/xtask/src/bench_cross.rs` directly — four real gaps to fix before trusting its
numbers, not just documentation debt:**

0. **No permission/sandbox flags — the most likely source of a fake result.** Every non-tm
   adapter calls its tool with *only* the task text and an output-format flag: `claude -p <task>
   --output-format json`, `opencode run <task> --format json`, `codex exec <task> --json`. None
   of the three pass anything that grants edit/write/bash permission. Confirmed from Claude Code's
   own permission-modes docs (fetched directly, not paraphrased): **`--permission-mode dontAsk`
   is the wrong choice here** — `dontAsk` converts every prompt into a **denial**, not an
   approval, so a headless run in that mode still can't Edit/Write/run arbitrary Bash without a
   pre-configured allow-list, which is the exact failure this gap is about. The correct flag is
   **`--permission-mode bypassPermissions`** (auto-approves everything except `rm`/`rmdir` against
   a handful of critical paths) — used only inside each trial's own disposable scratch clone, never
   against the primary checkout, matching this repo's existing worktree-isolation rules. (An
   `acceptEdits` + explicit `--allowedTools Bash` combination is the narrower alternative if a
   future implementer wants Bash calls individually visible rather than blanket-approved.)
   **Codex** defaults `codex exec` to a "never approve" policy but still runs inside a sandbox
   unless told otherwise — needs `--sandbox workspace-write` at minimum (the fully-open
   `--dangerously-bypass-approvals-and-sandbox`/`--yolo` only inside a disposable container, never
   on this machine directly). **OpenCode** needs `--auto` (auto-approves any permission request
   not explicitly denied) — matching posture with Claude Code's `bypassPermissions` and Codex's
   `workspace-write`. **Without these,
   every non-tm arm likely can't edit files or run tests at all, and `tm` "wins" every bench-cross
   comparison for a reason that has nothing to do with harness quality.** Required follow-up:
   add these flags to `ClaudeAdapter`/`OpencodeAdapter`/`CodexAdapter`'s `Command::args` calls
   (verify exact current flag names against each tool's own `--help` at implementation time — this
   research pass found the flags via docs/search, not a live invocation), and use the **same**
   effective permission posture across all four tools so the Interruptions/approvals rubric
   dimension in Track B measures the same thing for every tool, not "which tool happened to be
   configured to not ask."
1. **No model pinning.** `ClaudeAdapter`/`OpencodeAdapter`/`CodexAdapter` each call their binary
   with only the task text and an output-format flag (`claude -p <task> --output-format json`;
   `opencode run <task> --format json`; `codex exec <task> --json`) — no `--model` passthrough
   anywhere, and `TmAdapter` never sets a provider env var either. Every comparison run today
   uses whatever each tool's own default/last-configured model happens to be, which silently
   breaks the Fairness goal below. **Required follow-up before a real cross-tool run**: extend
   `ToolAdapter::run` (or the adapter structs) to accept an explicit model id per tool and pass it
   through (`claude -p ... --model <id>`, `codex exec --model <id>`, `opencode run --model <id>`,
   and, for `TmAdapter`'s scratch project, the real mechanism per `docs/providers.md`: write a
   `providers.toml` role candidate — `(provider, model)` — into the scratch project right after
   `tm init` (there is no `TM_*_MODEL` env var; role resolution order is `providers.toml` →
   legacy role-shaped `harness.toml` → `<state_dir>/default-model.json` → built-in defaults, so
   the adapter needs to write the file, not set an env var) — confirm each other tool's exact
   flag name against its own `--help` at implementation time, not assumed here.
2. **No per-task timeout.** `TmAdapter::tm_json`/the other three adapters call `.output()`
   directly with no `Command::spawn` + timeout wrapper — a hung or slow-looping tool blocks the
   whole suite indefinitely. **Required follow-up**: wrap each adapter's `Command` in a timeout
   (e.g. `wait_timeout` crate, or a `std::thread` + `kill()` after N seconds) before this is safe
   to run unattended, and before the cost caps below mean anything.
3. **No cost cap enforcement.** `parse_best_effort_usage` only *reads* a reported cost after the
   fact; nothing aborts a run mid-flight for exceeding a budget. Cost caps stated below are
   therefore an operator discipline (watch the run, kill it manually) unless a real cap is added
   to the adapters — note this explicitly rather than implying the harness enforces it.

Until items 1–2 land, run `bench-cross` only attended (watch it, ready to Ctrl-C), and treat any
result as "same harness shape, unpinned model" rather than a fair model-controlled comparison.

## Track A — Objective benchmarks

### Landscape (2026)

| Benchmark | Measures | Harness/CLI shape | Docker | Cost/run (full) | 8GB-Mac feasible? |
|---|---|---|---|---|---|
| **SWE-bench Verified** (500, OpenAI-curated) | Real GH issue → patch, Fail-to-Pass tests | `sb-cli`/official Docker harness, per-instance image | Required; ~30GiB registry (image dedupe), Docker Desktop wants ~120GB free | 62 min on 32-core/128GB CI runner for full 500; per-instance ~5–15 min | **No** for the full/Verified-Mini set at scale — disk alone blows this machine's budget (this repo's CLAUDE.md: 8GB Mac, disk incidents already happened). A **handful of hand-picked instances**, one Docker image at a time, sequentially, is feasible if disk is watched. |
| **SWE-bench Lite** (300, 11 Python repos) | Same as Verified, easier/curated-for-cost subset | Same Docker harness | Same requirement, smaller image set | Lower than Verified but still per-instance images | Same caveat — Lite reduces *model* cost, not *disk/docker* cost. |
| **SWE-bench Live** (1,565 tasks, 164 repos, monthly refresh) | Same shape, contamination-resistant (new issues each month) | Same Docker harness family | Required | Ongoing/rotating | No — larger, same Docker cost, and monthly churn adds pipeline maintenance this project doesn't want. |
| **SWE-bench Multimodal** (480–517, visual bug reports) | Same + screenshots/mockups | Same Docker harness, vision-capable model needed | Required | Similar | No — needs vision model wiring `tm`'s adapters don't have; out of scope. |
| **Terminal-Bench / Harbor** (Terminal-Bench 2.0/4.0) | General terminal/sysadmin task completion via a real shell, pytest-style verifier on end-state | `harbor run --dataset terminal-bench@2.0 --agent <adapter> --model <id> --n-concurrent N`; Harbor **already ships `claude-code` and `codex` adapters**, and an `opencode` adapter is configurable | Each task runs in its own Docker sandbox (typically small, task-scoped images, not one 30GB registry) | Task-dependent, roughly $0.05–$2/task depending on model/turns | **Yes, in small batches** — per-task images are far lighter than SWE-bench's; run 10–20 tasks sequentially (`--n-concurrent 1-2`), clean images between batches. |
| **Aider Polyglot** (225 Exercism problems, 6 langs: C++/Go/Java/JS/Python/Rust) | Code-edit correctness via unit tests, 2-attempt (with failing-test feedback) protocol, **no Docker** | `aider --benchmark`, or reimplement the loop: feed exercise, apply diff/whole edit, run tests, second attempt on failure | **None** — pure language toolchains (needs Python/Go/Java/Rust/Node/C++ compilers installed) | Full run for a frontier model ~$1–$180 depending on model/reasoning effort; per-instance a few cents to ~$1 | **Yes** — no Docker, disk footprint is the toolchains only, and it's already multi-language which fits tm's own polyglot repo. |
| **LiveCodeBench** | Competitive programming (LeetCode/AtCoder/Codeforces), contamination-resistant by release date | Python harness, sandboxed execution, no repo context | Light (sandboxed exec, not full Docker images) | Cheap per problem | Feasible, but **not repo/agent-shaped** — no ticket/PR/repo-navigation signal, doesn't exercise the harness surfaces (`tm run`, Genesis, tool use) this project cares about. Low priority. |
| **SWE-Gym / Multi-SWE-bench** | SWE-Gym: RL training environments (agent trajectories + verifiers) mirroring SWE-bench; Multi-SWE-bench: SWE-bench-shaped but multi-language (Java/TS/Go/Rust/C/C++...) | Docker per instance, same family as SWE-bench | Same Docker cost profile as SWE-bench | Similar to SWE-bench Lite | SWE-Gym is training-oriented, not eval-oriented — skip. Multi-SWE-bench is the interesting one for polyglot coverage but same Docker/disk cost as SWE-bench; treat as a stretch goal, not this round. |

### Recommendation: two benchmarks, both feasible on this machine

1. **Primary: Aider Polyglot, small subset (20–30 of the 225 exercises), no Docker.**
   Best fit: zero Docker/disk cost, genuinely polyglot (matches tm's own Rust/TS/Swift spread),
   cheap per-instance, and its "second attempt with failing-test feedback" protocol maps cleanly
   onto `tm run`'s own retry/verification loop. Reuse `bench-cross`'s adapter pattern rather than
   `aider --benchmark` itself, since the goal is comparing tm/Claude Code/OpenCode/Codex, not
   Aider.

   ```sh
   # 1. Vendor a subset of Aider's exercises as bench/tasks fixtures (one-time, needs explicit
   #    go-ahead per this repo's provenance convention — bench/tools/PROVENANCE.md).
   git clone --depth 1 https://github.com/Aider-AI/polyglot-benchmark /tmp/polyglot-benchmark
   # Pick ~4-5 exercises per language (Python, JS, Go, Rust; skip C++/Java to limit toolchain
   # installs) = ~20 exercises. Each becomes bench/fixtures/<lang>-<exercise>/ with a
   # bench/tasks/<lang>-<exercise>.toml pointing at that exercise's own test file as
   # test_command (Exercism problems already ship canonical tests).
   bash bench/tools/check-fixtures.sh   # must fail-before/pass-after for every new fixture

   # 2. Dry-run tm alone first (cheap, catches fixture/setup bugs before spending on 4 tools):
   cargo run -p xtask -- bench-cross --tools tm --task polyglot- --out /tmp/tm-bench-polyglot-tmonly

   # 3. Once fixtures are proven, run all four tools against the subset (after the permission/
   #    model/timeout follow-ups above land in bench_cross.rs):
   cargo run -p xtask -- bench-cross \
     --tools tm,opencode,codex,claude --real-claude-auth \
     --task polyglot- --out /tmp/tm-bench-polyglot
   ```

   Cost estimate: 20 exercises × 4 tools × ~$0.05–0.30/call (small model, few turns) ≈ **$5–25
   total** for one pass, using a fast/cheap model (see Fairness below). Disk: negligible (source
   files only, no Docker).

   **Caveats, stated plainly, not glossed over**: Aider's own protocol is pass@1 with one
   feedback retry, where the model sees the failing-test output but not the test file itself
   mid-run in most configs. `tm run`/`claude -p`/`opencode run`/`codex exec` all iterate freely
   against the fixture's real files (including its test file, since nothing here hides it from
   the agent) until they stop or the tool's own turn/time limit hits — a materially different,
   more permissive protocol. Treat this as **"pass@1, free self-iteration, capped by wall-clock,"
   not an Aider-leaderboard-comparable number** — do not report these scores next to Aider's own
   published leaderboard as if they were the same metric.
   Each fixture's `bench/solutions/<id>.patch` (per `bench/README.md`'s existing convention) comes
   from that Exercism exercise's own `.meta/`-directory example solution, not a hand-written fix —
   record that provenance in `bench/tools/PROVENANCE.md` same as the existing hermetic tasks.
   Per-language test-command gotchas to check at ingestion time, not assumed here: Exercism's Rust
   track marks some tests `#[ignore]` by convention (needs `cargo test -- --include-ignored`, or
   the fixture's committed test file needs that attribute stripped); Exercism's JS track exercises
   commonly need `npm install`/`pnpm install` before `test_command` can run at all, which conflicts
   with this repo's existing "hermetic, no network" bench-fixture convention (`bench/tools/
   PROVENANCE.md`'s framing) — either vendor `node_modules` into the fixture (adds real weight,
   check size first) or accept and document that the JS subset is not fully hermetic like the
   existing Python fixtures are.

2. **Secondary: Terminal-Bench/Harbor, 10–15 hand-picked easy/medium tasks.** Gives a second,
   independently-designed axis (general terminal competence, not just code-edit correctness), and
   Harbor already ships a `claude-code` agent adapter with `codex`/`OpenHands` also named as
   supported — confirmed from `github.com/harbor-framework/harbor`'s own README: install is
   `uv tool install harbor` (or `pip install harbor`); `harbor run` takes `--dataset`/`-d`,
   `--agent`/`-a`, `--model`/`-m`, `--n-concurrent`, and `--env` (for a cloud sandbox provider,
   e.g. `daytona` — omit to run local Docker); the confirmed dataset name is
   `terminal-bench@2.0`. **Confirmed** (via the repo's own file tree,
   `gh api repos/harbor-framework/harbor/git/trees/main?recursive=1`): an `opencode` agent
   adapter genuinely ships — `src/harbor/agents/installed/opencode.py` plus its own unit tests
   exist in the repo — so Claude Code, Codex, and OpenCode all have a real Harbor path, not just
   two of three. **Still not independently confirmed**: a per-task filter flag, an explicit
   output-directory flag, and whether task images are amd64-only. **Run `harbor run --help` and
   `harbor --help` for real before committing to exact flags** — the commands below are the
   best-supported shape from the README's own examples plus the confirmed agent list, not
   verified against a live install:

   ```sh
   uv tool install harbor   # needs Docker running
   harbor run --help        # confirm task-filter/output-dir flags before the real run
   harbor run --dataset terminal-bench@2.0 --agent claude-code \
     --model anthropic/claude-<pinned-id> --n-concurrent 1
   harbor run --dataset terminal-bench@2.0 --agent codex \
     --model <pinned-id> --n-concurrent 1
   harbor run --dataset terminal-bench@2.0 --agent opencode \
     --model <pinned-id> --n-concurrent 1
   ```

   **tm has no native Harbor adapter** — writing one (mirroring `bench_cross.rs`'s `TmAdapter`:
   `tm init` in the task's own container/workdir, `tm ticket dispatch <instruction>`, `tm run`,
   `tm stats --json`) is a real follow-up task, not something to fake by copying Terminal-Bench
   task statements into `bench/tasks/` fixtures — **that fallback is explicitly rejected**: a
   Terminal-Bench task's difficulty and verification depend on its own container's pre-seeded
   filesystem/service state (e.g. a partially-configured server, a corrupted build), which a bare
   `bench/fixtures/` directory copy cannot reproduce. Running tm on this axis at all requires
   either the real Harbor adapter, or accepting Track A's `bench-cross` (different fixture set) as
   tm's only cross-tool datapoint for now.

   **Platform constraints, checked, not assumed**: `.github/workflows/release.yml` builds
   `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin` — a Linux `tm` binary exists (x86_64
   only, no `aarch64-unknown-linux-gnu` target today) for a future Harbor adapter to run inside a
   Linux container. Terminal-Bench's own image architecture (amd64-only vs. multi-arch) was **not
   confirmed** from the README fetch — check `docker manifest inspect` on one task image before
   running; if amd64-only, every container runs under Rosetta/QEMU emulation on this arm64 Mac,
   which is slower and a real consideration for the 20-minute-per-trial-style time bounds below.

   Cost estimate: 15 tasks × 3 tools (claude-code, codex, opencode — tm has no Harbor adapter
   yet, see above) × ~$0.10–1.00/task ≈ **$5–45 total**. Docker: install one task image, run,
   `docker image rm`/`mise run disk:guard` before the next — never let more than 1-2 images sit at
   once given 8GB RAM / tight disk.

**Explicitly not pursued this round**: full SWE-bench (any flavor) — disk cost alone is
disqualifying on this machine, per this repo's own recorded disk incidents; SWE-bench
Multimodal (needs vision, tm's adapters don't do that yet); SWE-Gym (training-oriented);
Multi-SWE-bench (same Docker cost as SWE-bench, stretch goal only); LiveCodeBench (doesn't
exercise repo/agent navigation, low priority).

### Fixture-set extension task

Add `bench-aider-polyglot-fixture-ingestion` (S/M, sonnet, area: bench, deps:
`bench-swe-lite-fixture-ingestion`) to `docs/tasks/TASKS.md`'s bench section: vendor ~20 Aider
Polyglot exercises as `bench/fixtures/polyglot-<lang>-<name>/` + `bench/tasks/polyglot-*.toml`,
record provenance/license in `bench/tools/PROVENANCE.md` (Aider's polyglot-benchmark repo is
Apache-2.0 per its GitHub license file — verify at vendor time), and extend
`bench/tools/check-fixtures.sh` to cover the new language toolchains it needs (Go/Rust compilers
already implied by this being a Rust workspace + `mise.toml`; Node already needed for
`clients/web`; skip C++/Java to avoid new toolchain installs).

## Track B — Real-task subjective trials

### Repo/task list (7 repos, 4 languages, small-to-medium, real closed issues with a real fix)

Found live via `gh issue list -R <repo> --search "is:closed linked:pr" --state closed --limit 5
--json number,title,url,closedAt` (this session, 2026-09-25, `gh` already authenticated as
`alliecatowo`) — real, currently-closed issues, not placeholders. `linked:pr` narrows to issues
GitHub associated with a PR, but does **not** guarantee that PR merged or added a test — confirm
both (`gh pr list -R <repo> --search "<issue #> in:body"` or the issue's own timeline) and pin the
exact base SHA before running any trial, per the protocol below.

| # | Repo | Lang | Size | Task (real issue) | Verification |
|---|---|---|---|---|---|
| 1 | `psf/requests` | Python | small | [`psf/requests#7432`](https://github.com/psf/requests/issues/7432) — "`prepare_body` stream detection regression" | Apply the fix PR's test-file diff only, then `python -m pytest tests/ -k <that test>` |
| 2 | `pallets/click` | Python | small-med | [`pallets/click#3822`](https://github.com/pallets/click/issues/3822) — "`click.Path` should be generic on `path_type`" is a typing-only change; **verification for it should include a type-check step** (`mypy`/`pyright` against the fix, not just `pytest`, since a runtime test suite alone won't exercise a `Generic[AnyStr]`-shaped change) — otherwise substitute a genuine runtime-behavior bug from the same tracker at pin time. Avoid the `[rejected AI]`-tagged issues in this tracker — several recent closures are exactly that, a signal not to reuse for a trial. | `python -m pytest tests/` **and** `mypy`/`pyright` on the affected file, after checking out the fix PR's test files |
| 3 | `sindresorhus/ky` | TS | small | [`sindresorhus/ky#878`](https://github.com/sindresorhus/ky/issues/878) — "`onDownloadProgress` never emits `percent: 1` for empty response bodies" | `pnpm install && pnpm test` (not fully hermetic — needs npm registry access once) |
| 4 | `spf13/cobra` | Go | small-med | [`spf13/cobra#2257`](https://github.com/spf13/cobra/issues/2257) — "Completions modify os.Args" | `go test ./...` after checking out the fix PR's test files |
| 5 | `BurntSushi/ripgrep` | Rust | medium | [`BurntSushi/ripgrep#3376`](https://github.com/BurntSushi/ripgrep/issues/3376) — "`.gitignore` not taken into account when multiple search paths are provided" (deterministic behavior bug — prefer this over `#3419`'s nondeterministic-walk-ordering issue, whose hidden test could pass by chance rather than by a correct fix) | `cargo test -p ignore` after checking out the fix PR's test files |
| 6 | `tokio-rs/mini-redis` | Rust | small | Teaching repo, still active (`pushed_at` 2026-04-15, not archived) — pick from its own open issues/PRs at run time, since it has low issue volume; if nothing suitable is open, substitute a second `ripgrep` or `cobra` instance instead of forcing a weak task here | `cargo test` |
| 7 | `gohugoio/hugo` | Go | medium | [`gohugoio/hugo#15360`](https://github.com/gohugoio/hugo/issues/15360) — "`parser/metadecoders`: `UnmarshalToMap` does not strip a leading UTF-8 BOM" — single-package, narrowly scoped | `go test ./parser/metadecoders/...` after checking out the fix PR's test files |

**Protocol per task, made concrete**: clone at the fix PR's **base SHA** (`git checkout
<PR-base-sha>` right after clone — this is what makes the check objective, SWE-bench-style: the
agent never sees the fix). Give the agent only the issue's title+body text (optionally the issue
URL, if the tool can fetch it — record whether it did). After the agent's run, apply **only the
fix PR's test file(s)**, applied by **overwriting** whatever the agent's own trial produced —
`git checkout <fix-PR-merge-sha> -- <test file path(s)>` in the trial's scratch clone, not a
`git apply` of a diff (a diff can conflict if the agent itself touched the test file; a direct
checkout of the fix commit's test-file content always succeeds and matches the real SWE-bench
convention of overwriting rather than merging) — then run the verification command. This checks
the agent's *implementation* fix against a hidden test it never got to see or edit, not a test it
could have gamed. Record each task's own
**setup commands** discovered at pin time (e.g. `python -m venv .venv && pip install -e .[test]`
for the Python repos, `pnpm install` for `ky`, `go mod download` for the Go repos, bare `cargo
test` needs no extra setup for the Rust repos) — the table above omits them since they depend on
the exact pinned commit's own `setup.py`/`package.json`/`go.mod`, not something to guess now.

### Protocol

For each (repo × tool) pair, in a **fresh clone**: `git clone <repo> /tmp/tm-bench-b/<repo>-<tool>-<run-id>` — a **full clone, not `--depth N`**, since the protocol needs both the issue's base SHA and the fix PR's merge SHA (an old base commit and a since-merged PR commit are very unlikely to both sit inside a shallow window; if history size becomes a real problem for a specific repo, use `git fetch --depth 1 origin <base-sha> <fix-merge-sha>` instead of a full clone, fetching exactly the two commits needed rather than guessing a depth). Check out the same pinned base SHA across all tools for that repo.

- **tm — two arms**: (a) `tm init && tm ticket dispatch "<task text + issue URL>" && tm run <ticket>`
  (single dispatcher-driven run, matches `bench-cross`'s `TmAdapter` shape); (b) `tm genesis
  --prompt "<task text + issue URL>"` (`crates/tm-cli/src/args.rs`'s `GenesisArgs`: "turn a
  natural-language prompt into a running project via Genesis") for the repos where the task
  naturally decomposes into more than one ticket (e.g. the Hugo/repo-#7 package-scoped bug, if it
  turns out to touch more than one subsystem) — `tm genesis` is the real CLI surface for this, not
  a hand-built multi-ticket workaround; `--resume` if a genesis run needs to continue past one
  session. **Caveat, not verified in this research pass**: `GenesisArgs`'s own doc comment reads
  "turn a natural-language prompt into a running *project*", which reads as a greenfield/new-project
  flow — whether it also works cleanly pointed at an existing, already-cloned repo (as arm (b)
  needs) was not confirmed here. Check this before relying on it; if Genesis turns out to be
  greenfield-only, arm (b) becomes `tm milestone new "<title>"` plus several scoped `tm ticket new`
  calls instead, to still exercise a real multi-ticket decomposition path.
- **Claude Code**: `claude -p "<same task text>" --output-format json --permission-mode bypassPermissions --model <pinned-id>` in the same fresh clone (see gap #0/#1 above — `dontAsk` denies rather than approves, so `bypassPermissions` is the correct headless flag here).
- **OpenCode**: `opencode run "<same task text>" --format json --auto --model <pinned-id>` in the same fresh clone.
- **Codex** (if included per the note above): `codex exec "<same task text>" --json --sandbox workspace-write --model <pinned-id>` in the same fresh clone.
- Every arm's **setup commands run identically, before the agent starts, in every arm** —
  `pip install -e .[test]`/`pnpm install`/`go mod download` etc. happen once per fresh clone as
  a pre-step the trial protocol runs, never left for the agent to discover and run itself. This
  matters concretely for Codex: its `workspace-write` sandbox blocks outbound network by default,
  so an agent-initiated `pip install`/`pnpm install` would fail for the Codex arm specifically
  while succeeding for the others, skewing the comparison toward "codex failed to set up its own
  environment" rather than "codex failed the actual task."
- (Codex CLI is optional here — Track A already covers it structurally via `bench-cross`; include
  in Track B only if the owner wants a 4-way instead of 3-way real-task comparison.)
- **Time bound**: 20 minutes wall-clock per trial (kill and mark "timed out" past that — this
  is a trial-comparability bound, not a claim about any tool's real ceiling).
- **Cost cap**: $2/trial; abort and log if exceeded mid-run where the tool reports live cost.
- After each trial: run the repo's own verification command against the resulting worktree,
  independently (same "never trust self-report" rule as `bench-cross`). Record pass/fail
  mechanically before any subjective grading happens.

### Rubric (reviewer agent, one reviewer per trial)

Reviewer agent gets: the full transcript/log, the final diff, the verification command's
pass/fail, and cost/time/token counts if available — not the other tools' trials (avoid
comparative bias mid-grade; comparison happens at reduce time). 1–5 anchored scale per item
(1 = fails badly, 3 = adequate/competent, 5 = excellent/exemplary), plus one free-text field:

| Dimension | 1 | 3 | 5 |
|---|---|---|---|
| **Navigation quality** | Thrashes, re-reads same files, never finds the relevant code | Finds relevant code within a few reasonable searches | Near-direct path to the right file/function, minimal waste |
| **Context efficiency** | Reads far more than needed relative to task size (tokens/files touched vs. the task's actual footprint) | Reasonable ratio of context consumed to task size | Tight, surgical context use |
| **Plan quality** | No visible plan, or a plan that doesn't match the task | Plan present, roughly matches the fix needed | Clear, scoped, correctly ordered plan stated before acting |
| **Verification honesty** | Claims success without running tests, or misreports a failing run as passing | Runs tests, reports outcome roughly accurately | Runs the right tests, accurately reports result, notices near-misses |
| **UX/copy clarity** | Confusing, jargon-heavy, or misleading output to a human reader | Understandable progress narration | Clear, well-scoped narration a non-expert could follow |
| **Interruptions/approvals** (headless-mode meaning defined below — this is *not* a literal prompt count, since `-p`/`run`/`exec` modes mostly don't prompt) | Reports denied/blocked actions with no recovery, or the transcript shows the agent proceeding past something that should have required approval | A clean run with no denials, or a denial the agent correctly worked around | N/A — excluded from the aggregate when the tool reports no approval/denial data at all (not scored as 5; see note) |
| **Recoverability** | A stumble (wrong edit, failed test) derails the whole run | Recovers from one stumble with some backtrack | Recovers smoothly, adjusts plan without restarting from scratch |
| **Cost/time** | Blows the cap without finishing | Finishes within cap, unremarkable efficiency | Finishes well under cap |

Free text: *"What would make this better?"* — open field, one paragraph.

**Interruptions/approvals in headless mode, made concrete**: `claude -p`, `opencode run`, and
`codex exec` mostly don't prompt interactively in their non-interactive modes — a literal count of
"times it stopped and asked" will be near-zero for every tool most trials, which would make this
dimension empty as written. Score it instead from what each tool's own output *reports* about
permission handling: Claude Code's `-p --output-format json` result includes a
`permission_denials`-shaped field when an action was blocked by its permission mode (confirm the
exact field name against a real `claude -p --output-format json` run before scoring — not
independently verified in this research pass); `codex exec --json`'s streamed events include
approval-request-shaped events when its sandbox mode requires one; for `tm`, read the scratch
project's own event log for `approval.requested` events (`tm events`, same event kind `tm sched
run`/`tm run` already use to pop a desktop notification) — set `TM_NOTIFY=0` in every trial's
environment so a real trial run never pops a desktop notification. When a tool reports zero
denials/approvals, mark this dimension **N/A for that trial** and exclude it from the aggregate
rather than scoring it — with the permission/sandbox flags from gap #0 above in place (all four
tools running with equivalent auto-approve posture), "no denials" is the expected common case for
every tool, not a signal any one tool did better; scoring it as a 5 would bias the aggregate
toward whichever tool happens to report the field at all rather than measuring real behavior.

**Reviewer output schema** (fixed JSON shape every reviewer call must produce, so the reduce step
has a stable input to fold over):

```json
{
  "repo": "psf/requests",
  "tool": "tm",
  "arm": "dispatch",
  "issue_url": "https://github.com/psf/requests/issues/7432",
  "verification_passed": true,
  "scores": {
    "navigation_quality": 4,
    "context_efficiency": 3,
    "plan_quality": 5,
    "verification_honesty": 5,
    "ux_copy_clarity": 4,
    "interruptions_approvals": 5,
    "recoverability": 3,
    "cost_time": 4
  },
  "what_would_make_this_better": "free text, one paragraph",
  "cost_usd": 0.42,
  "wall_seconds": 610,
  "tool_calls": 14,
  "tokens": 38000
}
```

`arm` is `"dispatch"` or `"genesis"` for tm, `"cli"` for the other tools. `verification_passed`
is the mechanical pass/fail from the hidden-test check, computed before grading and handed to the
reviewer as ground truth (never inferred by the reviewer from the transcript) — matches
`bench-cross`'s own "never trust a tool's self-report" rule.

### Aggregation → scoreboard → tasks

- **Map**: for each (repo, tool) pair spawn a Sonnet runner subagent that executes the protocol
  above in an isolated worktree/tempdir (never the primary checkout — this repo's own CLAUDE.md
  rule) and writes a trial artifact: transcript, diff, verification result, cost/time.
- **Grade**: one reviewer-agent pass per trial artifact (not per repo — keeps grading blind to
  the other tools' runs), producing the rubric scores + free text as structured JSON.
- **Reduce**: aggregate per tool (mean/median per dimension across repos), per repo (does one
  tool dominate on Rust but not Go?), and overall. Render as a markdown scoreboard table
  (dimension × tool) plus a short narrative. Feed every free-text "what would make this better"
  entry that names a *tm-specific* gap into `docs/tasks/TASKS.md` as a real, sized task (reuse
  this repo's existing task-entry format: model/size/area/deps/change/acceptance/test) rather than
  leaving it as prose feedback that nobody acts on.

## Fairness, cost caps, reproducibility

- **Two model cohorts, not one universal pin** — `docs/providers.md` confirms tm's own routing is
  provider-table-driven (`providers.toml`/`harness.toml`/`default-model.json`), so pin per cohort
  rather than assuming one model reaches every tool: **Anthropic cohort** (tm via `providers.toml`
  pointed at an Anthropic model, Claude Code natively, OpenCode via its own Anthropic-model config)
  and **OpenAI cohort** (tm via `providers.toml` pointed at an OpenAI-compatible model, Codex
  natively, OpenCode again via its own config, switched). Record the exact model ID each tool
  actually used in its own reported output — tools sometimes silently default to a different model
  than requested — and run each cohort as a separate pass rather than mixing models within one
  comparison table.
- **Isolate each tool's global/user config before a trial**, not just the repo clone — every one
  of these tools reads user-level config (`~/.claude/CLAUDE.md`'s own zvec-grep/terminal-mcp
  instructions and enabled MCP servers/plugins apply in *every* directory Claude Code runs in
  unless overridden; Codex and OpenCode each have their own global config/`AGENTS.md` discovery).
  Left as-is, this leaks navigation shortcuts and context into some tools' trials but not others'
  and breaks reproducibility. Use each tool's documented isolation flag for a clean per-trial run
  (e.g. a scratch `--settings`/config-dir override, or an env var pointing at an empty config
  directory) — confirm the exact flag from that tool's own docs at implementation time, not
  guessed here; don't rely on memory of what each tool's flags are named.
- **Cost caps**: Track A ≈ $10–70 total across both benchmarks' small subsets; Track B ≈ 7 repos
  × (2 tm arms + claude + opencode) × $2 cap × 2 model cohorts = **≤$112** worst case, realistically
  $25–50 given most trials finish well under cap and not every repo needs both cohorts run in
  full. Reviewer-agent grading calls themselves add a small additional cost (cheap model, short
  context per trial) — budget an extra ~$5–10 for that pass. Total plan cost: **well under $200**
  for a full pass across both tracks.
- **Reproducibility**: record per run — tool binary version (`tm --version`, `claude --version`,
  `opencode --version`, `codex --version`), pinned model ID, exact commit SHA each fresh clone
  started from, and any seed the tool exposes (most don't have a temperature-0/seed knob for
  agentic CLIs; note that limitation rather than pretending determinism). Store this metadata
  alongside each trial artifact so a scoreboard entry is traceable back to exactly what produced
  it, matching `bench-cross`'s own `cross-report.json` + per-arm output convention.
- Never run any of this against the primary checkout — every trial uses a fresh clone/tempdir,
  per this repo's own hard rule about not running `tm` against its own primary checkout.

## Workflow shape (for eventual implementation)

```
map:  (repo × tool) trial   — Sonnet runner subagent, isolated worktree/tempdir, protocol above
       ↓ (per trial)
grade: reviewer agent        — rubric above, structured JSON output, blind to other tools' trials
       ↓ (all trials)
reduce: scoreboard + tasks   — aggregate table, narrative, and TASKS.md entries from free-text gaps
```

Track A's `bench-cross` runs are simpler (no reviewer needed — pass/fail + score are mechanical)
and can run standalone via `cargo xtask bench-cross`; only Track B needs the full map/grade/reduce
shape above.
