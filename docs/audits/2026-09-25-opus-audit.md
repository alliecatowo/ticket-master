# ticket-master (`tm`) end-to-end audit, 2026-09-25

Binary: `integrate` worktree at 3181f6e, debug build (`mise run build`, 12s incremental), copied to
`/tmp/tm-audit/tm`. Every run used scratch dirs under `/tmp/tm-audit/` with `TM_HOME=/tmp/tm-audit/home`.
Timeouts came from a perl wrapper (`/tmp/tm-audit/to`), since macOS has no `timeout`.

**Live providers.** `.env` supplies DEVPASS_* and LLM_GATEWAY_API_KEY. The only live provider is
**devpass `muse-spark-1.3-contributor`**, a reasoning model. No Anthropic or OpenAI key is set, and the decider
role is the mock. Every live number below comes from that one model, so any comparison with Claude Code is
model-confounded.

## Headline

tm's *chat* path works on a small greenfield task. A one-prompt Python todo CLI got 10 passing tests in 70s.
The *ticket* path, which is the product's reason to exist, does not yet get a ticket from request to
submission on a real repo. Genesis dies at its first stage on the only live model. The mechanisms the vision
leans on are built and unit-tested, but none of them is wired into a real command path: Potion semantic
search, the System-One decider, verification separation, and context-pack prefetch. The most damaging bugs
are cheap to fix: an uncapped `search.exact`, an all-or-nothing context pack, and a providers.toml frozen at
`tm init`.

## 1. Can tm work on ticket-master itself (dogfooding)? — **No, not yet.**

Setup: `git clone /Users/allie/Develop/ticket-master /tmp/tm-audit/self`, then `tm init` (0s, builds no index),
then `tm doctor` (57s; this is where the first index is built: "634 added, 3146 chunks, 502 commits"; the
index is 80MB).

- **Attempt 1:** `tm run T-1` failed in 25s with `no candidate can serve role coder.fast: provider not
  registered: anthropic`. `tm init` had run without `.env`, so `.tm/providers.toml` was written with only
  Anthropic candidates. Loading `.env` later does not help: the role table is frozen at init. The same happens
  to chat (`/tmp/tm-audit/split`: init without `.env`, then `tm -p` with `.env` gives `no usable model is
  configured for coder.fast`). There is no `tm provider` verb to repair it (only list/detect/status/default/test).
  To continue, I copied a devpass-routed providers.toml over it by hand.
- **Attempt 2** (objective describes the bug but doesn't name the file): 18 tool calls in 253s, **1,025,330
  tokens** (`tm stats`), no edits, then `model ended turn without submitting` (exit 2). From the event log
  (`sqlite3 .tm/project.db`), each provider request was 36k–94k tokens. The first request after one
  `search.exact` was already 67,870 tokens. Cause: `search.exact` returns up to `DEFAULT_HIT_CAP = 1000` hits
  with the full line text of each (`crates/tm-codeintel/src/exact.rs:30`, `crates/tm-agent/src/tools.rs:995`),
  and the tool has no limit or path parameter. Measured: `tm search --exact provider --limit 100000 --json` on the clone
  returns 1000 hits and **177,808 bytes (about 45k tokens)**, which is one agent call's result. (MCP's `search_exact` and the CLI default
  are capped at about 20 hits; only the agent tool is uncapped.)
- **Context pack:** `tm ticket context T-1` gives **478 tokens across 7 sections**. It dropped "Search hits —
  would have needed ~93427 tokens", "Project conventions ~12197" and "Wiki pages ~1771". `assemble`
  (`crates/tm-context/src/pack.rs:345-388`) admits a section whole or drops it whole. So on a big repo the
  prefetch contributes nothing, while the budget-status section (376 tokens) is bigger than the objective.
- **Attempt 3** (T-2, which names `crates/tm-cli/src/project.rs` and `doctor()`, run with `--record`): see
  the result in the addendum at the end.
- Observability: `tm run --plain` prints `* Ran a command`, `* Read a file -> error: couldn't read or write a
  file`, with no command, path or query (`crates/tm-cli/src/agent.rs:2297` `plain_tool_action`).

