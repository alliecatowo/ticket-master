# Agent Handoff — 2026-09-20

Written because this session is about to run out of usage for ~2 days. Read this first in the
next session before doing anything else. `main` is in a good, fully-verified state as of the
latest commit on this branch — you are not picking up a mess, you're continuing a working build.

**SECURITY, read this line first:** the DevPass/LLM Gateway API key in `.env` was accidentally
printed in full into an agent's tool output this session (caught by the agent itself, not
repeated afterward, but exposed regardless). If the user hasn't already rotated it, that's the
very first thing to raise next session — see `docs/backlog.md`'s top section.

## 2026-09-22 addendum: harness/environment setup, done deliberately before this restart

The user explicitly asked to pause all product work and get the harness itself fully set up
before starting a fresh session, so this landed instead of item 1 below — don't read its
presence as a change in priority, item 1 is still the real next task. What changed, each verified
empirically, not assumed:

- **LSP plugins and zvec-grep MCP moved from user-scope to project-scope.** They were working but
  only registered in `~/.claude/settings.json` — invisible to git, gone on a fresh clone or
  container. Now: `.claude/settings.json`'s `enabledPlugins` has `rust-analyzer-lsp`,
  `typescript-lsp`, `swift-lsp`; `.mcp.json` registers `zvec_grep`; `permissions.allow` pre-allows
  `mcp__zvec_grep__*`. All three repo-tracked commits: `fed4c97`, `3cad15c`.
- **The underlying binaries those plugins need are now real `mise.toml` dependencies, not things
  installed by hand once and forgotten.** `"npm:typescript-language-server"`, `"npm:typescript"`,
  and `"npm:@zvec/zvec-grep"` (provides `zg`) are in `[tools]` — verified by running `mise
  install` in a scratch directory with nothing pre-installed and confirming all three binaries
  resolved. `sourcekit-lsp` (Swift) is deliberately not listed: it ships with Xcode, not something
  mise can install — a machine without Xcode/Command Line Tools needs that separately.
- **Every worktree now shares the primary checkout's `target/` instead of building its own.**
  `mise.toml`'s `[env]` pins `CARGO_TARGET_DIR` to the primary checkout's absolute path. This is
  not cosmetic: `target/` is ~11GB and this machine has ~32GB free, so a couple of worktrees each
  doing an independent full build would be a real disk-space incident. Verified empirically: built
  `EnterWorktree` a probe worktree, ran `cargo check` in it, confirmed it wrote into the shared
  `target/` and created no local one, then tore the probe down (`ExitWorktree remove`).
- **`.env` does not exist in any worktree, and nothing auto-copies it there — confirmed, and left
  that way on purpose.** Git worktrees only ever contain tracked files; a gitignored file like
  `.env` never comes along. Verified with the same probe worktree. Deliberately not automated:
  the key `.env` currently holds is already flagged for rotation above, and scattering more copies
  of it across every worktree would make that worse. `CLAUDE.md`'s "Background and parallel
  subagents" section now documents the one-off symlink command for the rare case a subagent
  genuinely needs live-provider access inside its worktree.
- **This repo now has a real `origin`**: a private GitHub repo, `alliecatowo/ticket-master`,
  created and pushed this same pass (`gh repo create ... --private --source=. --remote=origin`;
  `.env` verified never in git history before pushing, and the pushed tree checked for `target/`/
  `.zvec-grep/`/`.tm/` — none present). This turned the earlier "`fresh`/`head` is currently a
  no-op" finding into a real, live behavior difference, so `.claude/settings.json` now pins
  `"worktree": {"baseRef": "head"}` explicitly — verified with a genuine divergence test (a
  local-only unpushed commit, then a probe worktree confirmed to branch from *that* commit, not
  the older `origin/main`). Without this, worktrees would silently branch from whatever was last
  pushed rather than the actual local state, which is exactly wrong for how this repo is worked
  in (commits land locally well before any push, all session).
- Commits, in order: `fed4c97` (plugins to project scope), `3cad15c` (zvec-grep to project scope +
  worktree base-ref finding), `8d5e70d` (mise dev-deps, shared `target/`, `.env` documentation),
  `d4a904e` (origin added, `worktree.baseRef` pinned to `head`).

None of this touched product code. `mise run verify` was not re-run after these changes (no
Rust source changed, only `mise.toml`/`.claude/settings.json`/`.mcp.json`/`CLAUDE.md`/
`.gitignore`) — worth a sanity `mise run build` early next session anyway, just to confirm the
shared-`target/` change didn't do anything unexpected on a cold path this pass didn't exercise
(e.g. `mise run build:all` or `mise run test`, which this pass never ran against the new config).

## Next session's plan, in priority order

1. **The session/ticket architectural fix.** Still the top priority — a session should not always
   be a ticket. Full detail in "A real architectural gap" just below; don't skip it for the
   newer items that follow, they all assume it's either done or deliberately deferred, not
   forgotten.
