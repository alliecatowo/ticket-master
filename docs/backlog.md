# Backlog

## SECURITY: rotate the DevPass/LLM Gateway API key

A diagnostic script (run by an agent verifying the real TUI against a live model) accidentally
printed `LLM_GATEWAY_API_KEY`/`DEVPASS_API_KEY` in full into its own tool output while reading
`.env` — the agent caught its own mistake and didn't repeat the value afterward, but the key was
exposed in this session's transcript. **Treat it as compromised. Rotate/revoke it, then update
`.env` with the new value** (`DEVPASS_API_KEY`/`LLM_GATEWAY_API_KEY`, same value both places per
this session's own notes — they're the same credential under two names).

## Real TUI + live model: confirmed working, with one structural UX finding

The ratatui TUI was driven end-to-end with a real model for the first time this session (a real
pty, a real typed prompt, a real streamed response) — confirmed via a rigorous test that caught
and fixed its own false positive (the first prompt attempt echoed its own expected-output
substring back before any network round trip completed; fixed by using a base64-decode task so a
correct on-screen answer could only come from real model output). Confirmed the TUI and the plain
`-p` loop share the exact same `build_fabric()` call (`crates/tm-cli/src/agent.rs:373`) — there is
no separate, divergent provider-wiring path for the interactive TUI.

**Structural finding, root cause now confirmed precisely**: a turn that ends without a successful
`ticket.submit` is always classified `AgentOutcome::Failed{class: Other}` and rendered as
`failed (Other): ...`, and this is not limited to text-only replies — **it is structurally
unreachable for every bare `tm -p`/interactive scratch ticket**, confirmed directly (live `tm -p`
run + `crates/tm-core/src/machine.rs:94`): `(Draft, Cancel)`/`(Draft, Activate)` are the only two
valid transitions out of `Draft`, and `Submit` is only ever valid from `Running`
(`(Running, Submit) => Ok(Submitted)`, line 94) — but a scratch ticket created by a bare `-p`/
interactive turn is never run through the scheduler's `Activate → Ready → Leased → Running` chain
at all; `AgentLoop` just executes tool calls against it directly, still in `Draft`. So
`ticket.submit` returning `invalid transition: no transition from Draft on Submit` is not a rare
edge case, it's the *only possible outcome* if the model ever calls that tool from this path —
confirmed live: a real task (create a file, read it back, verify content) completed 100%
correctly, including real `edit.create_file`/`fs.read`/`evidence.attach` tool calls, and still
ended `failed (Other)` for exactly this reason. The real chat/tool-use experience genuinely works;
the success/failure classification is what's structurally broken for this entire everyday path.
Needs a real decision: either scratch tickets from bare `-p`/interactive mode should skip the
`Running`-gated `Submit` requirement entirely (a different, lighter completion signal), or they
should actually be run through the scheduler's real lifecycle before a turn starts.

Two more real errors surfaced in the same live run, both recovered-from by the model on retry so
worth noting but not blocking: `artifact.store`/`evidence.attach` rejected wrong-guessed enum
variant names (`"file-content"`, `"file"`) — a real signal that a cheaper/smaller model (this was
`muse-spark-1.3-contributor`) may need clearer tool-schema value hints than a larger model would;
and `shell.run -> error: io: No such file or directory (os error 2)` fired twice before a later
`shell.run`-adjacent step succeeded — worth a closer look at whether this is a real environment/
working-directory issue or another wrong-guessed argument, not yet diagnosed.

## `tm-pty`'s agent tools are fully built, tested, and unreachable by any real agent turn

`PtyCapability`/`pty.*` tools: built, unit-tested (23/23 passing), authority-gated per SPEC §22.4
— and never registered anywhere a real agent turn can reach them. `crates/tm-cli/Cargo.toml` does
not depend on `tm-pty` at all (compare `BrowserCapability`/`ComputerCapability`, both wired in at
`crates/tm-cli/src/agent.rs:393`/`405-406` — no `PtyCapability` equivalent exists anywhere in the
workspace outside the `tm-pty` crate itself, which only `tm-tui` depends on, as a dev/test seed).
A real, complete feature sitting fully dark. Needs either wiring in for real or a decision that
it's intentionally not agent-facing yet.

