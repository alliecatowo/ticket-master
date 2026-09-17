# Backlog

Work that is specified and agreed but not yet built, in rough priority order. Anything here is
real, scoped work — not aspiration. Items that turn out to be wrong get deleted, not quietly kept.

## Executors (D-001)

- `Executor` trait, `ExecutorTask` / `ExecutorOutcome` / `ExecutorCapabilities` in `tm-core`, with
  `tm-agent` reimplemented as the `builtin` adapter behind it.
- Adapters: `claude-code` (print mode / SDK), `codex` (exec / JSON), `pi` (JSON RPC over stdio),
  `opencode`, `human`.
- Sandbox derivation: turn a granted `Authority` into a concrete filesystem/network/command sandbox
  for an external process, and validate returned writes against that scope.
- Executor fabric: placement routing (local process, container, remote node, cloud sandbox) reusing
  the provider fabric's quota, health and fallback machinery. Free tiers modelled as ordinary
  capacity with a daily ceiling.

## Workflow definitions (SPEC §25)

- The definition format, its parser, and expansion into a validated ticket subgraph committed in one
  transaction.
- `for_each` fan-out over a prior node's structured output; explicit joins with timeout and merge
  rules; cycle budgets required at compile time.
- Versioned definitions with running instances pinned to the version they started on.
- A starter library: `review-change`, `migrate-sites`, `research-and-synthesize`, `harness-benchmark`.
- `tm doctor` warning for a workflow that is one node wide and one node deep.

## Project wiki (SPEC §26)

- Wiki assembly from authoritative sources, each page carrying provenance and rendered freshness.
- Staleness banner naming exactly what invalidated a page (ticket and/or decision).
- `/wiki` in `tm serve`, cross-linked with tickets, decisions and presence; plain markdown in-repo
  so it still works on GitHub with nothing running.
- Wiki pages as a ranked retrieval source in context compilation, so workers read the compiled
  explanation before the source.

## Project templates (SPEC §27)

- Template format: manifest, files with substitution, pinned `deps.lock`, `skill.md`, `verify.toml`,
  `bench/`.
- Selection during Genesis graph compilation instead of generating a scaffold from nothing.
- Starter set: VitePress, Zola, Astro, Next.js, Textual, Ink, Ratatui, Cobra, Axum, FastAPI, Hono,
  Tauri, SwiftUI, and the three library templates.
- Registry with version and checksum pinning; third-party templates treated as untrusted input
  (sandboxed verify, `skill.md` as data, no self-granted authority).
- CI that scaffolds every template and runs its `verify.toml`, so a template that does not build is
  caught as a bug in the template.

## Verification ladder (SPEC §18.5)

- `T-E2E-WEB` — Playwright end-to-end against a live `tm serve`.
- `T-VIS-REG` — visual regression across light/dark and Reduce Transparency, including the macOS
  Liquid Glass surfaces and recorded PTY screens.

## From competitive research (2026-09-16)

Three agents surveyed orchestration products, durable-execution engines, and retrieval/computer-use,
and were asked specifically what Ticketmaster's spec *misses*. Ranked by how much damage the gap does
if left alone, not by how interesting it is.

### Correctness and safety — these are the ones that bite

- **The effect boundary is a policy, not an enforcement.** §21.5 describes idempotency keys and
  receipts as a discipline. Temporal's sharpest idea is that the workflow/activity split is
  *structural*: deterministic code physically cannot perform a side effect. We should make the
  effect API require a capability token checked against the live lease, so bypassing the receipt
  machinery is impossible rather than merely discouraged.
- **"Effect ran, receipt lost" is unhandled.** If a push succeeds and the process dies before the
  receipt commits, an idempotency key alone cannot distinguish that from "never ran". Each effect
  type must commit to a pattern — journal-first-then-execute, or execute-then-confirm-by-query
  (ask git whether the push landed) — and say which. This is the exact bug class from-scratch
  engines get wrong.
- **No dumb global backstop.** We bound a cycle (`CycleBudget`) and the review queue (§21.4), but
  there is no unconditional project-wide ceiling — total concurrent leases, events per unit time —
  of the kind Step Functions' 25,000-event cutoff provides. Its whole value is catching the case
  where every local invariant is satisfied and the system is still misbehaving in aggregate.
- **The resumption contract for escalation is unspecified.** When a human resolves an `Escalated`
  ticket days later, what exactly re-executes on `Escalated -> Blocked -> Ready`? Which context,
  partial output and accumulated evidence are still valid? LangGraph users hit this as the
  "everything before interrupt() re-runs" surprise. We must state it.