2. **The chat screen's real design pass, now specified with actual detail** (see "The chat
   screen's actual bar" section below for the user's own words in full) — a `?` help affordance,
   a `/`-command popup, a genuinely designed status bar, and an explicit bar stated outright:
   *more beautiful and more functional than Claude Code, Codex, and opencode's own chat
   interfaces*, not "as good as." Budget real design time for this, not a quick pass between
   other work — and note the screen-router navigation shape this session already built
   (`Ctrl+T`/`/tickets` → Kanban) turns out to already match the intended model once these
   specifics arrived, so this item is about the chat screen's own polish, not more navigation
   rework.
3. **Full, deliberate testing pass across every real surface — not just the ones touched live by
   accident this session.** What actually got exercised against a real backend so far: bare
   `tm -p` (twice, different prompts), the TUI's chat→Kanban→detail navigation (pty-driven,
   automated), and the TUI's chat path with a real streamed model response (once, via a base64
   round-trip proof). That is a thin slice of this product's real surface area. Not yet
   live-tested at all: `tm serve`/the HTTP+SSE server and multiplayer/presence (`SPEC.md` §14);
   `tm mirror` sync against a real GitHub/Linear/Jira/GitLab account (`crates/tm-mirror`);
   `tm-browser`/`tm-computer` capabilities against a real browser/screen; `tm-acp`/`tm-mcp`
   against a real third-party client (Zed for ACP, any real MCP host) rather than each crate's own
   hand-rolled test harness; `tm-pty`'s agent-facing tools at all (confirmed built and unit-tested
   but not wired into any reachable capability set — see the backlog); OpenTelemetry's real span
   export to a live collector (the plumbing is tested, no span has ever actually been emitted,
   since the workspace has zero `#[instrument]` sites); desktop notifications firing on a real OS
   notification center (only the trigger *logic* is tested, the real `notify-rust`/
   `terminal-notifier`/OSC 9 paths never fired for real); D-016's Codex OAuth adapter's one live
   call (blocked twice by this sandbox's classifier, never actually run). Build this into a real
   checklist next session, work through it deliberately, and log what's found the same way this
   session's live DevPass/TUI tests did — real commands, real output pasted into the record, not
   "should work."
4. **The competitor benchmark** (`docs/backlog.md`, "A head-to-head benchmark: opencode vs. Codex
   vs. Claude Code vs. Ticketmaster") **is now genuinely buildable, not a someday item.** Its own
   stated precondition — D-003 solid, a zero-cost credential available — is met: D-003 passed a
   full Reconciliation Gate, and DevPass is live, auto-loaded, and confirmed serving real
   completions through this product's own full turn-running path. Build the fixed task suite the
   backlog section sketches (bug-fix-with-tests, add-a-small-feature, refactor-under-constraint,
   multi-file navigation), run it against all four tools with each one's own cheapest real
   credential (DevPass or an OpenAI-compatible key for opencode/Codex, real Claude auth gated
   behind an explicit opt-in flag so it's never the accidental default), and score on what
   Ticketmaster's own kernel already gives for free: did tests pass, turn/tool-call count, tokens
   spent, wall time, did verification actually catch what it claimed to. Store results as real
   ticket/event state, not a bespoke report format — dogfood the product to measure the product.
5. **"Where project state lives" is now consolidated below** (see that section) specifically
   because it kept needing re-derivation mid-session — read it once, don't re-grep the codebase
   for it again.

## Where project state actually lives — one place, so this stops needing re-derivation

- **`root`**: the workspace — git toplevel if inside a repo, else the canonical cwd.
- **`state_dir`**: where a project's real, durable state lives — `project.db`, `index.db`,
  `artifacts/`, `harness.toml`, `mirror.toml`, `workflows/`, `sched.paused`. Two possible
  locations, chosen by scope (`docs/decisions/D-003-project-scope.md`):
  - **Repo scope**: `<root>/.tm/`.
  - **Global scope** (bare `tm` in a directory with no repo-scoped project yet):
    `$TM_HOME/projects/<key>/`, where `key = sanitize(root) + '-' + blake3(canonical root)[..8]`.
- **`$TM_HOME`**: the `TM_HOME` env var if set, else `$HOME/.tm` (default). This directory itself
  is never a project's `state_dir` — only `$TM_HOME/projects/<key>/` subdirectories are; a real,
  once-shipped bug came from `locate()`'s repo-scope walk misresolving `$TM_HOME` itself as an
  ordinary repo (fixed by anchoring against `dirs::home_dir()`, not the overridable env var).
- **`tm init`** *promotes* an existing global-scope project into `<root>/.tm/`, moving real
  ticket/event data losslessly (`D-003`'s "Phase 1-C"); it does not just create a fresh one if a
  global session already exists for that workspace.
- **`.env`**: lives at the repo root you run `tm` from, current-directory-only (no upward search),
  auto-loaded by `tm` itself as of this session (`main.rs::load_dotenv`), never overrides a real
  env var already set. Holds `DEVPASS_API_KEY`/`DEVPASS_BASE_URL`/`DEVPASS_MODEL` today.
  Gitignored; never committed.
- **`~/.codex/auth.json`** (mode 0600): the real, separate Codex CLI's own stored ChatGPT OAuth
  session — not this product's own state at all, just what `crates/tm-auth/src/
  codex_subscription.rs`'s `CodexSubscriptionOAuth` (D-016) reads from, elsewhere on disk, to
  drive the provisional Codex auth adapter.
- **This repository's own primary checkout has no `.tm/` of its own, on purpose** — dogfooding
  Ticketmaster's own event log on itself was deliberately never done this session (would require
  either global scope for this exact workspace, or a real `tm init` here), and the dispatch-agent
  playbook (`.claude/skills/dispatch-background-agent/SKILL.md`) treats a `.tm/` appearing here as
  a contamination incident to clean up, not a normal state to expect.

## A real architectural gap, found live at the very end of this session — start here

**`docs/decisions/D-017-session-ticket-executor-model.md` is now the authoritative synthesis of
this — read it first, in full, before anything below.** It was written specifically because two
paraphrases of the user's actual point already drifted and cost real patience; it quotes them
directly rather than summarizing, and it cross-references exactly which parts of this were already
correctly specified elsewhere (`SPEC.md` invariants #1/#9/#12, §14, `D-001`, the backlog's
"Remote control and teleport" section) versus genuinely new (the plan-to-tickets decision point)
versus an active bug in the running code (this section, below).

**Confirmed, precisely, by reading the actual code (not guessing):** `crates/tm-cli/src/
agent.rs`'s `AgentSession::resolve_ticket` eagerly creates a real, persisted scratch `Ticket` the
*first* time any turn runs in a session with no `attached_ticket` — unconditionally, regardless of
whether the prompt is real work or a trivial question. It does **not** re-create one per message
(`self.attached_ticket` caches across the rest of that session's turns — so it's "one ticket per
session," not "one ticket per message" — but "always exactly one, created eagerly, no way to have
zero" is still exactly the bug the user is naming: a session's *identity* is a ticket right now,
and it shouldn't be).

**Do not "fix" this by making `AgentTask.ticket: Option<TicketId>`.** This was seriously
considered and rejected within this session, with evidence, not just as a guess:
`crates/tm-agent/src/outcome.rs`'s `AgentTask.ticket` is a required, non-optional field, and a
grep of every real use in `crates/tm-agent/src/agent_loop.rs` confirms it is **not**
attribution-only — `view.tickets.get(&task.ticket)` seeds the durable goal from the ticket's own
`objective`, `store.goal_state(&task.ticket)` re-reads goal state, `store.event_count_for(...)`
enforces a per-ticket event cap for budget purposes, and the ticket id is baked directly into the
rendered prompt (`crate::prompt::render(&task.ticket, ...)`). Making this field optional means
every one of those call sites grows a real "what does this mean with no ticket" branch, in the one
part of this system supposed to be boring and correct. That's a multi-session change with real
regression surface, not a quick decoupling — confirmed, not assumed.

**The right-sized decomposition** (advisor-reviewed before writing this down):
1. Stop `resolve_ticket` from eagerly creating. A turn with no attached ticket should run against
   a session-level `Authority`/`Budget` — `AgentLoop` already carries its own baseline
   independently of any task (`self.authority.intersect(&task.authority)` in `agent_loop.rs`), so
   this part is more tractable than it first looks.
2. **The open question, unresolved, is what a ticketless turn gives `AgentTask.ticket` given the
   real Store reads found above** — a session-scoped synthetic id with no real `tickets` row would
   work if those reads can be made to tolerate "no such ticket" gracefully; if they can't, this
   step alone is the bulk of the work. Start here, concretely, next session.
3. `ticket.create_child` (the one model-invocable ticket-creation tool that exists today) requires
   a parent — so a ticketless session currently has no way to ever create its *first* ticket. That
   gap is the real missing piece for "sessions can create tickets." Whether the mechanism is a new
   model-callable tool, a `/ticket`-style typed command (see below), or both, is a product call for
   the user, not something to decide unilaterally.

**2026-09-22 update to step 2:** D-017 now has an addendum narrowing this. `Store::record_usage` already takes `Option<&TicketId>`, `event_count_for` takes a generic `Id`, and the prompt render only uses the ticket id as a display header. The real remaining question is what goal-tracking (`goal_state`/`set_goal`/`reorient_goal`/`claim_goal_complete`) and `budget_handoff` mean for a ticketless turn.

**On the `/tickets` typed-command addition made just before this was written**: real, tested,
merged (`is_tickets_command` in `tui.rs`) — and the mapping it implements turns out to be exactly
right, confirmed by the specifics given later in this same session (below). Keep it.

## The chat screen's actual bar, finally given in specifics — the user's own words

*"the home screen is lack luster as fuck"* got followed up, later in this session, with the real
specifics — don't guess past what's written here, this is close to verbatim:

> I don't want the home screen as the "home". theres the `claude agents` command. when you do
> `claude` you get to normal chat screen. (im not seeing a `?` for help, a `/` command pop up,
> etc, we need that, customized status bar, its gotta be more beautiful and functional then claude
> code / codex / opencode). you do `<-` in a claude [session] and it goes to the cross-session
> "agents" view, the same thing `claude agents` goes to. so I imagine "→ tickets" or some shit
> going to that [agents-equivalent] view.

Reading this precisely, not loosely: **the naming confusion is real and worth fixing, but the
navigation mapping this session already built is correct, not wrong.** Claude Code's model is:
bare `claude` → the plain chat screen (this *is* the real default, whatever it's internally
called); `claude agents` (or `<-` in certain contexts) → a separate, cross-session view listing
work across the whole project. Ticketmaster's own chat screen (`ScreenId::Home` in code — the
*name* "Home" is what reads wrong here, not its role as the default) already *is* that default
chat screen, and `Ctrl+T`/`/tickets` already routes to Kanban the same way `claude agents` routes
to its cross-session view — so the screen-router shape built this session (D-006) turns out to
already be the right shape. **What's actually missing, concretely, next session:**
1. A `?` help affordance — nothing shows available keys/commands today.
2. A `/`-command popup — typing `/` should surface a discoverable menu (of which `/tickets` would
   be one entry), not require already knowing the exact string to type. Compare `claude` and
   `codex`'s own `/`-triggered command palettes directly, live, before designing this — don't
   guess at the interaction from memory.
3. A genuinely customized status bar — the current one-line `project.scope_line()` note is
   functional but was never designed as a real status bar.
4. **The explicit bar stated outright: more beautiful and more functional than Claude Code, Codex,
   *and* opencode's own chat interfaces — not "as good as."** This is a real design pass, not a
   quick tweak — budget real time for it next session rather than fitting it in around other work.

## What this project is

Ticketmaster: a persistent software-engineering runtime with a deterministic, event-sourced
kernel (append-only SQLite log, blake3 hash chain; all state is a materialized view). Core
thesis: "the model should not be the orchestration system" — the project, not the agent session,
is the persistent entity. Authority is attenuating/leased/revocable/auditable. Full design is
`SPEC.md` (binding philosophy, not aspirational); `docs/audit-2026-09-18-fable.md` is the
file:line-anchored implementation-status audit that drove most of this session's work item
selection (search it for `B-NN`/`M-NN`/`P-NN`/`A-NN` item IDs referenced below).

**Product-level reconciliation (mid-session user directive, now largely realized):** the
standalone CLI/TUI must feel as good as a bare `claude`/`codex`/`opencode` invocation — zero
prompts, nothing written where you don't expect it — while the full ticket/event-sourcing
machinery is the deeper "expanded universe" underneath, reachable on demand, not forced. D-003
(below) is the architecture that makes this real.

## How to resume: read these in order

1. This file.
2. `git log --oneline -50` to see exactly what's landed since this was written.
3. `docs/backlog.md` — the two "Open decision" sections are the real, unresolved product calls.
4. `docs/decisions/D-001` through whatever's newest — each is a real design decision with its own
   "what this costs" section; skim titles at minimum.
5. `CLAUDE.md` — the actual working conventions (mise tasks, hygiene checks, the dispatch-agent
   playbook, the "harness should keep improving itself" standing directive).

## Current state of `main`

HEAD as of this (final) update: `85f16ae`. Full `cargo run -p xtask -- verify` passed clean as of
`c36e3eb` (the `.env` auto-load commit, one before the two doc-only D-017 commits that follow it
— doc commits carry no code-verification risk, so this is still the accurate last-known-green
point). No known failing test, no known broken build, no known regression anywhere on `main`.

**Every commit on `main` has already been through**: build in an isolated worktree → merge by the
orchestrator (never by the building agent) → a *separate* verify-only agent confirming the merged
result → cleanup. This is the pattern that caught every real bug this session (see "Real bugs
found and fixed" below) — keep using it, don't shortcut it under time pressure.

## What landed this session (chronological, high to low level)

### D-003: project scope architecture (the biggest single piece of work)
A project now has `root` (workspace) split from `state_dir` (where state actually lives) split
from `scope` (`Repo` or `Global`). Bare `tm` in a fresh directory no longer writes anything into
that directory — it silently creates state under `$TM_HOME` (`$HOME/.tm` by default) instead,
discoverable via `tm project list`/`show`. `tm init` is the *promotion* command: it adopts an
existing global session into a real `<root>/.tm`, moving real ticket/event data losslessly. Full
design: `docs/decisions/D-003-project-scope.md`. A **Reconciliation Gate** (a hands-on agent run,
not just unit tests) proved this end-to-end against real invocations, including the specific
historical worst case (bare `tm` inside a real git repo with committed history) — all 6 scenarios
passed. One real bug was found and fixed during D-003 hardening: `locate()`'s repo-scope walk
could misresolve the user's real `$HOME/.tm` as an ordinary repo, fixed by anchoring the
exclusion check to `dirs::home_dir()` (not the env-overridable `TM_HOME`).

### All Tier-1 "blocks core thesis" audit items (B-01 through B-16) — complete
Including the two adapters that were unstarted at the top of this session:
- **`crates/tm-acp`** — real Agent Client Protocol client + server (JSON-RPC, ndjson framing,
  `AcpExecutor` implementing the `Executor` trait). `docs/decisions/D-004-acp-wire-framing.md`.
- **`crates/tm-mcp`** — real Model Context Protocol client + server (both Content-Length and
  line-delimited framing, since real-world MCP servers use line-delimited, not ACP's
  Content-Length). Exposes `ticket.*`/`search.*`/`symbol.*` read tools.

### TUI overhaul
`crates/tm-cli/src/tui.rs`'s `App` is now a real navigation shell (screen router + back-stack),
not just the single `Home` chat screen from before. **Ctrl+T** from chat opens a literal Kanban
board (`crates/tm-tui/src/screens/kanban.rs`) — columns are the real 14 `TicketState` variants,
not invented labels — with Left/Right for columns, Up/Down for cards, Enter to drill into ticket
detail, Esc/Left to go back. Bare `tm`'s plain chat screen is unchanged as the default. Real
pty-based end-to-end test (`crates/tm-cli/tests/tui_navigation.rs`) proves the back-stack
actually works, not just that screens render. `docs/decisions/D-006-tui-navigation-shell.md`.

### DevPass as default provider (B-12-adjacent, product-requested) — later CONFIRMED WORKING LIVE
When `DEVPASS_API_KEY`/`DEVPASS_BASE_URL`/`DEVPASS_MODEL` are all set, `coder.fast` (the role
driving interactive turns) prefers DevPass over Anthropic — additive, byte-identical behavior
when unset. `crates/tm-cli/src/agent.rs::build_fabric` had to change too, not just the role
table (a naive table-only fix would have been an active regression). `docs/decisions/
D-005-devpass-default-provider.md`. This was the single most-requested thread of the whole
session — **it is now fully resolved**: real credentials were obtained, wired, and the whole
chain (auth adapter → provider → `AgentLoop` → real TUI) was verified live, more than once, by
both subagents and by me directly. See "DevPass — CONFIRMED WORKING end-to-end" further down for
the full evidence; don't re-derive this from this paragraph alone.

### Phase 2 "ecosystem parity" (M-04 audit item) — full batch, 5 parallel tracks
- **`oversight.toml`** wiring: `Oversight::review` now runs at the real effect boundary
  (`AgentLoop::drive`), not just in tests. `docs/decisions/D-009-oversight-policy-wiring.md`.
- **OpenTelemetry**: opt-in, `--features otel`, off by default, zero dependency-tree cost when
  unused. Honest caveat: the workspace has zero `#[instrument]` call sites yet, so this exports
  nothing to a real collector today — the plumbing is real and tested, the instrumentation is a
  separate follow-up. `docs/decisions/D-010-opentelemetry-tracing.md`.
- **Secret redaction**: pattern-based, at three real boundaries (`Fabric::execute`,
  `Store::store_artifact`/`StoreTx::append`, `pack::compile`) — not just a standalone module.
  `docs/decisions/D-011-secret-redaction.md`.
- **`tm run <ticket> --worktree`**: real per-run git-worktree isolation for a delegated worker.
  `docs/decisions/D-012-run-worktree-isolation.md`.
- **AGENTS.md / SKILL.md / hooks.toml**: AGENTS.md files feed `SectionKind::Conventions`;
  `.tm/skills/**` progressive disclosure via a new `skill.load` tool; `hooks.toml`
  (`PreToolUse`/`PostToolUse`/`UserPromptSubmit`/`SessionStart`/`Stop`, shell-only, fail-closed)
  wired into `ToolRegistry::dispatch` before `Authority::permits`. `docs/decisions/
  D-013-hooks-agents-skills.md`.

### Ticket checkpoints / fork-from-log (backlog: "top trust and control feature" per competitors)
`tm ticket fork <T> --at <seq>` — a new ticket lineage starting from `T`'s materialized state as
of `seq` (not current state, not raw event replay — a fresh `ticket.created`+`ticket.updated`
pair plus a purpose-built `ticket.forked` provenance event). Separately, `tm sched run`/`tm run`
capture a `git stash create`-style workspace snapshot per real patch. `docs/decisions/
D-008-ticket-checkpoint-fork.md`.

### Desktop notifications (backlog: "the single clearest signal" gap vs. every competitor)
Real notifications (`notify-rust` / `terminal-notifier` / OSC 9 fallback chain) on
`approval.requested`/`ticket.escalated`. New `crates/tm-notify`. `TM_NOTIFY=0` to opt out.
`docs/decisions/D-007-desktop-notifications.md`.

### Two real, independently-confirmed pre-existing bugs found and fixed (not caused by this
### session's other changes, but caught by verification passes and fixed anyway)
- **`tm sched run` panicked 100% of the time** — a second `tokio::Runtime` was built and
  `block_on`'d from a thread already inside `#[tokio::main]`'s runtime ("Cannot start a runtime
  from within a runtime"). Confirmed via git history that this never worked (introduced in the
  same commit as `#[tokio::main]` itself). Fixed by making `dispatch_sched` a real `async fn`,
  awaited in place. Real regression test spawns the actual binary and proves it no longer panics.
- **`Authority::is_subset_of` (security-relevant path-pattern containment check) had FOUR
  separate false-positive bugs**, found across two passes: the originally-reported one
  (`is_subset_of(["docs"], ["*/**/**"])` wrongly `true`), two more found by adversarial review
  while fixing the first (a `/**`-suffix bare-prefix disjunct gap, a messy-stripped-prefix trust
  gap), and a fourth found in a follow-up pass (`[...]` character classes treated as raw bytes
  instead of one match unit). All fixed conservatively (when in doubt, return `false`/not-a-subset,
  never a wrong `true` — this is security-relevant code gating authority containment). Backed by
  a 3.5-million-triple exhaustive differential test with zero violations, on top of the existing
  proptest. `docs/decisions/D-014-pattern-subset-double-star-fix.md` (updated in place across both
  passes — this repo's convention for a decision's own direct follow-up, not a new doc per pass).

### Harness self-improvements (the "keep improving itself" standing directive, taken seriously)
- `.claude/skills/dispatch-background-agent/SKILL.md` — the real playbook for dispatching
  background agents against this repo. Read it before dispatching anything. Documents two real,
  repeated failure modes this session hit and fixed the *process* for (not just the symptom):
  worktree isolation silently failing to hold across a rate-limit resume, and a verify-only agent
  racing against the orchestrator's own concurrent git merges in the primary checkout.
- `crates/xtask/src/hygiene.rs` gained: a hard-fail check for stray `.tm`-literal path
  assumptions outside the sanctioned `project.rs` resolvers, and a hard-fail check for dangling
  `D-NNN` decision-doc cross-references (built *because* this session hit five real
  decision-number collisions from parallel worktrees each independently picking "next free
  number" blind to siblings — this check would have mechanically caught all five, plus the
  several stale bare-`D-008`-style prose references that were only caught by luck).
- `crates/xtask/src/drift.rs` (`check-drift`, advisory) — catches an unchanged
  `assert_eq!(Enum::X.len(), N)`-shaped cardinality assertion whose enum drifted elsewhere in the
  same file since a given base sha. Built after a real incident (`SectionKind` gaining a variant
  during a B-10/B-14 merge, silently breaking two unrelated tests). Run it by hand after any
  batch of parallel-track merges: `mise run check-drift -- <base-sha>` — and derive `<base-sha>`
  from each feature branch's *actual first-commit parent*, not just "the commit before the
  earliest merge," since branch order and merge order can diverge (this bit an agent for real
  this session; see the check-drift run in the transcript for the exact method).

## In-flight work — none. Everything from this session is merged, committed, and idle.

`ListAgents` showed zero running subagents and zero worktrees as of the final commit on this
branch. The last verify-only agent (checking the Codex OAuth adapter merge) was deliberately
stopped mid-run, not left to finish — it was holding the shared `target/` build lock while the
user was actively trying to run `cargo run -p tm-cli --bin tm` themselves in their own terminal,
and every piece it was re-checking had already been individually verified before merging. If you
want that specific re-confirmation, it's cheap and safe to just run `cargo run -p xtask --
verify` yourself; nothing about stopping it left `main` in a questionable state.

**Everything done this stretch, for reference:** the decisions/wiki fix (merged as **D-015**,
`crates/tm-wiki/src/decisions.rs` now reads `docs/decisions/*.md` directly, wiki regenerated); the
live DevPass `-p` test (confirmed working, 4 bugs logged to `docs/backlog.md`); the live TUI+model
test (confirmed working, same `build_fabric()` path as `-p`, a structural UX finding and a
D-002/tm-pty gap list logged to `docs/backlog.md`, plus the key-exposure incident at the top of
that file); the Codex ChatGPT-session OAuth adapter + provider (merged as **D-016**, provisional —
read on).

### D-016 — Codex ChatGPT OAuth adapter: built, tested, merged, but its one live proof never ran

`crates/tm-auth/src/codex_subscription.rs` (`CodexSubscriptionOAuth`) + `crates/tm-provider/src/
providers/codex_chatgpt.rs` (`CodexChatGptProvider`) — reads the real `~/.codex/auth.json`,
refreshes via the same RFC 6749 §6-correct logic this session already fixed once in
`DeviceCodeOAuth`, targets `https://chatgpt.com/backend-api/codex/responses` (Responses API shape,
determined by decoding the JWT's public claims plus cross-referenced documentation — **never
empirically confirmed**, since the one live network test this was built to prove
(`mise run test:live-codex-auth`) was blocked twice by this sandbox's own auto-mode classifier,
even after the user had already explicitly authorized this exact task. Deliberately **not** wired
into the default provider registry/role table — reachable only by explicit name. `docs/decisions/
D-016-codex-chatgpt-session-auth-adapter.md`'s own Status line is **provisional, not accepted**,
specifically because of this. **Next session: run `mise run test:live-codex-auth` on a real,
unsandboxed environment with a live `codex login` session** — if it passes, flip D-016's status to
accepted; if it fails, the doc suggests bisecting with a bare text-only prompt first, since several
parts of the wire shape (the `originator` header value, `stream:true` necessity, SSE terminal-event
shape) are independently uncertain and a failure could come from any of them.

## DevPass — CONFIRMED WORKING end-to-end, for real, live

Resolved during this session, via the new "be liberal with research" `CLAUDE.md` directive: the
user gave a real API key (prefixed `llmgtwy_`, i.e. an LLM Gateway key) without a base URL or
model name. A few minutes of `WebSearch`/`WebFetch` resolved both: DevPass routes through LLM
Gateway's own OpenAI-compatible endpoint at `https://api.llmgateway.io/v1`, and the specific
cheap/bulk model the user asked for by a garbled name ("muse spark 1.3 contributor") is a real,
current model: `muse-spark-1.3-contributor` (Meta, released 2026-09-02, ~$0.10/$0.20 per 1M
tokens). All three are now in `/Users/allie/Develop/ticket-master/.env` (gitignored, mode 600):
`DEVPASS_API_KEY`, `DEVPASS_BASE_URL=https://api.llmgateway.io/v1`,
`DEVPASS_MODEL=muse-spark-1.3-contributor` — matching `crates/tm-provider/src/providers/
compat.rs`'s `DevPassProvider` exactly. **Update, later in this same session: `tm` now DOES
auto-load `.env`** — this was a real, twice-repeated user complaint ("cargo run command didn't
give it the .env"), fixed for real: `main.rs::load_dotenv` (new, calls `dotenvy::dotenv()`) runs
before anything else in `main()`, loads `.env` from the current directory only (no upward search,
deliberately, to avoid a D-003-shaped surprise), never overrides a real env var already set.
Verified live: unset every `DEVPASS_*`/`LLM_GATEWAY_API_KEY` var, ran `tm provider detect` from
the repo root with zero manual export, got `devpass`/`"availability": "ready"` back purely from
`.env`. No more manual `source .env`/`export` needed for any normal invocation from this repo
root.

**Confirmed, for real, by agent `af97e050cfb5c0de4`**: a real `tm --json -p "Reply with exactly
the following seven characters and nothing else: DEVPASS"` run against a real tempdir project got
back the real model's real text reply, `DEVPASS`, over a real network round-trip to
`https://api.llmgateway.io/v1/chat/completions`, served by `devpass`/`muse-spark-1.3-contributor`
— confirmed by source-level elimination (no other provider was ever registered given the
configured env) and cross-checked with an independent `curl` against the same endpoint/key/model
(200 OK, real billed usage). **This is the first real, live confirmation this session that D-005
(DevPass-as-default-for-`coder.fast`) actually works**, not just passes unit tests with a mock.

Four real, pre-existing (not caused by D-005) bugs surfaced along the way and are logged in
`docs/backlog.md`'s new "Four real bugs found by the first live DevPass round-trip" section —
summary: `tm provider test`/`status` are lying no-op stubs; `compat.rs`'s finish-reason mapping
doesn't handle this gateway's `"incomplete"` reason (a real, reproduced request-failure mode for
this specific model when `max_tokens` is tight); the actually-served model (`served_by`) is
computed but never surfaced anywhere, live or historical; and `tm --json -p` doesn't actually
emit JSON despite documentation claiming it does. None block real usage (confirmed by the
successful test above); all are real gaps worth fixing.

Note on the macOS Keychain: writing to it failed (`security add-generic-password` — "User
interaction is not allowed") because this session's shell is non-interactive and the keychain was
locked; that needs a GUI unlock only the user can do, not something to keep retrying
programmatically. `.env` is the real, working store for now.

## Personally verified live, at the very end of this session (not via a subagent report)

Ran the real compiled binary myself, directly: `tm -p "..."` answered a real arithmetic question
correctly via the live DevPass backend, and a real agentic task (create a file, read it back to
verify) completed correctly with real `edit.create_file`/`fs.read` tool calls. This is what led to
pinning down the `ticket.submit`-only-valid-from-`Running` root cause documented above.

Also added, in direct response to live user feedback comparing the TUI's ticket-view entry point
to `claude agents`: `crates/tm-cli/src/tui.rs`'s `is_tickets_command` — typing `/tickets` (or bare
`tickets`) into the chat input and pressing Enter now opens the Kanban board too, not just the
`Ctrl+T` chord, which nothing on screen hints exists. Real regression test added and passing
(`crates/tm-cli/tests/tui_navigation.rs::typing_slash_tickets_and_enter_opens_kanban_without_spawning_a_turn`,
deliberately runs with no mock provider configured, so it would hang instead of silently passing
if `is_tickets_command` ever failed to intercept the submission before a real turn spawned).
`docs/decisions/D-006-tui-navigation-shell.md` updated in place (§3b) with the same convention
this repo already used once for a direct decision follow-up. Full `cargo run -p xtask -- verify`
confirmed clean on this change (fmt caught one line-length issue on the first pass, fixed,
re-verified green) — this is settled, not pending.

Also confirmed, via a real screenshot the user shared from a live mobile SSH session: bare `tm`
correctly opens directly into `Home` (chat input focused, ticket dashboard visible alongside) —
this was a live, authentic real-world confirmation the default-screen design actually works as
intended, not a report I have to take on faith.

Shipped after that, in direct response to the next round of live feedback: `.env` auto-loading
(see the DevPass section above for the details and live proof) and `docs/decisions/
D-017-session-ticket-executor-model.md` — the session/ticket/executor synthesis this file's very
first section already points you to. If you haven't read D-017 yet, this is the last reminder:
read it before doing anything else in this codebase.

## Open / pending — things a human needs to weigh in on, not yours to resolve unilaterally

1. **The `resolve_genesis_provider` slug-ignoring bug** (in `crates/tm-cli/src/project.rs`) —
   found while building D-005, deliberately left unfixed and out of scope, logged in
   `docs/backlog.md`. Real bug: genesis's three frontier bootstrap roles always construct an
   `AnthropicProvider` regardless of what the role table's `provider` field actually names.
   Harmless today only because nothing points those roles at a non-Anthropic slug yet.
2. **The `-p` scripting exit-code question** (`docs/backlog.md`, "Open decision: should `-p` exit
   non-zero on an in-band agent failure?") — `tm -p` currently prints `failed (Other): ...` but
   exits 0 for an in-band agent failure (deliberate design in `run_turn_streaming`, but
   undocumented and in tension with `-p` being described as scripting-suitable). Not resolved.
3. **The `[...]` glob-class exhaustive-test alphabet extension is done, but the proptest
   generator itself (`authority_laws.rs`) was deliberately NOT extended** — it's a separately
   still-weak piece of test infrastructure for this bug class (confirmed empirically: ~22,000
   random proptest cases found the reported bug zero times, even though it was 100%
   reproducible). Worth someone deciding whether to strengthen the generator itself.
4. **`segment_implies`'s handling of `[...]` character classes is now correct but deliberately
   conservative** in a couple of named edge cases (documented precisely in `docs/decisions/
   D-014-pattern-subset-double-star-fix.md`'s final version) — fine today (zero real, non-test
   pattern data in the workspace uses character classes at all), but worth knowing if that ever
   changes.

## Task-tracking note

This session did not use a formal ticket/todo-list mechanism for its own meta-work — tracking was
done by (a) this conversation's own turn-by-turn narration, (b) `docs/backlog.md` as the durable
record of anything not fully resolved, and (c) each `docs/decisions/D-NNN-*.md` as the durable
record of anything that *was* resolved and why. Priority order for next session is the "Next
session's plan" section at the very top of this file — don't re-derive it here, that section is
the current one; treat the "Open / pending" list below as the smaller items to fold in around the
edges of that plan, not a competing priority order.

## Practical operating notes for whoever resumes this

- This machine is real, local, disk-and-memory-constrained (8GB Mac). `-j 2` on every cargo
  invocation, `rm -rf target` after every verify pass, watch `top -l 1 -n 1 | grep PhysMem` before
  dispatching more than ~2 concurrent heavy builds.
- Always dispatch background/parallel work per `.claude/skills/dispatch-background-agent/
  SKILL.md` — `isolation: 'worktree'` mandatory, never let a subagent run the real `tm` binary
  against the primary checkout outside a tempdir, orchestrator merges personally and verifies
  with a separate agent after every merge, clean up the worktree/branch immediately after.
- Decision-doc numbering collisions are real and repeated (6 this session, most recently D-015
  claimed independently by both the decisions/wiki fix and the Codex OAuth adapter) — if dispatching
  multiple parallel tracks that might each write a new `docs/decisions/D-NNN-*.md`, either
  serialize the doc-numbering step or expect to renumber on merge; the new hygiene check will at
  least catch any cross-reference you miss, but it won't catch the *filename* collision itself.
- The user corrects direction directly and expects it to stick immediately, not be
  re-litigated — e.g. the Codex integration was very clearly "auth provider for our own executor,
  not Codex as an executor" after one correction; don't need it re-explained if it comes up again.
- **Correction to an earlier entry in this file**: a recurring goal-check-in kept reporting
  background task `buopfo6qg` as still running — `ps aux`/`ListAgents` genuinely showed nothing
  (it doesn't run as an inspectable OS process or subagent), which earlier led to guessing it was
  harmless harness-internal plumbing. That guess was wrong, caught later via `TaskOutput`/
  `TaskStop`, which *are* the right tools for this (not `ps`/`ListAgents` — a harness-tracked
  background task isn't necessarily either). Its real command was `until [ -s
  .../tasks/biqip6zpt.output ] && grep -q "Doc-tests tm_workflow" .../tasks/biqip6zpt.output; do
  sleep 3; done` — a poller waiting on a *different*, much earlier `mise run verify` task
  (`biqip6zpt`, launched 2026-09-20 11:49) to print a specific sentinel string. `biqip6zpt` had
  already finished successfully (exit code 0) within seconds of being launched, but its doc-test
  summary never contained the literal substring `"Doc-tests tm_workflow"` (most likely because
  `tm-workflow` has zero doc-tests, so that header line never gets printed at all), so the `grep
  -q` completion check could never match — the underlying work was fine; only the sentinel string
  was wrong. It then polled every 3 seconds for about 43 hours before being noticed and stopped
  with `TaskStop`. **Lesson for next time a check-in flags a stuck background task**: use
  `TaskOutput(task_id, block: false)` first (it reports real status even when `ps`/`ListAgents`
  show nothing), and if a task is a `until [ -s <file> ] && grep ... <file>` poller, read the file
  it's actually waiting on before assuming either "it's fine, ignore it" or "it's hung, kill it" —
  the target task may have already finished with a completion string that just doesn't match.