## D-002 terminal-UI consequences: real, evidence-based gap list (not urgent, but concrete)

A fact-finding pass (not a fix) against `docs/decisions/D-002-terminal-ui-stack.md`'s six explicit
"Consequences" found: (1) SIGTERM/SIGHUP and (2) SIGTSTP/SIGCONT are real, signal-tested via
`kill`/re-exec'd child processes, but no test asserts the tty was *actually* restored/re-entered
afterward — `teardown_terminal`/`setup_terminal` errors are silently swallowed
(`crates/tm-tui/src/runtime.rs:109-121`), and a real one was observed live during this testing
pass: `"tm-tui: failed to re-enter the terminal after SIGCONT: Device not configured (os error
6)"`, uncaught by the test suite because it only awaits a notification firing, not the `Result`.
(3) truecolor detection exceeds the D-002 ask (a real OSC 11/DA1 probe, not just env trust). (4)
synchronized-output enable/disable logic is tested; actual per-frame emission of the DEC 2026
escape sequences is not. (5) grapheme-width handling is careful and well-tested at the unit level,
but the "cross-emulator test matrix" D-002 explicitly asked for (vs. unit-level self-consistency)
doesn't exist. (6) the PTY-harness test layer is genuinely strong; the `insta` snapshot layer is a
declared dependency never actually invoked anywhere in `tm-tui` (zero `insta::` call sites); VHS
tapes are honestly documented as out of scope, not silently missing. None of this blocks real
usage (the live TUI test above passed), but it's real, itemized technical debt against a decision
doc that framed these as explicit requirements "each with a test."

Work that is specified and agreed but not yet built, in rough priority order. Anything here is
real, scoped work — not aspiration. Items that turn out to be wrong get deleted, not quietly kept.

## Four real bugs found by the first live DevPass round-trip (not fixed, report-only task)

D-005 (DevPass as default provider) was confirmed working end-to-end for real for the first time
this session: a real `tm -p` invocation got a real model reply ("DEVPASS") from the real LLM
Gateway backend, `muse-spark-1.3-contributor`, confirmed by source-level elimination (no other
provider was ever registered given the env) and an independent `curl` against the same
endpoint/key/model. Along the way, four real, pre-existing gaps surfaced, all still open:

1. **`tm provider test`/`tm provider status` are unimplemented stubs whose doc comments lie.**
   `crates/tm-cli/src/ops.rs:700-740` — `provider_test`'s doc comment claims it sends a real
   `CompletionRequest` and reports success/latency; the actual code unconditionally returns
   `{"status":"ok","latency_ms":0}` regardless of whether the named provider even exists.
   `provider_status` is an unconditional `{"status":"no_live_fabric"}`. Neither is real evidence
   of anything today.
