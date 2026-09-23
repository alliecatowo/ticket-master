# Working in this repo

## Always use mise tasks

This repo's `mise.toml` defines the canonical way to build, test, lint and verify — run
`mise run <task>` (or `mise tasks` to list them) instead of hand-rolling the equivalent `cargo`
invocation. They exist so build-concurrency limits, `-p` scoping, and which gate actually runs
never drift between sessions:

- `mise run build` — build just the `tm` binary (fast default).
- `mise run build:all` — build the whole workspace.
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
  cross-reference — see that check's own doc comment for the full reasoning.
- `mise run verify` — the full gate (fmt check + clippy + `cargo test --workspace` + hygiene).
  **Run this before considering any change done**, not just a crate-scoped test pass.
- `mise run check-drift -- <sha>` — advisory, post-merge only, **not** part of `verify`: flags an
  unchanged cardinality assertion (`assert_eq!(Enum::X.len(), N)`-shaped) whose enum's variant
  count changed elsewhere in the same file since `<sha>`. Run it by hand right after merging
  parallel tracks, with `<sha>` set to the commit those tracks branched from. See "Parallel
  tracks against a moving `main`" below for what it does and honestly does not catch.
- `mise run clean` — `rm -rf target`. This machine runs tight on disk; do this after a verify
  pass lands, not mid-build. `mise run worktree:clean` sweeps every worktree under
  `.claude/worktrees/` the same way.
- `mise run tui` — build and launch the ratatui TUI against the current directory's project. It
  opens on the chat, which is Claude Code's chat (D-019 §1 and its "Implemented: chat"): nothing
  typed is a shortcut; on an empty prompt `/` opens commands, `?` the shortcuts panel, `!` shell
  mode (runs in the project root, output joins the conversation), and `←` twice the tickets screen
  (also `/tickets`); `@` completes file paths; Enter sends (queued while a turn runs), Shift+Enter/
  Ctrl+J newline; ↑/↓ history (persisted), Ctrl+R search; Esc interrupts, Esc Esc clears;
  Shift+Tab cycles auto/plan/ask; Ctrl+O transcript viewer; Ctrl+T task checklist; Ctrl+G
  `$EDITOR`; Ctrl+K/U/W/Y, Alt+B/F/D, Ctrl+_ edit; permission prompts take 1/2/3; `/resume`,
  `/compact`, `/model`, `/status`, `/cost`, `/init`, `/bg`; quit is Ctrl+C twice, Ctrl+D on an
  empty prompt, or `/exit`. The
  tickets screen is Claude Code's `claude agents` view with tickets as rows
  (`docs/decisions/D-019-claude-code-parity-shell.md` §2 and its "Implemented: tickets screen"):
  groups Needs input / Working / Ready for review / Queued / Completed, a dispatch input at the
  bottom (Enter creates and queues a ticket), Space peeks, Enter/→ attaches the chat, Ctrl+X twice
  cancels, Space opens a peek whose numbered options `1`/`2` accept/reject a submission, retry an
  escalated ticket (with or without guidance), or queue a draft, Ctrl+B the Kanban board, `?` shortcuts,
  Esc back (no plain letter is a shortcut: typing always goes to the dispatch input). While
  the TUI is open a scheduler runs in-process (`sched::spawn_background_runner`), so dispatched
  tickets get worked; the header says so if it could not start. `cargo run -p tm-tui --example
  chat_demo` plays a scripted turn (every tool shape, an inline diff, long and failing commands,
  a permission prompt) through the real chat screen, for looking at rendering without a model.
  If your shell inherited `CARGO_TARGET_DIR` from a parent session, prefix builds with
  `env -u CARGO_TARGET_DIR` so a worktree builds into its own `target/` (a worktree-isolated
  subagent's command guard refuses `env -u …`; use `unset CARGO_TARGET_DIR && mise run …` there,
  in the same command, since shell state doesn't carry between calls).
- `tm tickets` opens the TUI straight onto the tickets screen (Esc goes to a fresh chat);
  `tm tickets --json` prints open tickets as a JSON array and exits (`--all` adds closed and
  cancelled), like `claude agents --json`, and never creates a project just to print `[]`.
- `mise run dev` — same, with `RUST_LOG=tm=debug,tm_core=debug,tm_agent=debug` piped to
  `/tmp/tm-dev.log` instead of the alt-screen (so debug output doesn't corrupt the TUI's frame).
- `mise run doctor` — `tm doctor` against the current directory.
- `tm serve [--open] [--no-workers] [--web-dir DIR]` — the HTTP API plus the web client at
  `/app/` (build it first: `pnpm -C clients/web install && pnpm -C clients/web build`). It also
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
  (`docs/decisions/D-021-capacity-wait-is-not-a-failed-attempt.md`).
- `mise run docs:wiki` — regenerate `docs/wiki/` (`tm wiki generate`); pass `-- --dry-run` to
  preview without writing (see "Navigation" below).

Every build/test/clippy call in these tasks is already capped at `-j 2` — this is an 8GB Mac,
concurrent full-workspace compiles have caused real disk-space incidents. If you're driving
several agents/worktrees at once, don't override that cap.

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
  failed the task (`TmError::TurnFailed`), 4 when it ran out of budget; `--json -p` prints one
  result object (outcome, text, model, tokens, steps). Chatting never creates a ticket (D-017).
- Ticket lifecycle from the CLI: `tm ticket new "<objective>"` (starts a global-scope project if
  none exists; new tickets get `Authority::worker()`), `tm ticket activate <T>` (draft -> ready),
  then `tm run <T>` (activates a draft itself, and prints each step live) or `tm sched run`. `tm
  run` reports a failed attempt as an error (exit 2), not as "finished". A submitted ticket waits
  for `tm ticket accept <T>` or `tm ticket reject <T> --reason "..."`; an escalated one (out of
  attempts) for `tm ticket retry <T> [--guidance "..."]`, which gives it a fresh round of attempts
  and appends the guidance to its objective. All three are human-only, and the tickets screen and
  `tm serve`'s `/tickets/{id}/transition` offer the same three.
- `tm run <ticket> --worktree` isolates one delegated run in a real `git worktree` (a fresh branch
  off `HEAD`, under `<state_dir>/worktrees/<ticket>-<suffix>/`) instead of the main checkout —
  requires a repo-scoped project backed by a real, non-bare git repository with at least one
  commit (`docs/decisions/D-012-run-worktree-isolation.md`). The worktree is removed automatically
  only once the run is *confirmed* to have reached a forward-progress ticket state
  (`Submitted`/`Verifying`/`Auditing`/`Closed`); anything else — a retry, an escalation, a
  `RUN_TICKET_MAX_WAIT` detach — leaves it on disk with the `git worktree remove --force` command
  to clean it up by hand printed alongside.

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
constraint on this machine: a worktree's `target/` grows to ~8GB, so run `mise run worktree:clean`
after merging and keep concurrent heavy builds to about two.

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