- **Authority has no organization scope.** The lattice is project-rooted, so nothing lets an org cap
  what any project's ROOT may ever contain. That matters the moment a second team uses this.
- **Credentials are allowlists, not short-lived grants.** A lease is already a scoped, expiring
  grant; platform-native ephemeral tokens (GitHub's `GITHUB_TOKEN` model over long-lived PATs)
  compose with it almost exactly. Minting a per-lease credential that dies with the lease is a
  better story than a permanent token plus a network allowlist.
- **Single-writer is an unstated boundary.** One SQLite write connection and one scheduler is a
  defensible choice for a local-first tool, but Restate and DBOS treat multi-process contention as
  core. Either state scale-out as consciously out of scope, or specify what happens with two
  schedulers on one log.

### Capability gaps worth taking

- **Time travel and forking.** We can replay to rebuild, but not fork a ticket's history at an
  arbitrary seq to explore an alternative resolution, and there is no verb for stepping through a
  ticket's event history interactively. LangGraph ships both; our event log already makes it nearly
  free.
- **Distilled memory, not just documentation.** `tm-docs` tracks staleness and §26's wiki is the
  compiled-knowledge surface, but neither is the "lessons, conventions, runbooks" layer that
  Factory and Devin carry forward. Cheaper to consult than replaying the log; distinct from docs.
- **Interactive self-verification as a first-class Evidence kind.** "The agent drove the app and
  watched it work" should be its own evidence type with its own executor role, not an afterthought
  of browser automation. Competitors catch with this what static tests miss.
- **A tight autofix loop, distinct from full recovery.** write -> verify -> autofix -> reverify,
  bounded, before a human ever looks. Today that shape is only expressible as the generic recovery
  machinery, which is heavier than it needs to be.
- **Model tier as an explicit cost lever.** Roles imply it; competitors advertise it (Amp's
  Rush/Smart/Deep). Difficulty-to-tier should be a stated policy, not an emergent property of how
  someone wrote `providers.toml`.
- **A living spec, not a one-shot compile.** Genesis compiles spec to graph once. Kiro treats the
  spec as authoritative and cascades edits into regenerated tasks, tests and docs. Continuous
  reconciliation of spec against ticket state is a sharper version of what we already claim.

## Terminal surface quality bar (SPEC §15)

The CLI is the proof surface, so it is also the one that has to be visibly better than the field —
Claude Code, Codex, opencode and pi are the comparison set, and "as good as" is a failure. Two
demands sit in tension and both are hard requirements: it must be beautiful, and it must be sturdy.

- **Sturdy first, because pretty is worthless if it corrupts the screen.** A single render model:
  the TUI is a pure function of project state to a frame, diffed and flushed, never interleaved
  `println!` from background tasks. Correct wcwidth/grapheme handling so CJK, emoji and combining
  marks do not tear the layout. Resize, suspend/resume (`SIGTSTP`), and `SIGWINCH` handled without
  a redraw storm. Degrades by capability detection, not by guessing: truecolor to 256 to 16 to
  monochrome, Unicode to ASCII box drawing, and a `--plain` mode that is the same information with
  no cursor addressing at all. A panic restores the terminal — no orphaned alternate screen, no
  disabled echo. Every frame is reproducible from a recorded event log, which is what makes the TUI
  testable at all (see the PTY layer, SPEC §22).
- **Then beautiful.** The thing worth showing is what nobody else shows: the ticket graph, the
  authority a worker actually holds, live lease countdowns, budget burn, and the verification
  ladder's state — a project you can watch, not a scrolling transcript. Streaming output that never
  jumps, a status region that stays put, diffs rendered as diffs, and evidence rendered as evidence.
- **What to take from the field.** Codex's and pi's input handling and approval affordances are
  worth cargo-culting outright (D-001 already says so). What none of them have is persistent
  project state to render, which is exactly our advantage — do not copy a transcript UI onto a
  state machine.
- **Accessibility is not a later pass.** Respect `NO_COLOR`, `TERM=dumb` and reduced motion; never
  encode meaning in colour alone; keep a screen-reader-sane linear mode.

## First-party integrations: GitHub and Linear apps

Distinct from the §13 mirroring adapters, which sync tickets. These are installable applications
that put Ticketmaster *inside* the tracker.

- **GitHub app.** Checks and status on the verification ladder, PR review comments carrying evidence
  and provenance, issue-to-ticket adoption, and installation tokens as the per-lease ephemeral
  credential the research gaps above already argue for.
- **Linear app, built as a Linear Agent — not a bot.** Linear's agent model maps onto ours closely
  enough that fighting it would be the mistake. Install via OAuth2 with `actor=app` and the
  `app:mentionable` / `app:assignable` scopes; the agent is *delegated* an issue rather than
  assigned it, which is the same authority-grant shape we already model. An `AgentSession` is
  created when the agent is mentioned or delegated an issue, arriving as an `AgentSessionEvent`
  webhook with action `created`; follow-up user messages arrive as action `prompted`. Sessions carry
  six states — `pending`, `active`, `error`, `awaitingInput`, `complete`, `stale` — which Linear
  derives from the activities we emit, so we never set state directly. Progress is reported by
  `agentActivityCreate` with a content type of `thought`, `action` (with `action`, `parameter` and
  an optional `result`), `elicitation`, `response`, or `error`; only `thought` and `action` may be
  ephemeral.
  - The hard constraint: **an activity must be emitted within 10 seconds of the `created` event** or
    the session is marked unresponsive, and webhooks themselves must be acknowledged in 5. So the
    webhook handler appends an event and returns; a `thought` is emitted immediately; the real work
    runs on the scheduler as an ordinary ticket. This is the first external system that imposes a
    latency SLA on our loop, and it is worth taking as a design forcing function.
  - The mapping worth getting right: `elicitation` is our escalation-to-human, `action` is our
    effect boundary, `response` is the verified outcome, `error` is a failed verification — not
    freeform chat. Activities are frozen snapshots, so conversation history is reconstructed from
    them, which is the same discipline as our event log.

## Documentation retrieval (Context7 or equivalent)

Workers currently get repository context and nothing about the libraries they are using, which is
the single largest source of confidently wrong code. Add an external-documentation source to context
compilation (SPEC §8), ranked alongside code and wiki pages.

- Provider-shaped, like everything else: Context7 (`resolve-library-id` then fetch, or its HTTP
  `/libs/search` and `/context` endpoints), a self-hosted index such as `docs-mcp-server`, and a
  plain `llms.txt` fetcher for projects that publish one.
- Pinned to the dependency versions actually in the lockfile — docs for the wrong major version are
  worse than no docs.
- Cached in-repo with provenance and a fetched-at stamp so a context pack stays reproducible, and so
  the compiler can be offline.
- Feeds the wiki (§26) as a citable source rather than being a separate silo.

## Remote control and teleport (SPEC §14, §18)

A project outlives any one machine, which is precisely the claim Ticketmaster makes — so a session
must be attachable from somewhere else. Survey of how the field does it, and what we should take:

- **opencode** splits `serve` (headless HTTP backend, OpenAPI 3.1 at `/doc`) from the TUI, then
  `opencode attach <url>` points a terminal at a running backend on another machine, and
  `opencode web` serves a browser client from the same process. The separation is right and is
  already our shape: `tm serve` is the backend, every surface is a client.
- **Claude Code** runs two directions deliberately: `/remote-control` pushes a locally running
  session out to phone/web, while `--teleport` pulls a cloud session down into the local terminal.
  `claude ssh` drives a remote machine over an ordinary SSH connection. The push/pull distinction is
  the useful idea — they are different trust and ownership stories, not one feature.
- **Codex** takes the cloud-first route: work runs in a hosted sandbox and the local client is one
  view onto it.

What we build, given that our state is already an event log — which makes this cheaper for us than
for any transcript-based agent:

- Attach and detach as ordinary operations: `tm attach <url>` resumes from a known `seq` using the
  subscribe-then-backfill protocol already specified in `tm-events`, so a reconnect never gaps and
  never replays from zero.
- Both directions, named as such. Push (expose this local project to another device) and pull
  (adopt a remote project into this terminal) are separate verbs with separate authority grants — an
  attached client gets its own attenuated `Authority`, and revoking it is a lease expiry, not a
  special case.
- Multiple concurrent viewers with presence, since the server already has it.
- Transport as a provider: local socket, SSH, and a relay for the case where neither end can accept
  a connection. End-to-end encryption on the relay path, because source is passing through it.
- The single-writer boundary from the research gaps above has to be resolved before this ships — two
  attached clients issuing commands is exactly the contention case that is currently unstated.

## Stretch

- iOS simulator executor (SPEC §23).
- Additional model providers beyond Anthropic in the provider fabric.
- Additional tracker adapters beyond the shipped four.