2. **`map_finish_reason` doesn't handle the gateway's `"incomplete"` finish_reason.**
   `crates/tm-provider/src/providers/compat.rs:911-920` — a real, reproduced failure mode:
   `muse-spark-1.3-contributor` returns `finish_reason: "incomplete"` (reasoning tokens exhausted
   `max_tokens` before visible output) on a real 200 OK, real-billed response, and the unmapped
   reason falls through to `Err(ProviderError::MalformedResponse)`, failing the whole request —
   not retried (`MalformedResponse` isn't retryable). Fix: map `"incomplete"` to
   `StopReason::MaxTokens` alongside `"length"` at line 914.
3. **The actually-served model/provider (`StepRecord.served_by`) is computed but never surfaced
   anywhere.** `crates/tm-agent/src/agent_loop.rs:759` computes it from the real wire response,
   but `crates/tm-cli/src/agent.rs`'s `format_step` drops it when rendering, and `StepRecord` is
   never persisted to an event/`project.db` either. There is currently no `tm` surface, live or
   historical, that lets an operator confirm which provider/model actually served a turn.
4. **`tm --json -p <prompt>` does not emit JSON**, contradicting `args.rs`'s own module doc
   ("every subcommand's JSON schema is stable and snapshot-tested"). `agent.rs`'s `run_turn`
   `on_event` closure always calls `renderer.note()` with preformatted plain text, never checking
   `renderer.is_json()`. Fixing this would also let bug 3's `served_by` ride along for free.

Also noted, not a bug: the gateway's wire `"model"` field is provider-prefixed
(`"meta-contributor/muse-spark-1.3-contributor"`), not the bare configured `DEVPASS_MODEL` value —
worth knowing before anything string-compares served vs. configured model.

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

See `docs/decisions/D-017-session-ticket-executor-model.md` first: this item is not a standalone
nice-to-have, it's the other half of the session-as-ephemeral-view model that document names —
without attach/detach against durable server state, a "session" is just a shorter-lived version of
the same session-is-the-persistent-thing conflation that document exists to correct.

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

## Ecosystem parity, and where the native/plugin line goes (2026-09-16)

Three surveys: opencode's published ecosystem (38 plugins, 11 projects), the extension systems of
Claude Code, Codex and gemini-cli, and the IDE-based agents (Cursor, Zed, Cline, Roo, Continue,
Aider, Devin/Windsurf, Amp, OpenHands, Goose, Kilo).

The method that made this useful: **a plugin ecosystem is a map of what the product failed to make
native.** Where several people independently built the same plugin, that is an unmet core need, and
the count is the signal. Four separate opencode notification plugins. Two independent Neovim
frontends. Three plugins whose entire purpose is using a subscription you already pay for instead of
API credits. Six reimplementing spec-to-plan-to-implement with session continuity.

That last cluster deserves stating plainly: **six opencode plugins are building Ticketmaster.**
`conductor` (Context → Spec → Plan → Implement), `micode` (Brainstorm → Plan → Implement),
`subtask2`, `goal-plugin`, `background-agents`, `workspace`. They exist because opencode is a
transcript runtime with no project state, so everything durable has to be bolted on from outside.
That is the thesis of this repository, validated by people paying the cost of its absence.

### Table stakes — ship natively, these are no longer differentiators

- **MCP, as both client and server.** 11 of 12 IDE agents and all three CLI agents support it; only
  Aider does not, and it is criticised for it. Codex runs both directions (`codex mcp-server` lets
  other agents call Codex as a tool), which is the shape we want too.
- ~~**AGENTS.md.**~~ Done: `tm_context::sections::build_conventions` reads it natively, walking up
  from a ticket's claimed paths (`docs/decisions/D-013-hooks-agents-skills.md`). No proprietary
  filename was invented.
- ~~**Skills as progressively-disclosed procedure bundles**~~ Done: `.tm/skills/**` discovery,
  metadata-in-the-pack/body-on-`skill.load` (`docs/decisions/D-013-hooks-agents-skills.md`).
- ~~**Lifecycle hooks, using the event names Claude Code and Codex already share**~~ Done, shell
  handlers only (not in-process — see the decision doc's "what this costs" for why that's the
  honest first cut): `PreToolUse`/`UserPromptSubmit` can deny or rewrite;
  `PostToolUse`/`SessionStart`/`Stop` are observational
  (`docs/decisions/D-013-hooks-agents-skills.md`).
- **Sandboxing, with the approval policy as a separate axis.** Every serious tool separates "what is
  technically blocked" from "what needs a human yes" — Codex most explicitly (`sandbox_mode` ×
  `approval_policy`). We already have that split in authority versus escalation; say so in those terms.
- **Checkpoints, distinct from git.** 5 of 12 ship it and it is consistently named the top trust
  feature. gemini-cli uses a shadow git repo; Cursor and Cline use per-turn snapshots.
- **Git worktree isolation** as a first-class flag, per session and per delegated worker.
- **Browser control as first-party**, not a community bolt-on. All three CLI agents treat it as core.
- **OpenTelemetry** as the observability substrate, opt-in and off by default.

### Where we can actually win