Verdict: navigation of a big repo fails because of context blowup and empty prefetch. The submit protocol
also fails a run whose model ends on plain text, with no nudge (`crates/tm-agent/src/executor.rs`
`"model ended turn without submitting"`).

## 2. One prompt → working software (Genesis)? — **No via Genesis; yes via plain chat.**

- `tm genesis --plain --prompt "A todo CLI in Python ... with pytest tests."` (devpass) ran twice
  (`genesis1.log`, `genesis2.log`). Both times: `genesis: reading the prompt` then `error: parse: completion has
  no text content`, exit 1, about 30s. `analyze_prompt` uses `max_tokens: 2048` (`crates/tm-genesis/src/seed.rs:199`).
  That fits the reasoning-model truncation that `compat.rs:921` already documents for this gateway, but I did
  not confirm the root cause (debug logs showed nothing more).
- Under `TM_TEST_MOCK_PROVIDER=1` the pipeline runs: `reading the prompt → vision → spec → ticket graph →
  ignition`, then prints `2 tickets committed under milestone M-1. Run tm sched run ... then tm genesis
  --resume`. **That advice dead-ends.** The tickets are `draft`, `tm sched plan` prints "No scheduler actions
  planned", and `tm sched tick` prints "No work to do right now". D-027 deliberately chose not to activate.
- Nothing verifies the work automatically. SPEC.md:721-724 ("Verification separation, non-negotiable": V-*
  verification tickets, A-* audit tickets) and :308 (`Submitted -> Verifying automatically`) have no
  implementation. The only production call of `Store::verify` is the HTTP route (`crates/tm-server/src/routes.rs:846`).
  Genesis's maturity gate scores the pass rate of `TicketKind::Verification` tickets
  (`crates/tm-genesis/src/maturity.rs:95`), which nothing creates.
- For contrast: `tm --json -p "Build a todo CLI ... run pytest and make them pass"` in an empty dir gave `replied`
  in 70s, 10 steps, **217,262 tokens**, and `10 passed`. The chat agent loop is competent.

## 3. Is context usage good? Is semantic search used on real paths? — **No.**

- **Potion is never used.** No production code calls `CodeIntel::open_auto`/`open_at_auto`. The real call sites
  all use hash-only `open_at`: `Project::code_intel` project.rs:91, `doctor` project.rs:2221, dispatch.rs:447,
  tm-mcp server.rs:386, and genesis attach.rs:430. The model is cached on this machine
  (`~/.cache/huggingface/hub/models--minishlab--potion-code-16M-v2`). `embed.rs:222`'s comment claims "real
  command paths use" it, which is false.
