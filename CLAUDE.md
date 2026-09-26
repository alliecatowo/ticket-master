# Working in this repo

## Always use mise tasks

This repo's `mise.toml` defines the canonical way to build, test, lint and verify — run
`mise run <task>` (or `mise tasks` to list them) instead of hand-rolling the equivalent `cargo`
invocation. They exist so build-concurrency limits, `-p` scoping, and which gate actually runs
never drift between sessions:

- `mise run build` — build just the `tm` binary (fast default).
- `mise run build:all` — build the whole workspace.
- `mise run build:release` — optimized release build of just the `tm` binary (what the release pipeline ships).
- Provider/auth/config overhaul: `workflows/provider-overhaul.workflow.js` is a bounded
  Open Dynamic Workflow run using isolated worktrees and `openai/gpt-6-luna`; follow
  `workflows/README.md` before launching it. Only the integration worker builds,
  and its final commit is reviewed and verified in the primary checkout before a release.
- Releases: `.github/workflows/release.yml` runs on `v*` tags and attaches `tm-<target>.tar.gz` per OS to the GitHub Release (`git tag vX.Y.Z && git push origin vX.Y.Z`). The root README's install section names those assets — keep the two in sync.
- `mise run release` — builds a release tarball locally: `dist/tm-<target-triple>.tar.gz`
  (`bin/tm` + `share/tm/web/` + README-INSTALL.md) plus a `.sha256`, refusing to ship a
  `.env`/`.tm` path (`scripts/release.sh`). `.github/workflows/release.yml` runs the same script
  per OS on a `v*` tag push (or manual `workflow_dispatch`), so a CI-built and a locally-built
  release install identically via `scripts/install.sh`. Pass `-- --publish` to also `gh release
  create` — don't run `--publish` yourself, only the orchestrator publishes from main.
- `mise run test` — `cargo test --workspace`.
- `mise run test:crate -- <crate>` — one crate's tests.
- `mise run test:otel` — clippy + test `tm-cli`'s opt-in OpenTelemetry export path
  (`--features otel`, D-010). **Not** part of `verify`: enabling `otel` compiles a second full
  `reqwest`/HTTP-client stack (`docs/decisions/D-010-opentelemetry-tracing.md`'s costs section),
  and this machine's `-j 2`/disk constraints mean `verify` shouldn't grow a second dependency tree
  by default. Run this by hand after touching `crates/tm-cli/src/otel.rs`, `main.rs`'s
  `install_tracing`, or the `otel` feature's dependency pins in `crates/tm-cli/Cargo.toml`.
- `mise run test:live-codex-auth` — drives a real `AgentLoop` turn against the real, already
  logged-in Codex CLI's ChatGPT-subscription backend (`tm_auth::CodexSubscriptionOAuth` +
  `tm_provider::providers::codex_chatgpt::CodexChatGptProvider`, D-016, `--features
  live-codex-auth`). Requires a real `codex login` session on this machine and hits the real
  network. **Not** part of `verify`, same reasoning as `test:otel`: this is a real, live external
  call, not something any other machine's `cargo test --workspace` should ever attempt by
  accident — see `docs/decisions/D-016-codex-chatgpt-session-auth-adapter.md`.
- `mise run clippy` — workspace lint, `-D warnings`, matching CI.
- `mise run fmt` — apply rustfmt everywhere (`cargo xtask fmt`).
- `mise run hygiene` — the fast standalone hygiene scan (non-determinism, unwrap/expect,
  network-in-tests, stray `.tm` literals, dangling `D-NNN` decision-doc cross-references — see
  `crates/xtask/src/hygiene.rs`). The last of these catches a real, repeated failure mode: a
  source comment or doc mentions `docs/decisions/D-NNN-*.md` (or bare `D-NNN`) for a number that
  was renamed/renumbered elsewhere (see "Parallel tracks against a moving `main`" below) without
  this cross-reference following it. It only checks that the *number* resolves to some real file
  in `docs/decisions/` (not that the full slug matches the topic), and treats
  `crates/xtask/src/hygiene.rs`'s `DECISION_ID_EXAMPLE_ALLOWLIST` as the escape hatch for the
  handful of places (`SPEC.md`'s own ID-format table, mainly) that use a `D-NNN`-shaped string as
  an illustrative example of the *other*, unrelated `DecisionId` domain type rather than as a
  cross-reference — see that check's own doc comment for the full reasoning. It also flags
  dangling `D-NNN`/`crates/`/`tm_*::` jargon in `tm-cli`'s `--help` text (`args.rs`'s `///` doc
  comments), on the theory that a user-facing help string shouldn't assume repo-internal
  knowledge. It also rejects SPEC section pointers in production string literals and in `args.rs`
  help comments; user-facing wording should explain behavior directly rather than point users at
  internal SPEC numbering.
- `mise run verify` — the full gate (fmt check + clippy + `cargo test --workspace` + hygiene).
  **Run this before considering any change done**, not just a crate-scoped test pass.
- `mise run check-drift -- <sha>` — advisory, post-merge only, **not** part of `verify`: flags an
  unchanged cardinality assertion (`assert_eq!(Enum::X.len(), N)`-shaped) whose enum's variant
  count changed elsewhere in the same file since `<sha>`. Run it by hand right after merging
  parallel tracks, with `<sha>` set to the commit those tracks branched from. See "Parallel
  tracks against a moving `main`" below for what it does and honestly does not catch.
- `mise run clean` — `rm -rf target`. This machine runs tight on disk; do this after a verify
  pass lands, not mid-build. `mise run worktree:clean` force-removes every worktree under
  `.claude/worktrees/` unconditionally — prefer `mise run disk:guard` below for routine cleanup;
  it checks locks, uncommitted changes, busy targets and merge status before removing anything,
  same as it does when run unattended.
- `mise run dogfood` — `scripts/dogfood-smoke.sh`: clones this repo into a scratch dir, builds
  `tm` from that clone's own code, and runs one fixed, small, real ticket to completion under a
  wall-clock bound, printing a one-line verdict (`SUBMITTED in 212s, 380k tokens, 14 tool calls,
  2 files changed`, or a clear failure) plus `git diff --stat` for what it actually changed.
  Makes a real provider call (DevPass, sourced from `.env`, never echoed) and can take several
  minutes — a manual dogfood check, not a CI gate; **not** part of `verify`.