- **Notifications.** The single clearest signal in the whole survey: *none* of Claude Code, Codex or
  gemini-cli ships rich desktop notification, and all three spawned near-identical community bridges
  (`terminal-notifier`, `ntfy`, OSC-9 capture). opencode has four competing plugins. Everyone needs
  "tell me when the agent needs me," nobody ships it.
- **Cost and token accounting with zero setup.** Codex has no built-in dollar view at all — the
  feature request was closed unshipped. Claude Code's cost data requires standing up your own OTel
  backend. Third-party log-scrapers exist for both. We have an append-only event log with usage on
  every provider call, so this is nearly free for us and structurally awkward for them.
- **Checkpoints and forking from the event log.** Everyone else bolts a snapshot mechanism onto a
  transcript. Ours falls out of replay, and forking a ticket's history at an arbitrary seq — already
  in this backlog — is something none of them can offer at all.
- **Hooks as enforcement rather than convention.** The competitive-research gap above says our effect
  boundary is policy, not enforcement. A hook that can deny or rewrite a call at the boundary is how
  that becomes structural, and it is rare: only Cursor has a general system among the IDE agents.
- **Auth arbitrage as routable capacity.** Three opencode plugins exist to spend a ChatGPT/Gemini/
  Antigravity subscription instead of API credits. The fabric already models free tiers as capacity
  with a daily ceiling; subscription-backed auth is the same idea with better economics, and it is
  demonstrably what people want badly enough to hack around billing for.
- **Context enrichment on read.** `opencode-type-inject` injects resolved TypeScript/Svelte types
  into file reads. We have `tm-codeintel`; enriching a read with its resolved types is a small
  addition to context compilation and a real quality win.
- **Secret redaction before the model call.** `opencode-vibeguard` redacts secrets and PII into
  placeholders and restores locally. We have nothing here and it belongs at the effect boundary.

### Registry and distribution

Continue.dev's **Hub** is the model worth copying: everything is a typed "block" (models, context,
rules, prompts, docs, MCP servers), composed into assistants, published git-ops style with
public/private/organization visibility. That is real registry semantics — versioning, scoping,
remixing — rather than a flat awesome-list. It maps directly onto our templates (§27) and wiki (§26).

Two cautions from the data. First, every official catalogue skews to vendor SaaS connectors: the
Anthropic marketplace is 308 plugins dominated by AWS/Azure/Atlassian-style integrations, and
Google's first-party extension org is 66 repos of Google Cloud surfaces. The interesting long tail —
test writers, memory, notifications, workflow — lives in unofficial community lists in all three
ecosystems. Second, `ocx` exists: an extension manager with portable isolated profiles, i.e. a
package manager for the plugin system. That is what happens when distribution is an afterthought.

### Surfaces the ecosystems say people want

Ranked by how many independent implementations exist: Neovim (two separate opencode frontends),
mobile web over a VPN/Tailscale (`portal`), desktop/web/mobile clients (`OpenChamber`, `CodeNomad`),
chat-ops (`kimaki`, a Discord bot; Codex is drivable from Slack and from `@codex` on a GitHub issue),
Obsidian, and Zellij/tmux integration. Our remote-control backlog item covers the transport for most
of these; what they add is that the demand is for *many thin clients*, which is an argument for
keeping `tm serve` and its event stream the real product boundary.

## Adapter layers (SPEC §28)

- `tm-acp`: ACP client (drive Claude Code, Codex, Gemini CLI, Goose through one adapter) and ACP
  agent (be drivable by Zed, JetBrains). Client first — it subsumes most of the D-001 adapter work.
- MCP client and server. Client so a worker reaches any MCP server *under its granted authority*;
  server so `tm` is a tool other agents can call.
- `tm-auth`: the auth adapter trait and its five kinds — API key, subscription OAuth (device/PKCE),
  cloud IAM (SigV4, ADC, Azure AD), platform ephemeral tokens minted per lease, delegated/none.
  Each reports its entitlement (quota class, rate limits, metered vs subscription, daily ceiling) so
  the fabric can route on it. `tm auth <provider>` for interactive login, OS keychain by default.
