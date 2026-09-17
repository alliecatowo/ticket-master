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

## Stretch

- iOS simulator executor (SPEC §23).
- Additional model providers beyond Anthropic in the provider fabric.
- Additional tracker adapters beyond the shipped four.