- Semantic quality: `tm search --semantic "where does the scheduler decide a ticket is ready"` on the clone gives
  D-008, D-023, TASKS.md and thesis.md, all at **0.51–0.52**, and no scheduler code. The hybrid ("lease heartbeat
  expiry") works only because the FTS side carries it. MCP `search_hybrid` works the same way.
- Tool-output bloat (§1): an uncapped `search.exact`, 1M tokens per failed ticket.
- Context prefetch is all-or-nothing (§1). Nav tools exist and are reachable from MCP (15 tools:
  search_*/symbol_*/history_*/ticket_*), and `tm symbol outline todo.py` works.
- Chat baseline: `tm -p "Reply with exactly: hello"` costs 7,109 tokens (reasonable). On bench tasks, tm used
  122k–178k tokens over 8–11 steps; Claude Code used about 94k (mostly cache reads) over 5 turns.

## 4. Stubs, fake-done, dead code, machine-speak, incoherent hierarchy — **Several fake-dones; copy mostly good.**

"Done" claims I checked:
- `d20-shadow-triage-new-tickets` ([x]): **done in tests only.** `Store::with_decider` has no production caller.
- D-025 Potion ([x], docs claim "real command paths"): **not wired** (see §3).
- B3/D-027 genesis termination ([x]): it terminates, but the resume advice doesn't work (drafts).
- B1 index freshness ([x]): **true.** A file created after init is found by search and symbol outline.
- B8 MCP nav toolset ([x]): **true** (15 tools listed and callable).
- B14 `tm stats` ([x]): **true**, but it can't show where tokens went.
- B21 template catalog ([x]): `tm templates list` prints "No templates declared in .../templates.toml" with no
  built-in catalog shown.

Copy and hierarchy:
- `SPEC.md §` in user output: `tm doctor` ("(SPEC.md §20.3)", tm-computer macos.rs:633), `tm acp --help`
  ("§28.1"), drive.rs:142 ("§19.1a"), workflow.rs:267 and project.rs:2153 ("§25.3"). Hygiene misses these.
- `tm doctor`: `providers ok` next to "no model provider is ready"; first index reported as "repaired incremental
  drift"; a fresh repo with no commits gives `index-health FAIL git2: reference 'refs/heads/main' not found`
  and a non-zero exit.
- `tm history why todo.py` (uncommitted file) gives "repository may be corrupted".
- `tm workflow list` prints a bare `NAME NODES PARAMS 1X1?`.
- `tm genesis --resume` with no snapshot gives a provider error.
- Ticket kind: the CLI help says `task` (default), lists show `work`, and `POST /tickets {"kind":"task"}` returns 400.
- `tm "some words"` without `-p` gives `required --prompt` (unlike `claude "…"`).
- Every subcommand `--help` repeats five global flags as long paragraphs.
- `tm acp` works but isn't in `--help`'s "More commands" list. `stats` is in the main list but not in CLAUDE.md's.
- TUI: the chat welcome box advertises `anthropic/claude-sonnet-5` when no key exists, and the first message
  fails. The tickets screen is clean and Claude-agents-like. The **board** (Tab) shows raw states
  `draft/blocked/ready/leased/active` with no tab strip and columns cut off, which breaks D-024's one-label
  rule. The `?` panel doesn't mention Tab. Ctrl+C twice doesn't quit while the `?` panel is open.
- `tm serve`: `/health`, `/state` and `/app/` (200) work. `tm mcp`: `initialize` works but always answers
  protocolVersion 2024-11-05.

## 5. Is the Jev/Laya System-One decider (D-020) real and useful? — **Plumbing yes, product no.**

How far it goes:
- **Trait:** real. `DecisionProvider` in tm-provider/src/decide.rs, with `MockDecisionProvider` and `Role::Decider`.
- **HTTP /v1/systemone client:** real. `providers/systemone.rs` (`SystemOneProvider::from_env/with_token`,
  AI_GATEWAY_API_KEY/TYPESAFE_API_KEY, base-URL override), with redaction through `SessionRedactor`
  (`redact_decide_request`).
- **Config:** real. `[decider]` candidates parse in role_config.rs (mock, systemone, systemone-http, base_url,
  token_env), and `Registry::build_decider` dispatches them.
- **Event:** `classify.decided` payload/kind exist. There is an offline decision-eval in bench.
- **Shadow triage:** `TriageDecider` hook in `Store::create_ticket` (store.rs:268, 616). **Not connected:**
  nothing adapts `DecisionProvider` to `TriageDecider`, nothing calls `with_decider` or `build_decider` outside
  tests, and `tm provider list` shows `decider mock mock-decider not-configured`. No `classify.decided` event
  can be produced by any `tm` command today. `AI_GATEWAY_API_KEY` isn't in `.env`, so no live test was possible.
- Useful? Not yet. It needs (a) wiring at project open, (b) a real site that *acts* (review-band routing,
  tier-down, approval escalation per D-020 §4-5), and (c) an eval that compares shadow decisions with the LLM path.

## 6. Competitive position vs. SPEC vision

Head-to-head (bench fixtures, same prompt, independent `python3 -m unittest` check, `/tmp/tm-audit/h2h`):

| task | tool (model) | result | wall | tokens / cost |
|---|---|---|---|---|
| py-merge-counts-overwrites | tm -p (devpass muse-spark) | PASS | 38.0s | 122,428 tok, 8 steps, $0 |
| | claude -p (opus-5-5) | PASS | 17.9s | ~94k tok (70k cache-read), 5 turns, $0.214 |
| | opencode run (default) | PASS* | 14s | n/a |
| py-binary-search-bound | tm -p | PASS | 38.5s | 178,353 tok, 11 steps |
| | claude -p | PASS | 16.9s | 5 turns, $0.218 |

\*opencode hung for the full 300s with stdin open (rc=124 on both tasks). With `< /dev/null` it passed in 14s. The
cross-tool xtask (D-033) must close stdin. tm ran through the chat path, not the ticket executor. The models
differ, so read this as "tm's loop is functional, about 2× the steps and wall time".

Feature matrix (✓ solid, ~ partial or unwired, ✗ none; competitor cells from 2026 sources and my knowledge,
marked † where recalled rather than sourced):

| capability | tm | Claude Code | Codex CLI/cloud | OpenCode | Aider† | Cline/Roo† | Devin/Factory/Cursor cloud |
|---|---|---|---|---|---|---|---|
| interactive chat agent | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| background/parallel workers | ✓ (scheduler, leases) | ✓ subagents/bg | ✓ cloud tasks, parallel | ✓ subagents | ✗ | ~ | ✓ cloud VMs |
| durable ticket graph, deps, milestones | **✓ unique** | ✗ | ✗ | ✗ | ✗ | ✗ | ~ (Factory Linear/Jira intake, Cursor from Linear) |
| hash-chained event log, replay, record/replay cassettes | **✓ unique** | ✗ | ✗ | ✗ | ✗ | ~ checkpoints | ✗ |
| independent verification before "done" | ~ (spec'd, unwired) | ~ hooks | ✓ runs tests in sandbox | ✗ | ✓ lint/test loop | ~ | ✓ |
| LSP diagnostics fed back after edits | ✗ | ~ (plugins) | ✗ | **✓** | ✗ | ✓ | ~ |
| repo map / semantic retrieval | ~ (hash embedder, symbols ok) | agentic grep | agentic grep | LSP | **✓ repo map** | ✓ | ✓ (DeepWiki†) |
| prompt → project (spec → plan → build) | ~ (Genesis broken live) | ~ plan mode | ~ | plan agent | ✗ | plan/act | ✓ Factory spec mode / "Missions" |
| multi-provider routing by role, budgets | **✓** | ✗ | ✗ | ✓ any model | ✓ | ✓ | ✓ BYOK (Factory) |
| tracker integration | ~ mirror (GitHub/Linear/Jira adapters) | ~ MCP | ~ | ✗ | ✗ | ✗ | ✓ first-class |
| MCP server of its own work | ✓ `tm mcp` | ✓ | ✓ | ~ | ✗ | ✗ | ✗ |
| cheap "system-one" classifier layer | ~ (D-020 unwired) | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |

What tm has that no one else does: a deterministic, replayable, hash-chained ticket state machine with
leases, budgets and role routing, exposed through the CLI, TUI, HTTP, MCP and ACP. The SPEC §0 thesis
("deterministic machinery beneath nondeterministic intelligence") is visible in the code. What competitors do
better: the loop hygiene that makes a single run succeed (bounded tool output, LSP feedback, test-then-stop
discipline, prompt caching) and first-class tracker intake.

### New ideas to adopt
1. **Mechanical verification step** (Codex/Aider-style test loop) as the *first* slice of SPEC's
   verification separation: run declared commands after submit, then human audit. (Task u1-automatic-verification-step.)
2. **LSP/compiler diagnostics after every edit** (OpenCode). tm already has tm-codeintel. Adding `cargo check`/
   `tsc`/`pyright` diagnostics to the edit tool result would cut failed attempts. (Follow-up, not in the task list.)
3. **Tracker-as-intake** (Factory/Cursor: assign a Linear issue, get a PR). tm-mirror is push-only. A pull
   direction ("adopt issue #N as a ticket") would make tm usable from where teams already work.
4. **Best-of-N attempts under D-012 worktrees** (Codex cloud†): the scheduler already has attempts and leases.
   Racing 2 cheap attempts and keeping the one that verifies fits the thesis.
5. **Dogfood as a gate:** `mise run dogfood` (task u1-dogfood-e2e-smoke). A harness whose core promise is never
   exercised end to end regresses silently. Each of this audit's 5 critical tasks would have shown up there.
6. **System-One where it pays:** start D-020 at *routing* (pick coder.fast vs coder.deep, and "does this ticket
   need decomposition?"), since a wrong answer there is cheap and measurable.

### Consolidations
- **One provider-resolution path.** Chat, workers and genesis each apply their own fallback. Genesis silently
  swaps anthropic for devpass; chat and workers don't. Make the role table env-aware at load (task
  u1-provider-table-follows-env), then delete the per-surface fallbacks.
- **One CodeIntel open** (`open_at_auto`) for every surface.
- **One set of state labels** across the tickets screen, board, `tm ticket list` and the API (`work` vs `task`,
  raw states on the board).
- **One tool-activity labeller** for the chat transcript and `tm run` output.

### What to cut or demote
- `tm computer`/`tm browser`/iOS-simulator surfaces and the macOS Swift client are far from the core loop, and
  doctor warns about computer-use on every run. Hide the computer-use row unless `computer.toml` exists.
- The `harness`/`bench compare`/`promote` epoch machinery has little value until single tickets succeed;
  freeze feature work there.
- `tm-mcp-server` (the older standalone, Content-Length framing): deprecate in favour of `tm mcp`.
- `wiki` generation: fine, but keep it out of the context pack until the pack trims sections instead of
  dropping them.

## Artifacts
- Logs: `/tmp/tm-audit/dogfood.log`, `dogfood2.log`, `dogfood3.log`, `genesis1.log`, `genesis2.log`, `chatgen.json`,
  `h2h/results.txt` and the `h2h/*.out` files, `serve.log`.
- Tasks: `/tmp/tm-audit/TASKS-new.md` (35 tasks: 5 critical, 8 high, 14 medium, 8 low).

Sources (competitive): [Northflank: Claude Code vs Codex](https://northflank.com/blog/claude-code-vs-openai-codex),
[Composio: OpenCode vs Codex](https://composio.dev/content/codex-vs-opencode),
[Nimbalyst comparison](https://nimbalyst.com/blog/claude-code-vs-codex-vs-opencode-definitive-comparison/),
[Background agents compared (amux)](https://amux.io/guides/background-agents-compared/),
[Factory Linear/Jira](https://factory.ai/product/ai-project-manager),
[Factory review 2026](https://www.digitalapplied.com/blog/factory-ai-multi-agent-coding-platform-review),
[Tech Stackups harness comparison](https://techstackups.com/comparisons/coding-agent-harness-comparison-2026/),
[Vellum best agents](https://www.vellum.ai/blog/best-ai-coding-agents).

## Addendum: dogfood attempt 3 (T-2, file named, `--record`)

`tm run T-2 --plain --record /tmp/tm-audit/dog2.cassette`, bounded to 580s: **killed at the bound (EXIT 124)**
after **47 tool calls, 2,534,924 tokens, 521s** (`tm stats`). It never submitted. It did make an edit
(`git diff --stat`: `crates/tm-cli/src/project.rs | 6 +++---`). The substantive change is right:
`provider_doctor_detail` went from `ok: true, required: true` to `ok: any_ready, required: false`. **But the
same edit corrupted a neighbouring doc comment**, duplicating a phrase: `a smell, not a regardless -- a 1x1
workflow is a smell, not a`. That fits the byte-offset `edits: [{byte_start, byte_end, replacement}]` edit
shape the chat trial also showed, which is presumably what the edit-hash-fix track addresses.

After the kill, **the ticket stays `active`** (lease held) and **no cassette was written**, because `--record`
saves only at the end, so the one artifact meant for diagnosing a failed run is lost exactly when a run fails.

Revised Q1 verdict: naming the file gets tm to a correct core edit, so navigation plus context is the dominant
problem, not the model's ability. But even then the run burns about 2.5M tokens (uncapped tool output, about
54k tokens per call), doesn't converge to a verified submit within 10 minutes, and can damage nearby text.
Dogfooding is **not viable yet**. The fix order is u1-agent-search-exact-limit, u1-context-pack-fit-sections,
u1-worker-submit-nudge, then the dogfood smoke gate.