- Runtime adapters behind `Executor`: `builtin`, `acp`, `opencode` (its HTTP server), `pi`, `human`.
- A redaction test that fails the build if credential material can reach an event, a context pack, a
  `--json` payload, an error message or a rendered frame.

## The goal loop (SPEC §29)

- Goal and steps as events, so they survive death, compaction and handoff and replay identically.
- Explicit re-orientation each cycle: re-read the goal against observed state rather than trusting
  the model's memory of it.
- Bounded auto-continue, gated on measurable progress plus the cycle budget plus the global backstop.
- Completion claimed by the loop, decided by the verification ladder. Never self-verified.
- Live in the TUI, since a user watching a worker should see what it thinks it is doing.

## Context economy (SPEC §30)

- Tool-surface derivation from `Authority`: a disallowed capability contributes no schema at all.
  This is the highest-leverage item on this list and it is nearly free given the authority algebra.
- Per-section token accounting on every compiled pack; an unattributed section is a bug.
- Deterministic pruning from the event log: a re-read supersedes its earlier read, superseded results
  leave the working set but never the log. Must prune identically on replay.
- Progressive skill loading (metadata first, body on invocation).
- Context budget in `Authority`, checked at compile time. Over budget fails and names the sections;
  it never silently truncates.
- `tm doctor` flags sections that have not changed any ticket's outcome over a window.

## Budget-aware execution (SPEC §31)

- Remaining budget in the context pack, refreshed per goal-loop cycle, expressed as an affordability
  menu across reachable model tiers rather than a bare number.
- Worker behaviours: tier down before exhaustion, reprioritise toward durable progress under
  scarcity, refuse to begin an effect it cannot afford to finish.
- Budget handoff: finish or roll back the in-flight effect, persist goal state, release the lease,
  report. Explicitly NOT a failed attempt — no retry consumed, no Recovery transition. Needs a state
  machine change and a regression test, since the default reading gets this wrong.
- Scheduler: reserve verification budget before dispatch; refuse to strand a ticket it cannot fund;
  escalate when the remaining budget cannot finish the remaining graph.

## Ecosystem parity, as work items

- Native notifications (desktop, push, ntfy-style), since nobody ships this and everybody needs it.
- Zero-setup cost and token accounting off the event log, including per-ticket and per-worker spend.
- ~~Hooks with the Claude Code / Codex shared event vocabulary~~ Done, shell-only
  (`docs/decisions/D-013-hooks-agents-skills.md`) — `PreToolUse`/`UserPromptSubmit` deny or
  rewrite; `PostToolUse`/`SessionStart`/`Stop` observe.
- ~~`AGENTS.md` read natively.~~ Done (`docs/decisions/D-013-hooks-agents-skills.md`).
- ~~`SKILL.md` skills.~~ Done (`docs/decisions/D-013-hooks-agents-skills.md`). Checkpoints and
  forking off the event log. Worktree isolation per worker.
- Secret and PII redaction before the model call, restored locally (the `vibeguard` shape).
- Context enrichment on read: resolved types alongside a file read, from `tm-codeintel`.

## A head-to-head benchmark: opencode vs. Codex vs. Claude Code vs. Ticketmaster

Eventually, not now — this is a marker for later, once the standalone chat UX (D-003) and the
core coding loop are solid enough that a comparison is actually informative rather than noise.

The point isn't a leaderboard for its own sake: running the same task set through all four on a
schedule (or on every significant harness/architecture change) is how regressions in *our own*
product get caught before a person notices them by hand — a smoke-test tier for quick per-change
signal, and a larger periodic tier across a broader task set for the kind of drift a smoke test
is too shallow to catch. Feed findings back into the self-improvement loop rather than letting
them sit in a report nobody reads.

Auth, so this doesn't quietly burn real API budget on every run: default each tool to whatever
credential is cheapest/already sitting there rather than Anthropic's real metered API — opencode
and Codex both already read `OPENAI_API_KEY`/an OpenAI-compatible credential from the environment
for their own default paths, and DevPass (already a `tm-provider` backend,
`crates/tm-provider/src/providers/compat.rs`) is the equivalent zero-additional-cost option for
Claude Code and for Ticketmaster's own runs. Keep a real-Claude-auth switch available as an
explicit opt-in for the rare deliberate "how does actual Claude Sonnet/Opus perform here" run —
gate it behind a flag/env var so it's never the accidental default.

