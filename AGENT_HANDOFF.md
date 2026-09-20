# Agent Handoff — 2026-09-20

Written because this session is about to run out of usage for ~2 days. Read this first in the
next session before doing anything else. `main` is in a good, fully-verified state as of the
latest commit on this branch — you are not picking up a mess, you're continuing a working build.

**SECURITY, read this line first:** the DevPass/LLM Gateway API key in `.env` was accidentally
printed in full into an agent's tool output this session (caught by the agent itself, not
repeated afterward, but exposed regardless). If the user hasn't already rotated it, that's the
very first thing to raise next session — see `docs/backlog.md`'s top section.

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

HEAD as of writing: `00e6b10`. Full `cargo run -p xtask -- verify` (fmt + clippy -D warnings +
`cargo test --workspace` + hygiene) passes clean — last full run: **3001 passed, 0 failed, 4
ignored** (pre-existing, documented ignores). `check-drift` (the advisory parallel-track-drift
checker) was also run by hand across this session's entire merge chain and found nothing. There
is no known failing test, no known broken build, no known regression anywhere on `main` as of
this commit.

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

### DevPass as default provider (B-12-adjacent, product-requested)
When `DEVPASS_API_KEY`/`DEVPASS_BASE_URL`/`DEVPASS_MODEL` are all set, `coder.fast` (the role
driving interactive turns) prefers DevPass over Anthropic — additive, byte-identical behavior
when unset. `crates/tm-cli/src/agent.rs::build_fabric` had to change too, not just the role
table (a naive table-only fix would have been an active regression). `docs/decisions/
D-005-devpass-default-provider.md`. **Still never actually tested live** — see "Open/pending"
below, this is the single most-requested-and-least-resolved thread this session.

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

## In-flight work — check on these FIRST in the next session

Use `ListAgents` to check status; if any shows `completed`, its report should already be in the
conversation (look for a `SubagentHandback`/agent-message) — merge it following the established
pattern (verify in a separate agent, then clean up the worktree). If any is still `running` or got
interrupted by the usage cutoff, resume it via `SendMessage({to: <agentId or name>, message:
"..."})` referencing exactly what it last reported doing — this has worked reliably every time
it's been needed this session (including across real rate-limit interruptions).

**Already done since this doc was first written** (mentioned only so you don't redo them): the
decisions/wiki fix (merged as D-015, wiki regenerated); the live DevPass `-p` test (confirmed
working, 4 bugs logged to `docs/backlog.md`); the live TUI+model test (confirmed working, same
`build_fabric()` path as `-p`, a structural UX finding and a D-002/tm-pty gap list logged to
`docs/backlog.md` — including the key-exposure incident at the top of that file).

**Still in flight, check this one first:**

1. **`a9c97b78b66b302b0`** (worktree `.claude/worktrees/agent-a9c97b78b66b302b0`) — building a
   real `AuthAdapter` (`crates/tm-auth`) that reads the local, already-logged-in Codex CLI's
   stored ChatGPT OAuth session (`~/.codex/auth.json`) and a `Provider` that uses it to drive a
   real completion through Ticketmaster's own `AgentLoop` — **not** Codex as an external
   executor, explicitly corrected mid-session after an initial misdirected build attempt. This
   was explicitly authorized by the user after an auto-mode classifier initially blocked it
   ("Credential Exploration") — that authorization stands for this specific task as already
   scoped; don't re-ask, but also don't broaden scope beyond what was authorized. Security
   hygiene was explicitly required: no raw token substrings in logs/tests/commits, ever (note:
   a *different* agent leaked the *DevPass* key this session, not this one — see the security
   note at the top of this file; re-check this agent's own diff for the same mistake before
   trusting its own "no leak" claim, precisely because it just happened once already today).

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
compat.rs`'s `DevPassProvider` exactly. **`tm`/`tm-cli` does NOT auto-load `.env`** (checked, no
dotenv dependency anywhere) — these vars need to be explicitly exported/sourced before running
`tm` for real, or wired into a real dotenv-loading mechanism if that's wanted as a permanent
convenience (not built yet, wasn't asked for).

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
record of anything that *was* resolved and why. If you want a more formal continuation mechanism
next session, priority order for what's left is roughly: check the three in-flight agents above
first (especially the live DevPass test result) > `docs/backlog.md`'s remaining "Open decision"
section (`resolve_genesis_provider`) > the smaller open items listed above.

## Practical operating notes for whoever resumes this

- This machine is real, local, disk-and-memory-constrained (8GB Mac). `-j 2` on every cargo
  invocation, `rm -rf target` after every verify pass, watch `top -l 1 -n 1 | grep PhysMem` before
  dispatching more than ~2 concurrent heavy builds.
- Always dispatch background/parallel work per `.claude/skills/dispatch-background-agent/
  SKILL.md` — `isolation: 'worktree'` mandatory, never let a subagent run the real `tm` binary
  against the primary checkout outside a tempdir, orchestrator merges personally and verifies
  with a separate agent after every merge, clean up the worktree/branch immediately after.
- Decision-doc numbering collisions are real and repeated (5 this session) — if dispatching
  multiple parallel tracks that might each write a new `docs/decisions/D-NNN-*.md`, either
  serialize the doc-numbering step or expect to renumber on merge; the new hygiene check will at
  least catch any cross-reference you miss, but it won't catch the *filename* collision itself.
- The user corrects direction directly and expects it to stick immediately, not be
  re-litigated — e.g. the Codex integration was very clearly "auth provider for our own executor,
  not Codex as an executor" after one correction; don't need it re-explained if it comes up again.