- `mise run disk:guard -- [--dry-run] [--verbose] [--aggressive]` — the periodic disk-space guard
  for `target/` dirs, `.claude/worktrees/` and tm scratch dirs; see "Keeping disk use bounded"
  below for what it does and doesn't remove. `mise run disk:guard:install`/`:uninstall` manage its
  launchd agent.
- `mise run tui` — build and launch the ratatui TUI against the current directory's project. It
  opens on the chat, which is Claude Code's chat (D-019 §1 and its "Implemented: chat"): nothing
  typed is a shortcut; on an empty prompt `/` opens commands, `?` the shortcuts panel, `!` shell
  mode (runs in the project root, output joins the conversation), and `←` twice the tickets screen
  (also `/tickets`); `@` completes file paths; Enter sends (queued while a turn runs), Shift+Enter/
  Ctrl+J newline; ↑/↓ history (persisted), Ctrl+R search; Esc interrupts, Esc Esc clears;
  Shift+Tab cycles auto/plan/ask; Ctrl+O transcript viewer; Ctrl+T task checklist; Ctrl+G
  `$EDITOR`; Ctrl+K/U/W/Y, Alt+B/F/D, Ctrl+_ edit; permission prompts take 1/2/3;   `/resume`,
  `/compact`, `/model`, `/level fast|deep` (switches between a quick chat model and a slower,
  more careful one), `/status`, `/cost`, `/connect`, `/provider`, `/config`, `/init`,
  `/bg`, `/context`, `/todos`, `/search`, `/review`, `/memory` (opens this project's
  `AGENTS.md` in `$EDITOR`), `/export [path]` (saves the conversation as markdown), `/doctor`
  (runs `tm doctor`'s checks inline), `/permissions [mode]` (shows or sets auto/plan/ask, same
  as Shift+Tab), `/workflow [name]` (lists workflows, or starts one as a background ticket);
  `/board`, `/milestones`, `/timeline`, `/deps` (jump straight to that tab of the tickets hub;
  Esc returns to the chat), `/ticket <T>` (prints the ticket's summary inline), `/run <T>`
  (activates and queues it);
  `/stats` (the `tm stats` per-ticket usage table), `/bench [task]` (lists benchmark tasks, or
  runs the matching ones), `/events` (the last 20 events), `/replay <path>` (reruns the attached
  ticket offline from a saved cassette, like `tm run <T> --replay`), `/genesis <prompt>` (starts
  `tm genesis` in the background);
  quit is Ctrl+C twice (the first press closes an open overlay before arming quit), Ctrl+D on an
  empty prompt, or `/exit`. The
  tickets screen is Claude Code's `claude agents` view with tickets as rows
  (`docs/decisions/D-019-claude-code-parity-shell.md` §2 and its "Implemented: tickets screen"):
  groups Needs input / Working / Ready for review / Queued / Completed, a dispatch input at the
  bottom (Enter creates and queues a ticket), Space peeks, Enter/→ attaches the chat, Ctrl+X twice
  cancels, Space opens a peek that includes submitted evidence and check results; its numbered
  options `1`/`2` accept/reject a submission (including one in verification or review), retry an
  escalated ticket (with or without guidance), or queue a draft, Ctrl+B the Kanban board, `?` shortcuts,
  Esc back (no plain letter is a shortcut: typing always goes to the dispatch input). While
  the TUI is open a scheduler runs in-process (`sched::spawn_background_runner`), so dispatched
  tickets get worked; the header says so if it could not start. `cargo run -p tm-tui --example
  chat_demo` plays a scripted turn (every tool shape, an inline diff, long and failing commands,
  a permission prompt) through the real chat screen, for looking at rendering without a model.
  If your shell inherited `CARGO_TARGET_DIR` from a parent session, prefix builds with
  `env -u CARGO_TARGET_DIR` so a worktree builds into its own `target/` (a worktree-isolated
  subagent's command guard refuses `env -u …`; use `unset CARGO_TARGET_DIR && mise run …` there,
  in the same command, since shell state doesn't carry between calls). The tickets hub's tab
  strip (Tickets · Board · Milestones · Timeline · Graph), the fuller slash-command table, the
  daily/planning/serving/more CLI grouping, and the one-set-of-display-labels rule are designed
  in `docs/decisions/D-024-command-surfaces.md`.
- `tm tickets` opens the TUI straight onto the tickets screen (Esc goes to a fresh chat);
  `tm tickets --json` prints open tickets as a JSON array and exits (`--all` adds closed and
  cancelled), like `claude agents --json`, and never creates a project just to print `[]`.
- `tm milestone new "<title>" [--ticket <T>...]` (alias `create`) creates a milestone and attaches
  any given tickets; `tm milestone show <M>` prints its title, state, member tickets with state
  labels, and a done/total count. `tm ticket new --milestone <M>` now errors clearly instead of
  silently dropping the milestone when `<M>` doesn't exist. `tm ticket new`/`tm ticket edit` take
  `--due YYYY-MM-DD` (`--due none` clears it on `edit`); `tm ticket show`/`tm ticket list` show
  it, and `tm milestone show` shows the max due date of its member tickets.
- `tm --help` groups the ~16 commands used day to day — daily (`init`, `status`, `tickets`,
  `ticket`, `run`, `search`, `symbol`, `doctor`), planning (`milestone`, `dep`, `decision`),
  serving (`serve`, `mcp`), plus `provider`/`auth` — and folds the rest of the tree (`lease`,
  `harness`, `bench`, `browser`, `computer`, `attach`, `genesis`, `sched`, `history`, `docs`,
  `workflow`, `mirror`, `templates`, `events`, `project`, `wiki`, plus `sched plan`/`tick`,
  `events replay`/`verify`, and `ticket submit`/`delegate`) out of the listing into a `More
  commands` note at the bottom of `--help` — every one of them still runs exactly as before, just
  not listed by default. `tm search` gained `--exact`/`--semantic` flags (`tm search --exact foo`
  is `tm search --mode exact foo`); `--mode` still works, hidden, and is the only way to reach
  `--mode regex`.
- `mise run dev` — same, with `RUST_LOG=tm=debug,tm_core=debug,tm_agent=debug` piped to
  `/tmp/tm-dev.log` instead of the alt-screen (so debug output doesn't corrupt the TUI's frame).
- `mise run doctor` — `tm doctor` against the current directory.
- `tm serve [--open] [--no-workers] [--web-dir DIR]` — the HTTP API plus the web client at
  `/app/` (build it first: `pnpm -C clients/web install && pnpm -C clients/web build`). An
  installed `tm` (via `scripts/install.sh` or a release tarball) finds the web client
  automatically at `<exe>/../share/tm/web` with no separate build step; lookup order is
  `--web-dir`, then `TM_WEB_DIR`, then that installed layout, then `clients/web/dist` in a source
  checkout. It also
  works ready tickets in-process, like the TUI does, so a ticket created and activated from the web
  client actually runs; `--no-workers` turns that off, and `GET /health`/`GET /state` report
  `workers: true/false` (false also when the runner couldn't start). A `POST /tickets` with only
  `kind`, `objective` and `actor` gets the same worker defaults as `tm ticket new`. `GET
  /tickets/{id}/events?after=<seq>&limit=<n>` pages one ticket's history (`next` is the cursor,
  `null` at the end); `GET /events` resumes from `Last-Event-ID` when a reconnect sends it and
  sends keep-alive comments while idle; a `reject` with a blank reason is a 400 (and refused by
  `Store::reject` for the CLI too).
- The scheduler acts as `system`, not as you: leases, attempt starts, retries and escalations from
  `tm sched run`/`tm sched tick`/`tm serve`/the TUI's in-process runner are recorded under
  `ParticipantId::system()` (`sched::scheduler_actor`). `tm run <T>` stays yours, since you asked
  for that run. A provider at capacity (`RouteDecision::Wait`) makes the agent loop wait, up to
  5 minutes, instead of failing the attempt
  (`docs/decisions/D-023-capacity-wait-is-not-a-failed-attempt.md`).
- `tm mcp [--no-workers]` — the project as an MCP server over stdio (newline-delimited JSON-RPC),
  negotiates protocol versions `2024-11-05`, `2025-03-26`, and `2025-06-18` with the client, and
  defaults to the latest supported version for an unknown request. A Claude Code session can
  delegate to tm workers: `claude mcp add --transport stdio tm -- tm
  mcp` (add `--scope project` to share it via `.mcp.json`). Tools: `ticket_list`, `ticket_show`,
  `ticket_dispatch` (objective in; creates a ticket with `tm ticket new`'s defaults, activates it,
  actor `agent:mcp/<clientInfo.name>`), `search_exact`, `search_hybrid`, `search_regex`,
  `search_semantic`, `symbol_def` (now returns an `id` that round-trips into `symbol_references`/
  `symbol_callers`/`symbol_callees`), `symbol_outline`, `symbol_references`, `symbol_callers`,
  `symbol_callees`, `history_why`, `history_search`, `history_deleted`. No accept/reject/retry:
  those stay human-only. It resolves the project from
  Claude Code's working directory like any subcommand, so one must already exist there (`tm
  init`), or pin one with `-- tm --project DIR mcp`; workers run at the project root either way;
  it runs the scheduler in-process like `tm serve`, and
  a `human_required` ticket fails its attempt rather than prompting (stdin/stdout are the
  protocol). Stdout carries JSON-RPC only; log to stderr. `tm-mcp-server` (crate `tm-mcp`) is
  the older standalone form: explicit `--project-root`/`--state-dir`, `Content-Length` framing,
  no workers.
- `tm acp` — the project as an ACP agent over stdio (newline-delimited JSON-RPC), so an
  ACP-speaking client (Zed, or any other) can connect and hold a conversation against real
  project state: `initialize`, `session/new`, `session/prompt`. The backend
  (`tm_acp::ProjectAgentBackend`) answers a prompt with a live summary of the project's tickets
  from the same already-open `tm_core::Store` every other subcommand shares — proving this is
  wired to real state, not a full coding-agent turn (that richer backend is future work; see
  `tm-acp`'s own doc comments for the seam it would plug into). It resolves the project the way
  every other subcommand does, so pin one with `--project DIR` if none exists at the working
  directory. Stdout carries JSON-RPC only; log to stderr.
- `tm genesis` activates its committed ticket graph after compilation so `tm sched run` can work
  it. Once V0 or V1's milestone isn't closed yet, or after the maturity gate fails once, it prints
  the next step and exits 0. Use `tm genesis --run` to run the scheduler in-process until V0 closes
  (or a ticket escalates), then continue Genesis automatically. Re-run with `tm genesis --resume` to
  continue a stopped run from its
  persisted stage instead of starting over (an omitted `--prompt` auto-detects a persisted
  snapshot the same way; explicit `--resume` with no snapshot reports that there is no stopped
  run and suggests starting with `tm genesis --prompt "…"`, before checking provider credentials.
  See
  `docs/decisions/D-027-genesis-cli-stops-for-work.md`. Under
  `TM_TEST_MOCK_PROVIDER=1` (the same offline test hook `tm run` and chat turns honor via
  `crate::agent::TEST_MOCK_PROVIDER_ENV`), `tm genesis` skips provider resolution entirely:
  `resolve_genesis_provider` returns a `MockProvider` scripted with
  `tm_genesis::fixtures::offline_sequence()`, so it runs in a scratch tempdir with no credential
  or local model, and still stops per D-027 at the first unclosed milestone or failed maturity
  gate.
- `mise run docs:wiki` — regenerate `docs/wiki/` (`tm wiki generate`); pass `-- --dry-run` to
  preview without writing (see "Navigation" below).
- `tm docs attest <doc> --note "..."` closes a `Maintained`/`Human` doc's open review ticket with
  a human attestation (`SPEC.md` §9's "a human closes with an attestation that is itself
  evidence") — the only path back to `Fresh` for such a doc. `<doc>` is the id or path shown by
  `tm docs list`; it must already be `Reconciling` (via `tm docs reconcile`) — attest itself
  drives that ticket the rest of the way (activate, lease, submit with the note as evidence) if
  it's still sitting `Draft`/`Ready`/etc, the state a fresh `tm docs reconcile` actually leaves
  it in. Records the note as `EvidenceKind::HumanAttestation` evidence against the ticket
  (tagged with `HEAD`'s commit sha when the project is a git repo, in the evidence artifact's
  `meta` — `DocRow` itself has no persisted per-doc commit column yet), closes it via the same
  human-only `Store::accept` path `tm ticket accept` uses, and persists the doc's `Fresh`
  transition (`Store::reconcile_doc`) so a following `tm docs list` reads it back.
- `tm bench report <report.json> [--out FILE]` renders a saved `BenchmarkReport` (from `tm bench
  run --out`) as markdown: an aggregate-score heading, then a per-task table of pass/fail, score,
  cost, tool calls and wall time. Prints to stdout by default; `--out` writes the markdown to a
  file instead. The rendering (`crates/tm-cli/src/bench_report.rs`) is a pure function of the
  parsed report.
- `tm bench run --live` drives each matched task through a real ticket run instead of replaying a
  fixed script: it copies the task's fixture into a scratch directory with a fresh git history,
  creates and activates a worker ticket, runs it through the same dispatcher `tm run` uses, then
  scores the task by actually running its `test_command`. A cassette is recorded next to the
  run's own report, and real cost/tool-call/token numbers replace the scripted path's
  byte-length proxy. Slower, non-deterministic, and a real provider call unless
  `TM_TEST_MOCK_PROVIDER=1` is set; never part of `mise run verify`. See
  `docs/decisions/D-032-live-benchmark-mode.md`.
- `mise run bench:cross -- [--tools tm,opencode,codex,claude] [--task <filter>] [--out <dir>]
  [--model <provider/model>] [--task-timeout <secs>] [--max-cost-usd <amount>]
  [--tm-binary <path>] [--real-claude-auth] [--help|-h]` (`cargo xtask bench-cross`) runs the same
  `bench/tasks/*.toml` suite
  identically through `tm`'s own live path and through configured external coding CLIs, and
  reports pass/fail, score, model, tokens, cost, tool calls and wall time for every tool side by
  side, where the tool reports those usage details
  (`crates/xtask/src/bench_cross.rs`). Defaults to `--tools tm` alone; `opencode`/`codex` read
  their own already-cheap credential from the environment, but `claude` is refused unless
  `--real-claude-auth` is also passed, so a real, metered Claude/Anthropic API call is never the
  accidental default. Every non-`tm` adapter runs under a real edit/bash permission posture
  (`claude -p --permission-mode bypassPermissions`, `codex exec --sandbox workspace-write`,
  `opencode run --auto`) rather than a headless no-op that used to make `tm` win by default; all
  adapters receive closed stdin so tools cannot wait indefinitely for piped input;
  `--model` pins every tool (including `tm`, via a scratch `providers.toml` role candidate) to the
  same model; `--task-timeout` kills a hung adapter's whole process group and marks that pair
  `TIMEOUT`; `--max-cost-usd` stops scheduling further pairs once cumulative reported spend
  reaches it. The report header states each tool's permission posture, `--version` output, the
  pinned model and any caps, so the numbers below it read as a fair, labeled comparison rather
  than an unlabeled one. The `tm` adapter resolves its own binary rather than trusting a bare `tm`
  on `$PATH` (which a normal from-source dev workflow never sets): `--tm-binary <path>` overrides
  it explicitly (resolved to an absolute path, since `TmAdapter` spawns with `.current_dir` set to
  each task's own scratch fixture), but only when it exists -- a mistyped or stale `--tm-binary`
  warns on stderr and falls through instead of being spawned as given; if the flag is absent or
  not a real file it looks for a `tm`/`tm.exe` next to this `xtask` binary's own `current_exe()`
  (the common case, since `xtask` and `tm` land in the same `target/<profile>/` directory),
  falling back to a bare `"tm"` `$PATH` search only if neither resolves to a real file. `--help`/
  `-h` prints usage and exits without discovering tasks or spawning anything. Opt-in, real runs only, never part of `mise run verify`/`hygiene`; unit
  tests exercise the harness against a scripted fake adapter (and, for the timeout path, a real
  `sleep` subprocess) instead. See `docs/decisions/D-033-cross-tool-benchmark.md` and
  `bench/README.md`.

`tm-codeintel`'s semantic search can now use real Potion static embeddings
(`minishlab/potion-code-16M-v2`, via `model2vec-rs`) instead of the hash stand-in, but only
through real command paths using `CodeIntel::open_at_auto` (picks Potion when it's already cached
locally, hash otherwise, never downloads); `CodeIntel::open`/`open_at` still default to the hash
embedder unconditionally so existing tests stay network-free. The selected embedder identity is
stored with the index; changing embedders clears stale vectors and re-embeds the corpus on refresh.
Env override `TM_EMBEDDER=hash|potion` (whitespace/case-insensitive), download opt-out
`TM_EMBEDDER_DOWNLOAD=0`. See `docs/decisions/D-025-potion-semantic-embedder.md`.

Plain `tm run` progress reports include the attempted tool action. Parse failures also show a
short diagnostic and suggest retrying the operation (checking its input if it fails again);
inconsistent-state failures show their short diagnostic and suggest one retry, then reporting
the failure if it persists. Detailed errors remain in logs.

Every build/test/clippy call in these tasks is already capped at `-j 2` — this is an 8GB Mac,
concurrent full-workspace compiles have caused real disk-space incidents. If you're driving
several agents/worktrees at once, don't override that cap.

## Keeping disk use bounded: `disk:guard`

This machine's disk has hit 100% for real — `target/` growing unbounded (primary + every
worktree, ~8-11GB each), stale worktrees piling up under `.claude/worktrees/`, and leftover probe
scratch dirs under `$TMPDIR`/`/tmp` are the three causes seen so far. `scripts/disk-guard.sh`
(POSIX `sh`, no non-macOS-base dependencies) is the guard against all three, and it's meant to run
unattended, not just by hand — prefer it over `mise run worktree:clean`/`mise run clean` for
routine cleanup, both of which act unconditionally rather than checking locks/busy/merge state
first.

- `mise run disk:guard -- [--dry-run] [--verbose] [--aggressive]` runs it once. `--dry-run` prints
  what it *would* remove without touching anything; always use this to check before an unattended
  install, and whenever changing the script itself. `--aggressive` (or free disk already below
  30GB, automatically) shortens every target dir's idle threshold to 15 minutes instead of the
  normal 2h (worktrees)/6h (primary).
- `mise run disk:guard:install` installs it as a launchd agent
  (`~/Library/LaunchAgents/com.ticketmaster.disk-guard.plist`, `com.ticketmaster.disk-guard`)
  running every 30 minutes, pointed at the primary checkout's own copy of the script — run this
  from the primary checkout, not a worktree, and only when you've actually decided to install it
  (it's not installed as a side effect of anything else). `mise run disk:guard:uninstall` reverses
  it.
- What it removes, every run: an idle `target/` dir (build output only — never anything else in a
  checkout) in the primary checkout or any non-`odw-*` worktree, once idle past its threshold and
  not busy (a
  cargo/rustc process cwd'd inside it specifically — not just any process, since an editor, MCP
  daemon, or shell (including `rust-analyzer` itself) routinely sits in the primary checkout and in
  `tm-integrate` without that meaning a build is in flight; a `.cargo-lock` held open; or recent
  `.fingerprint`/`deps` activity); a worktree under `.claude/worktrees/` once it's unlocked, at
  least 90 minutes old (its own git admin dir's HEAD/index/logs/HEAD show no activity more recent
  than that — protects a just-branched worktree an agent hasn't committed to yet, since lock status
  alone doesn't reliably signal "still working here" for this harness's workflow worktrees), has no
  uncommitted/untracked/non-allow-listed-ignored changes (checked with `git status --ignored`, not
  plain `--porcelain` — a gitignored-but-stateful file like a real `.tm/` dir, a sqlite `*.db*`, or
  a symlinked `.env` blocks removal same as a tracked change would; only `target/` and
  `node_modules/` are allow-listed to still count as clean), its HEAD is contained in `main` or
  `integrate`, and no process at all has its cwd inside it; and tm scratch dirs
  (`$TMPDIR/tmp.*`, `/tmp/tm-*`) older than 6 hours that look like tm's own (a `.tm` dir, a
  `projects/` dir, a `tm` binary, a `project.db`/`index.db` directly at its own root, a bare cargo
  `target/`-shaped dir at its own root, or a git repo
  whose only commits are from the last day).
- What it never removes: anything under a worktree named `odw-*` — the worktree itself, and its
  `target/` too (owner decision — ODW's own in-progress state stays fully untouched, not just the
  worktree directory, until that work lands) — or the `tm-integrate` worktree itself (its `target/`
  is still fair game and does get cleaned on the normal idle schedule), a locked worktree, a
  worktree younger than 90 minutes, a worktree with local changes (tracked or a non-allow-listed
  ignored file) or an unmerged HEAD, or anything a live process has its cwd inside.
- Every run appends one line to `~/Library/Logs/tm-disk-guard.log`: timestamp, free space before
  and after, and what was removed (or `(nothing removed)`).
- The primary checkout's `target/` deliberately does *not* stay pinned just because an open Claude
  Code session has `rust-analyzer-lsp` active there (see "Code intelligence" below):
  `rust-analyzer` itself is excluded from the busy-process check on purpose, since this repo
  enables that plugin project-wide and a live session's own language server has its cwd in the
  primary checkout essentially all the time — counting it as busy would have pinned the primary's
  `target/` permanently, defeating "target clears periodically". `rust-analyzer`'s actual writes to
  `target/` go through a `cargo` child process (still caught by the cwd check) and touch
  `.fingerprint`/`deps` (caught by the mtime check), so a target mid-flycheck is still protected;
  it just isn't pinned forever by the editor session alone.

## Codebase search: zvec-grep is indexed for this repo

This workspace has a persistent zvec-grep index (`.zvec-grep/`, gitignored, local embedding
model — no network dependency). Use it exactly per the global routing rules in
`~/.claude/CLAUDE.md`: `zvec_grep_rg` for an exact symbol/string/path, `zvec_grep_search` for
"where does X happen" / architecture / cross-file questions. The index has a live watcher, so it
stays current across edits without a manual rebuild. Don't rebuild or drop it without asking —
that rule is global, not repo-specific, and still applies here.

The server itself is registered project-scope in `.mcp.json` (`zg server --stdio`) and
pre-allowed in `.claude/settings.json`'s `permissions.allow` (`mcp__zvec_grep__*`) — both
repo-tracked, so a fresh clone gets the tool without a manual `claude mcp add` and without a
permission prompt on first use. It still needs the `zg` binary itself on `$PATH`, which
`mise.toml`'s `"npm:@zvec/zvec-grep"` tool entry now provides — `mise install` in a fresh clone
gets it automatically, nothing to run by hand.

## Code intelligence: LSP plugins for every language in this repo

This repo is polyglot — `crates/` (Rust, the primary surface), `clients/ts` + `clients/vscode` +
`clients/web` (TypeScript/JavaScript), and `clients/macos` (Swift). Each has a matching Claude
Code LSP plugin enabled project-scope in `.claude/settings.json`'s `enabledPlugins` (repo-tracked,
so a fresh clone gets all three without a manual `claude plugin install`/`enable`); once active (a
fresh session, or `/reload-plugins` in an open one) each gives Claude a native LSP tool for that
language — automatic diagnostics after every edit, plus go-to-definition/references/hover/
call-hierarchy, sourced from the same language server an IDE would use — instead of falling back
to grep-shaped heuristics for that language.

- **`rust-analyzer-lsp`** — covers `crates/`. Needs `rust-analyzer` in `$PATH`; `mise.toml`'s
  `[tools]` already lists it, so `mise install` provides it. `mise run lsp` is the
  fallback/manual path when you want a one-shot full-workspace `rust-analyzer diagnostics` CLI
  dump instead of the live plugin — informational only (it exits non-zero on *any* diagnostic,
  including the benign `#[cfg(test)]` "inactive-code" note every test module produces, so read the
  output, not the exit code).
- **`typescript-lsp`** — covers `clients/ts`, `clients/vscode`, `clients/web`. Needs
  `typescript-language-server` and `typescript` on `$PATH`; `mise.toml`'s `"npm:typescript-
  language-server"`/`"npm:typescript"` tool entries provide both via `mise install`.
- **`swift-lsp`** — covers `clients/macos`. Needs `sourcekit-lsp` on `$PATH`, which ships with the
  Xcode toolchain (already present on this machine at `/usr/bin/sourcekit-lsp`) — not something
  mise can install, since it isn't a package-manager-distributed binary; a machine without Xcode
  (or the Command Line Tools) needs that installed separately before this plugin can work.

All three can be memory-heavy on a large workspace; if one causes trouble on this machine's 8GB,
`/plugin disable <name>` for just that language and fall back to `tm-codeintel`'s heuristics
(Rust) or grep/zvec-grep (TypeScript/Swift) there — don't disable the others along with it.

## Logging and debugging

- `RUST_LOG` controls `tracing` output on stderr (`tracing_subscriber::EnvFilter`, see
  `crates/tm-cli/src/main.rs`'s `install_tracing`). `mise run dev` sets a sane default; for a
  narrower trace use e.g. `RUST_LOG=tm_agent=trace,tm_scheduler=debug`.
- The TUI runs in the alt screen, so raw `println!`/`eprintln!` debugging will corrupt the frame
  — use `tracing::debug!`/`info!` (routed to stderr, invisible inside the alt screen, visible
  when redirected to a file as `mise run dev` does) rather than print statements when working on
  `tm-tui`/`tm-cli`'s TUI path.
- Every session's real state lives under `Project.state_dir` (see
  `docs/decisions/D-003-project-scope.md`) — `sqlite3 <state_dir>/project.db` is a legitimate way
  to inspect what actually got written, and `tm events` reads the same log a UI would.
- `tm sched run`/`tm run` pop a real desktop notification on `approval.requested`/
  `ticket.escalated` (`docs/decisions/D-007-desktop-notifications.md`, `crates/tm-notify`). Set
  `TM_NOTIFY=0` (or `false`/`off`/`no`) to opt out in headless/CI/server contexts — a notification
  call there is pointless at best and, without a controlling terminal for the OSC 9 fallback, just
  noise on stderr.
- `tm ticket fork <T> --at <seq>` checkpoints a ticket: a new ticket lineage starting from `T`'s
  materialized state as of `seq` (objective/kind/authority/budget/goal — never `T`'s current state,
  never `T`'s history), with the provenance itself a real, hash-chained `ticket.forked` event
  (`docs/decisions/D-008-ticket-checkpoint-fork.md`). Separately, `tm sched run`/`tm run` capture a
  `git stash create`-style workspace snapshot (pinned under `refs/tm/snapshots/<sha>`, recorded as
  an `ArtifactKind::WorkspaceSnapshot`) whenever a scheduler-dispatched turn produces a real patch —
  same decision doc, "What this costs" section has the real gaps (untracked files never captured,
  only one call site wired).
- OpenTelemetry span export is opt-in and off by default (`docs/decisions/D-010-opentelemetry-
  tracing.md`): build `tm-cli` with `--features otel` (`mise run test:otel` builds+lints+tests it)
  and set `TM_OTEL_ENDPOINT` to a full OTLP/HTTP traces endpoint (e.g.
  `http://localhost:4318/v1/traces` — the full path, not a bare host:port) to export to a
  collector, alongside the stderr `fmt` layer rather than instead of it. Neither a normal
  `cargo build -p tm-cli`/`mise run build` nor `TM_OTEL_ENDPOINT` alone does anything; both the
  feature and the env var are required. As of D-010, this exports nothing yet in practice: the
  OTel layer is span-shaped and this workspace has zero `#[instrument]`/`*_span!` call sites, only
  bare `tracing::info!`/`debug!` events — adding real instrumentation is a separate follow-up.
- Conversations are saved per project: `tm -c` continues the latest, `tm -r` lists saved ones,
  `tm -r S-12` resumes one (D-019). The TUI runs the scheduler in-process while open
  (`sched::spawn_background_runner`), so tickets it queues get worked.
- `tm -p "<prompt>"` is the scriptable one-shot chat turn: exit 0 on a reply, 2 when the agent
  failed the task (`TmError::TurnFailed`), 4 when it ran out of budget, 64 when the command line
  itself is malformed (`args::usage_exit_code`, matching sysexits' `EX_USAGE`) — so a caller
  scripting against the exit code can tell a bad invocation (e.g. an unknown flag) apart from a
  real failed turn. A bare trailing `TEXT` starts the chat with that message in a terminal and runs
  one prompt to completion when input is not interactive. `-p`/`--prompt` remains the explicit
  one-shot form. It is a boolean flag plus a trailing positional `TEXT`, not a
  value-taking option, so flag order doesn't matter (`tm -p --json "x"` and `tm --json -p "x"`
  both work) and `tm -p --help` prints help instead of erroring. `--json -p` prints one
  result object (outcome, text, model, tokens, steps). Chatting never creates a ticket (D-017).
- Provider configuration is project-scoped: `tm init` creates `providers.toml` for role routing
  and `harness.toml` for `HarnessConfig`; `tm harness set` validates and persists a candidate,
  which takes effect after `tm harness promote`.
  Chat defaults live in `default-model.json`. Sessions, background workers, and provider list/status
  use the same effective role-table loader (`providers.toml` → legacy role-shaped `harness.toml` →
  built-in defaults). A configured non-mock `[decider]` runs ticket-creation triage in shadow
  mode and records successful answers as `classify.decided`; the default mock is silent unless
  `TM_DECIDER_SHADOW=1` opts in. `tm provider list/status` show the mock decider as `offline (mock)`
  and identify whether a System One candidate has `AI_GATEWAY_API_KEY`; `tm provider test decider`
  sends one triage question through the configured decider and reports its answer and confidence.
  Decider failures never prevent ticket creation. See
  `docs/providers.md`, D-020 and D-022. Test the real `tm` binary only against a
  fresh `mktemp` project root, never this checkout.
- Ticket lifecycle from the CLI: `tm ticket new "<objective>"` (starts a global-scope project if
  none exists; new tickets get `Authority::worker()`), `tm ticket activate <T>` (draft -> ready),
  then `tm run <T>` (activates a draft itself, and prints each step live) or `tm sched run`. `tm
  run` reports a failed attempt as an error (exit 2), not as "finished". A submitted ticket waits
  for `tm ticket accept <T>` or `tm ticket reject <T> --reason "..."`; an escalated one (out of
  attempts) for `tm ticket retry <T> [--guidance "..."]`, which gives it a fresh round of attempts
  and appends the guidance to its objective. All three are human-only, and the tickets screen and
  `tm serve`'s `/tickets/{id}/transition` offer the same three.
- `tm ticket context <ID>` shows the context pack a ticket's next attempt would be given —
  admitted and dropped sections with token counts — without spending an attempt.
- `tm run <ticket> --worktree` isolates one delegated run in a real `git worktree` (a fresh branch
  off `HEAD`, under `<state_dir>/worktrees/<ticket>-<suffix>/`) instead of the main checkout —
  requires a repo-scoped project backed by a real, non-bare git repository with at least one
  commit (`docs/decisions/D-012-run-worktree-isolation.md`). The worktree is removed automatically
  only once the run is *confirmed* to have reached a forward-progress ticket state
  (`Submitted`/`Verifying`/`Auditing`/`Closed`); anything else — a retry, an escalation, a
  `RUN_TICKET_MAX_WAIT` detach — leaves it on disk with the `git worktree remove --force` command
  to clean it up by hand printed alongside.
- `tm run <ticket> --record <path>` captures every provider call the run makes to a cassette file
  at `<path>` (`docs/decisions/D-028-record-replay-harness.md`) and stores those same bytes as the
  ticket's `ArtifactKind::Transcript` artifact once the run finishes; combinable with `--worktree`.
  `tm run <ticket> --replay <path>` reruns a ticket offline against a cassette instead: the
  ticket's role gets a single `MockProvider` scripted from the cassette and no other provider is
  registered, so no network call is reachable. Once the run finishes it reports how many served
  calls diverged from the recording and how many ran past its end, as JSON under `--json`;
  `--strict-replay` turns either into a hard error.
- `tm harness replay-diff <a> <b>` runs inside a project like every other `tm harness` verb, and
  structurally diffs two session transcript JSON files (the `<state_dir>/sessions/<id>.json` shape
  `tm -r`/`tm -c` already save, or any hand-built/future file with the same `turns` field, plus an
  optional `harness_epoch`): assistant-text mismatches, tool-call name, input, or result
  mismatches, spend deltas, and step- or turn-count differences, one line per finding, or the
  whole result under `--json`. It warns separately when both files carry a harness epoch and they
  differ — today's saved sessions don't stamp one, so that check is proven by unit tests against
  hand-built fixtures rather than by a real file yet. The comparison itself
  (`crates/tm-cli/src/replay_diff.rs`) is a pure function of the two parsed transcripts, with no
  provider or model call involved.
- `tm stats [--by ticket|day|model|tool] [--ticket T]` rolls up local usage from the project's own
  event log: tokens, cost, and tool calls per ticket (default), per UTC calendar day, per
  provider/model pair (`unattributed` when a call predates cost/model attribution), or per tool
  (count, failures, average duration, result size, and largest request). Ticket and tool tables
  include result KB and maximum request tokens; result size is summed from completed tool calls,
  and older events without a recorded size contribute zero. `--ticket T` restricts any of the
  four to one ticket. A
  `0` cost means the serving candidate has no price configured (`not priced`), not that the call
  was free. It folds the whole event log fresh on every run — nothing is cached or persisted
  separately (`docs/decisions/D-030-local-telemetry.md`).

## Background and parallel subagents

Full playbook: `.claude/skills/dispatch-background-agent/SKILL.md`. The rules below are the
load-bearing subset, stated plainly because getting them wrong has already cost real time in this
repo.

**This repo has a real `origin` now** — a private GitHub repo, `git@github.com:alliecatowo/
ticket-master.git`, created 2026-09-22 (`gh repo create alliecatowo/ticket-master --private
--source=. --remote=origin`), with `main` pushed and tracked. Before this, `EnterWorktree`'s
default `worktree.baseRef` (`fresh`, which branches new worktrees from `origin/<default-branch>`)
was a checked no-op with no `origin` to resolve against. That's no longer true, and `fresh`'s real
behavior would now be a problem for how this repo is actually worked in: commits land locally
throughout a session well before any `git push` (this file's own history is full of that pattern),
so `fresh` would silently branch new worktrees from a stale, already-behind `origin/main`, missing
whatever's only local so far. `.claude/settings.json` now pins `"worktree": {"baseRef": "head"}`
explicitly for exactly this reason — verified empirically (a local-only, not-yet-pushed commit,
then a probe worktree confirmed to branch from it rather than from `origin/main`). If this project
ever gains other collaborators pushing from elsewhere, revisit whether `head` is still the more
correct default.

**Compiled dependencies are shared across worktrees through sccache; `target/` is not.**
`mise.toml` sets `RUSTC_WRAPPER=sccache` (installed by `mise install`), which caches compiler
output by content hash, so a new worktree reuses the third-party builds instead of recompiling
~11GB of them. Each checkout keeps its own `target/` for this workspace's own crates. Do **not**
point several worktrees at one `CARGO_TARGET_DIR`: that was tried and is unsafe. Cargo names
path-dependency artifacts by a workspace-relative hash, so two worktrees of this workspace write the
same files and one tree's build can silently link the other tree's code; a subagent hit exactly
that (its build saw an error variant that only existed in the primary checkout). Disk is still the
constraint on this machine: a worktree's `target/` grows to ~8GB. `mise run disk:guard` (see
"Keeping disk use bounded" above) reclaims an idle one on its own schedule, or run it by hand with
`--aggressive` right after merging; keep concurrent heavy builds to about two either way.

**A worktree does not get `.env`, and nothing here auto-copies it in.** Git worktrees only ever
contain tracked files (plus your own uncommitted changes on that branch) — a gitignored file like
`.env` is invisible to `git worktree add` by design, confirmed empirically (a probe worktree had
no `.env` at all). This is left deliberately manual, not automated: `.env` currently holds a real
credential already flagged for rotation (see the top of `AGENT_HANDOFF.md`), and auto-copying it
into every worktree would scatter more live copies of a secret that's already known-exposed rather
than fewer. If a specific subagent genuinely needs a real provider credential inside its worktree
(rare — most work doesn't touch a live provider at all), symlink it in deliberately for that one
task — `ln -s "$(git rev-parse --git-common-dir)/../.env" .env` from inside the worktree — rather
than copying it, so rotating the key once still invalidates every worktree's access rather than
leaving stale copies behind.

- **`isolation: 'worktree'` is mandatory** for any subagent that will edit files here. No
  exceptions for "it's a small change."
- **Never let a subagent run the real `tm` binary against the primary checkout.** Bare `tm` used
  to auto-write a `.tm/` directory into whatever directory it ran in
  (`docs/decisions/D-003-project-scope.md` fixed the product bug), but a subagent doing its own ad
  hoc manual testing running the compiled binary against this repo's own primary checkout instead
  of an isolated tempdir is a process failure that fix doesn't prevent — it happened three times
  in one session, once leaving a real 37MB index behind and breaking the next `mise run verify`
  for whoever ran it. Always a fresh tempdir; never the worktree root, never the primary checkout.
  `.claude/settings.json` has a `PostToolUse` hook on `Bash` that warns once per session when
  `.tm/` exists in the primary checkout — it is a **detector**, not a preventer (it only fires after the fact, and a
  reliable *preventive* hook would need to pattern-match arbitrary shell commands, which is not
  something that can be done without real false-positive/false-negative risk — see that file's
  hook for the one thing that *is* reliably checkable: the artifact, not the command). Don't treat
  its silence as permission to be careless.
- **Worktree isolation can silently fail to hold across a resume.** Two subagents in one real
  session were interrupted mid-task by a rate limit, resumed via a direct message to the same
  agent, and resumed editing directly in the primary checkout instead of their assigned worktree —
  their own final reports flagged it only after the fact. If a subagent's report expresses *any*
  uncertainty about which checkout it's in, stop it before any further write and have it confirm
  with `git rev-parse --show-toplevel` (compared against its assigned `.claude/worktrees/<name>`
  path) before resuming.
- **Adversarial self-review before declaring done, as a real instruction, not just a finding.**
  Subagents that ran a deliberate skeptical pass over their own finished work (this session's
  `advisor` tool, when available, is exactly this) caught real bugs that the implementation pass
  missed. Ask it to do the work, then re-check the work distrustfully, before handing back.
- **The hygiene checker's `#[cfg(test)]` region tracker is a one-way latch**, not a real parser:
  it flips permanently "in test code" at the first `#[cfg(test)]`/`#[test]` line in a file and
  never flips back (`crates/xtask/src/hygiene.rs`'s `TestRegionTracker`, deliberately simple —
  see its own doc comment for why). A test-only helper placed above real production code in the
  same file will make everything below it silently exempt from the hygiene checks; test code
  placed before the file's real `mod tests` block gets exemptions too early. Put
  `#[cfg(test)]`-gated code at the bottom of the file.

### Parallel tracks against a moving `main`

The real, historically-verified failure mode: two tracks each add a variant to the same enum from
different base commits. Git auto-merges the enum cleanly (no conflict marker), but a test
elsewhere in the same file that hardcoded arithmetic derived from the old variant count — neither
branch's diff touched it — is now silently wrong, and only surfaces as a test failure in a full
`mise run verify` after merging. This happened for real: `crates/tm-context/src/tokens.rs`'s
`SectionKind` gained a variant during the B-10/B-14 merge (`2f86cfe`), and commit `dba4e7f` is the
by-hand fix for the two tests that assumed the old count.

`mise run check-drift -- <sha>` (see above) mechanically catches exactly one shape of this: an
unchanged `assert_eq!(Enum::X.len(), N)`-style line where `N` is the enum's old variant count. It
correctly does **not** claim to catch the harder half of the real incident — a test whose expected
value is a *derived* computation (`floor(800 / 9) = 88`) three algebraic steps from the enum's
cardinality, with no `.len()` call and no enum name anywhere on the line. Catching that
generically would need real dataflow analysis, not a grep-shaped structural check; a broader
pattern match was tried and rejected because on the real file it flagged on the order of thirty
lines, which is noise, not triage. The honest fix for that harder half is a **convention**: when a
test's expected value is derived from a collection's cardinality, compute the divisor from
`X::PRIORITY_ORDER.len()` (or equivalent) at test time instead of hardcoding the quotient, so the
value tracks the enum instead of relying on someone remembering to update it. Prefer that pattern
in new tests going forward.

## Navigation

- `docs/audit-2026-09-18-fable.md` is a comprehensive, file:line-anchored implementation status
  audit — read it before assuming a SPEC section is done or missing.
- `SPEC.md` section 0 is binding project philosophy, not aspirational prose: deterministic
  machinery stays pure/unit-testable with no network and no model calls; the hygiene task
  enforces the sharpest edges of this mechanically.
- Prefer `symbol.*`/`search.hybrid`-shaped retrieval (or zvec-grep, above) over blind
  multi-file `Read` sweeps — this workspace's own `tm-codeintel` crate exists because that's
  faster and more precise than grepping cold.
- `docs/wiki/` is generated documentation (`SPEC.md` §26, B-14): architecture/<crate>, decisions,
  history/<path>, tickets, and glossary pages assembled from live project state. Regenerate it
  with `mise run docs:wiki` (`tm wiki generate`; add `--dry-run` to preview without writing) —
  don't hand-edit a page unless you also flip its front matter to `mode = "maintained"`/`"human"`,
  or the next regeneration silently overwrites it.

## Keep documentation honest as you change things

A change that lands without its documentation landing with it is half-finished, not done. When
you touch behavior this repo documents, update the same turn, not a follow-up:

- A new/changed decision (an architecture choice, a tradeoff, something a future session would
  otherwise have to re-derive) gets a `docs/decisions/D-NNN-*.md` in the existing format
  (`docs/decisions/D-002-terminal-ui-stack.md` is the template: Status/Date/Supersedes, then
  Context/Decision/Why/"What this costs, stated plainly").
- A SPEC.md section whose actual implementation now disagrees with what's written gets corrected
  or pointed at the decision doc that supersedes it — don't let SPEC.md silently drift out of
  sync with reality the way parts of it already have (see the fable audit's "Part 3:
  corrections to the baseline").
- `docs/audit-2026-09-18-fable.md` is a point-in-time snapshot, not a living document — don't
  edit it after the fact to mark things done; a stale audit is still useful as history, a
  silently-edited one isn't. Track new status in the SPEC/decision docs instead.
- New CLI verbs, mise tasks, or dev-workflow changes get a line in this file, not just in the
  code's own `--help` output — this file is what a fresh session reads first.

## This harness should keep improving itself

Treat friction — a repeated manual fix, a stray file some track created by mistake, a check that
should have caught something but didn't, a convention two agents independently reinvented
slightly differently — as a signal to fix the harness itself, not just the immediate symptom.
That means: add the missing hygiene check, write the missing `SKILL.md`, add the missing mise
task, correct this file, rather than only patching the one instance. This is a standing
directive, not a one-time cleanup — it doesn't expire when the current backlog does.

## Be liberal with research before asking

You have `WebSearch`/`WebFetch` — use them proactively when a task depends on an external
service's real, current details (an API's base URL, auth flow, request/response shape, a
provider's model names) rather than defaulting to asking the user for something a few minutes of
research could resolve. This applies concretely to this repo's own provider integrations
(DevPass, the LLM-gateway credential added this session, any future one): if you're given a
credential but not a base URL, or asked to wire up a named model you don't recognize, search for
it first. Still ask when research genuinely can't resolve it (private/internal services, ambiguous
naming with no authoritative source, anything where guessing wrong risks real cost or a security
mistake) — the point is to not skip the cheap step, not to stop asking altogether.