Shape (sketch, refine when actually built): a fixed task suite (bug-fix-with-tests, add-a-small-
feature, refactor-under-constraint, multi-file navigation) run identically against all four,
scored on objective signal already available for free from Ticketmaster's own kernel — did tests
pass, how many turns/tool calls, tokens spent, wall time, did verification actually catch what it
claimed to. Store results as real event-log/ticket state (dogfood the product to measure the
product) rather than a bespoke reporting format.

## Known bug: `resolve_genesis_provider` ignores the candidate's provider slug

`crates/tm-cli/src/project.rs::resolve_genesis_provider` (the provider construction for `tm
genesis`'s three frontier bootstrap roles — `vision.frontier`/`planner.frontier`/
`architect.frontier`) reads a `RoleCandidate`'s `provider`/`model` fields into a `ModelId` but then
unconditionally constructs an `AnthropicProvider` from it, regardless of what `provider` actually
names. Harmless today only because `RoleConfig::default_table()` has always named `anthropic` for
every role; pointing one of these roles at a non-Anthropic slug (e.g. `devpass`, once
D-005-devpass-default-provider's pattern is extended past `coder.fast`) would silently construct
the wrong provider rather than fail loudly. Found and deliberately left out of scope while building
D-005 (see that decision doc). Fix: route through the same provider-registry lookup
`crates/tm-provider/src/fabric.rs::Fabric::execute` already uses for every other role, rather than
hardcoding a single provider type at the genesis call site.

## Open decision: should `-p` exit non-zero on an in-band agent failure?

`crates/tm-cli/src/agent.rs::run_turn_streaming` deliberately returns `Err` only for an
infrastructure failure (network, storage) and `Ok(AgentOutcome::Failed {..})` for the agent's own
turn failing in-band (e.g. the model ends its turn without submitting) — confirmed live by the
Reconciliation Gate: `tm -p "<prompt>"` prints `failed (Other): ...` to stdout but exits `0`. This
is intentional (the doc comment is explicit about the distinction), not a bug, but `args.rs`
documents `-p` as "suitable for scripting," and a script that only checks the exit code would see
success on a turn the CLI's own output just labeled failed. Needs a real decision, not a silent
fix: either exit non-zero for `AgentOutcome::Failed` under `-p`/`--json` specifically (a real
behavior change for anything already scripting against today's exit codes), or document the
current split plainly in `args.rs`'s own `--help` text and `docs/` so a script author can't miss
it. Not resolved yet — pick one deliberately before calling scripting support done.

## Resolved: `docs/decisions/*.md` vs. `tm-wiki`'s `Decision` model

Was open: `SPEC.md` §26.2 specified the wiki's "decision history" page as derived from
`tm_core::Decision`/`DecisionId` (a `Store`-backed entity via `tm decision new`), while this repo's
actual, exclusively-used convention was hand-authored `docs/decisions/D-NNN-*.md` files — the two
were never reconciled, and `docs/wiki/decisions.md` rendered "No decisions recorded yet" despite 14
real decision docs existing on disk.

Resolved: keep the hand-authored markdown convention as the one real source of truth.
`crates/tm-wiki/src/decisions.rs` now reads `docs/decisions/D-NNN-*.md` directly instead of
`Store::view().decisions`; `SPEC.md` §26.2/§26.3 are corrected to match. `tm_core::Decision`/
`DecisionId` and `tm decision new`/`tm decision supersede` are unchanged and still work — they are
simply no longer the wiki's source for this page family, leaving open whether they still have a
genuine separate use. Full reasoning, the rejected alternative, and what this trade costs:
`docs/decisions/D-015-decision-docs-are-the-wiki-source.md`.

## Stretch

- iOS simulator executor (SPEC §23).
- Additional model providers beyond Anthropic in the provider fabric.
- Additional tracker adapters beyond the shipped four.
