# TASKS — the working task list

Conventions: `docs/tasks/README.md`. Source: the 2026-09-23 benchmark-readiness audit (workflow `wf_87a85411-848`, planner output), plus tasks found by live trials of every surface (section **T**). Worked by the `tasks-all` workflow: Sonnet/Haiku implementers in worktrees, at most two Rust builds at once, merged one at a time with a crate test + clippy, and a full `mise run verify` every few merges.

Decision numbers: D-021 and D-022 are the provider/config docs from the parallel provider-overhaul session, D-023 is the capacity-wait decision. A task marked `[new decision: X]` writes the next free `docs/decisions/D-NNN-*.md` when it lands.

Not here: D-020/jev (deferred, `docs/backlog.md` "Ask the owner later"); the web e2e suite (parked); provider/model/config UX (owned by the provider-overhaul workflow in `workflows/`, don't duplicate it).

Plan summary: tm is close to benchmark-ready on primitives, but the pieces are not wired together. The event log is hash-chained, tm-codeintel's four retrieval modes are real, and tm bench list/run/compare, D-012 worktrees, D-008 snapshots, and MockProvider scripting all exist. What blocks a trustworthy benchmark is a set of verified wiring bugs. (1) Nothing on the CLI, chat, dispatch, agent-tool or MCP paths calls CodeIntel::update_incremental. On a project that was never doctored, symbol and history queries come back empty, and files an agent creates mid-run never enter the index. The tool set holds a single Arc<CodeIntel> for the whole session (tools.rs:793, dispatch.rs:267). (2) Symbol refs and callers count a definition's own name as a reference, and ids are positional, so they shift when the file set changes. (3) `tm events replay` and `tm events tail` are stubs; I read both (ops.rs:2199-2289). (4) `tm genesis` cannot run offline, and with a real key it never terminates: tickets are committed as Draft and the failed MaturityGate/Stabilization cycle loops forever (stages.rs:183-190, project.rs:1297-1315). (5) usage.recorded always has dollars_micros 0 and names no model, and tool calls are never logged as events. (6) Nothing records or replays provider traffic. (7) `tm bench run` only replays scripted text and scores TestsPass by matching a string; there is no --live mode. Client surfaces have separate wire bugs: the vscode extension's stand-in client uses the wrong casing and request shapes, and the TS SDK lacks accept/reject/retry. SPEC §9 doc staleness can never fire, and mirror push re-sends every ticket on every run. The plan fixes wiring and correctness first (B1-B9: index freshness on every path including in-run tools, symbol accuracy, the events replay/tail stubs, genesis termination and offline mock/resume/e2e, docs state, mirror idempotency, MCP navigation tools, client wire fixes). Batch 1 also covers the user's direct request: a release workflow, install guidance and `mise run install`. The GitHub repo is already private (origin, per CLAUDE.md), and B2 pushes main and cuts v0.1.0. Telemetry and record/replay come next (B9-B15). Real cost and model attribution, tool_call.completed events, a pure TicketMetrics fold and `tm stats` are covered by [new decision: telemetry and cost attribution]. The cassette format, `tm run --record/--replay`, tool-result replay and replay-diff are covered by [new decision: record/replay cassettes]. Replay is ordered with per-entry divergence reporting, because matching by request hash would diverge on the first call in a fresh tempdir. Only then does the benchmark come (B15-B18): the fixture schema, report rendering, SWE-lite-style fixtures, the live mode ([new decision: live benchmark mode], reusing the fold and the cassettes) and the cross-tool harness ([new decision: cross-tool benchmark]). Remaining features follow in B19-B22: genesis events, V0/V1 tags and the template catalog, docs staleness and attest, tm acp, Swift. D-020 comes last, in six owner-gated batches. Decision numbers are pre-assigned so tasks running in parallel don't collide: [new decision: genesis termination] genesis, [new decision: telemetry and cost attribution] telemetry, [new decision: record/replay cassettes] record/replay, [new decision: stable symbol ids] symbol ids, [new decision: live benchmark mode] live bench, [new decision: cross-tool benchmark] cross-tool. The orchestrator, not a subagent, should write this plan into docs/tasks/TASKS.md, leaving out D-020 as that README requires.

## T — Found in live trials

Trial agents drive each surface for real (ticket request → implementation → verification → accepted, Genesis, TUI, server/web, MCP, code navigation) and append concrete tasks here. These run before the plan batches below.

- [ ] **nav-potion-embedder** — Real semantic search: potion static embeddings, zvec-grep style
  model: sonnet · severity: high · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-codeintel/src/embed.rs`, `crates/tm-codeintel/src/potion.rs`, `crates/tm-codeintel/src/api.rs`, `crates/tm-codeintel/src/hybrid.rs`, `crates/tm-codeintel/Cargo.toml`, `Cargo.toml`, `Cargo.lock`, `CLAUDE.md`
  change: The semantic half of hybrid search runs on `LocalHashEmbedder`, a hash stand-in, so "semantic" search is lexical in disguise. The owner wants search like zvec-grep's (the tool this repo already uses), which embeds with model2vec static embeddings. Add a `PotionEmbedder` implementing `embed::Embedder` in a new `potion.rs`, using the `model2vec-rs` crate (MinishLab's official Rust port; pure Rust, CPU only, no ONNX). It uses `minishlab/potion-code-16M-v2` by default, the same model zvec-grep uses. Load it from the local HuggingFace cache (`$HF_HOME` or `~/.cache/huggingface/hub/models--minishlab--potion-code-16M-v2/snapshots/*`, already present on this machine). Download it at runtime only if it's missing and the user hasn't opted out; never download in tests. Make it the default for `CodeIntel` when the model is available; fall back to `LocalHashEmbedder`, logging once through tracing, when it isn't. Keep the hash embedder for tests: hygiene forbids network in tests. The vectors table is already keyed by `embedder.identifier()`, so switching embedders must re-embed the stale chunks on the next `update_incremental` rather than mixing vector spaces. Make sure `hybrid` fuses FTS and vector hits with RRF the way zvec-grep does, and that code chunks are symbol- or line-window-sized, not whole files. Add an `index.embedder = "potion" | "hash"` project config key, plus `TM_EMBEDDER` for overrides. Record the choice in a new decision doc (next free D-NNN; D-002's format).
  acceptance: On a clone of this repo, `tm search hybrid "where are tickets moved between states"` puts `crates/tm-core/src/machine.rs` in the top 3, and "how is the provider chosen for a request" puts `crates/tm-provider/src/fabric.rs` in the top 3; show before/after in the handoff. Unit tests cover the fallback path and re-embedding on an identifier change, and use a tiny fixture model or the hash embedder, never the network. Indexing this repo from scratch with potion takes under 60s on this machine.
  test: `mise run test:crate -- tm-codeintel`
  evidence: owner, 2026-09-23: "use potion like zvec grep for semantic search, basically zvec grep inspired"; `crates/tm-codeintel/src/embed.rs` only has `LocalHashEmbedder`.

- [ ] **release-web-assets** — Ship the web client in the release tarball, and let an installed `tm serve` find it
  model: sonnet · size: S · builds Rust: yes · area: release · deps: none
  files: `crates/tm-cli/src/serve.rs`, `mise.toml`, `.github/workflows/release.yml`, `scripts/install.sh`, `docs/install.md`, `README.md`, `CLAUDE.md`
  change: Branch `worktree-agent-a279c9fa9d538d6a2` (commit 5c7f7db) holds unfinished work: a `mise run release` task that packs `bin/tm` plus `share/tm/web` and a sha256 (refusing to pack `.env` or `.tm`), `serve.rs` looking for the web client at `<exe>/../share/tm/web`, `scripts/install.sh` (via `gh release download`, since the repo is private) and `docs/install.md`. Merge that branch into your worktree and reconcile it with main, which already has `.github/workflows/release.yml`, README install paths (edd3d7e) and release v0.1.0: keep main's README and add to it rather than replacing it, and make `release.yml` build the web client (pnpm) and pack it the same way the mise task does. Don't publish a release.
  acceptance: `mise run release` builds a tarball whose `share/tm/web/index.html` exists and which contains no `.env`/`.tm`; a test covers the installed-layout lookup in `serve.rs`; README and docs/install.md agree with each other and with release.yml.
  test: `mise run test:crate -- tm-cli`

- [ ] **s1-surfaces-decision-doc** — Write the command-surfaces decision doc (next free D-NNN)
  model: sonnet · severity: high · builds Rust: no · area: docs · deps: none
  files: `docs/decisions/D-0NN-command-surfaces.md`, `docs/decisions/D-019-claude-code-parity-shell.md`, `CLAUDE.md`
  change: Run `ls docs/decisions` and take the next free number (another track may claim the next-lowest first — re-check before naming the file). Write the design in the D-002 format (Status, Date, Supersedes, then Context, Decision, Why, "What this costs, stated plainly"). The Decision section covers: the visible/hidden CLI tree (daily/planning/serving/more groups, `#[command(hide = true)]` plumbing verbs), the slash-command table (existing plus `/context`, `/todos`, `/memory`, `/export`, `/doctor`, `/permissions`, `/review`, `/search`, `/run`, `/ticket`, `/board`, `/milestones`, `/timeline`, `/deps`, `/workflow`), the Tickets-hub tab strip (Tickets · Board · Milestones · Timeline · Graph), the one-set-of-display-labels rule, and ticket due dates. Add a one-line pointer in D-019's command list and in CLAUDE.md's surface paragraph.
  acceptance: The new file exists with the D-002 section headings. D-019 and CLAUDE.md reference it by its real number. `mise run hygiene` passes (no dangling D-NNN).
  test: `mise run hygiene`
  evidence: `ls docs/decisions | tail -2` -> D-022-unified-provider-model-config-ux.md, D-023-capacity-wait-is-not-a-failed-attempt.md

- [ ] **s1-dep-rm-emit-removed-event** — tm dep rm silently does nothing: emit ticket.dependency_removed
  model: sonnet · severity: critical · builds Rust: yes · area: cli/tickets · deps: s1-surfaces-decision-doc
  files: `crates/tm-core/src/store.rs`, `crates/tm-cli/src/tickets.rs`
  change: `dep_rm` in tickets.rs (~line 835) writes a `dependencies` field through `update_ticket`, but materialize never reads that field — edges live in `ticket_deps` and are only removed by the TicketDependencyRemoved event (materialize.rs:210, payload TicketDependencyRemovedPayload). Add `Store::remove_dependency(ticket, depends_on, actor)` next to `add_dependency` (store.rs:728); it returns not_found when the edge is absent and otherwise emits TicketDependencyRemovedPayload. Rewrite `dep_rm` to call it. Add a store unit test (add, rm, then view.graph has no edge).
  acceptance: In a tempdir project: `tm dep add T-2 T-1`, `tm dep rm T-2 T-1`, then `tm --json dep graph` lists no T-2->T-1 edge. Running rm a second time gives a clear "no dependency T-2 -> T-1" error.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`
  evidence: `sed -n 847,872p crates/tm-cli/src/tickets.rs` -> `let fields = serde_json::json!({"dependencies": updated_edges}); project.store.update_ticket(&ticket, fields, ...)` (materialize only deletes on EventKind::TicketDependencyRemoved). Confirmed independently: "tm dep rm T-2 T-1 --plain followed by tm dep graph --json still shows the edge."

- [ ] **s1-display-labels** — One set of human display labels for ticket state, kind, milestone state, dep kind, budget and authority
  model: sonnet · severity: critical · builds Rust: yes · area: cli · deps: s1-surfaces-decision-doc
  files: `crates/tm-cli/src/render.rs`, `crates/tm-cli/src/tickets.rs`
  change: Add `pub fn state_label(TicketState)`, `kind_label`, `milestone_state_label`, `dep_kind_label` (Hard -> "blocks", Loop -> "loop"), `budget_label` ("unlimited" for u64::MAX, else e.g. "50k tokens, 20 steps, $2.00") and `authority_label` in render.rs. Labels are lowercase and match exactly what `tm ticket list --state` parses (tickets.rs:590). Replace `format!("{:?}", t.kind/t.state)` at tickets.rs:336-337, 654, and the milestone list (~941). Add a test that feeds every TicketState's label back through the `--state` parser.
  acceptance: `tm ticket list` shows `draft`/`ready`/`in progress`-style lowercase states. `tm milestone list` shows `open`, not `Open`. Every label round-trips through `--state`.
  test: `mise run test:crate -- tm-cli`
  evidence: `grep -n '{:?}' crates/tm-cli/src/tickets.rs` -> 336/337/941 debug-format State/Kind/milestone state.

- [ ] **s1-ticket-show-human** — tm ticket show prints Rust Debug structs; render a readable summary
  model: sonnet · severity: critical · builds Rust: yes · area: cli/tickets · deps: s1-display-labels
  files: `crates/tm-cli/src/tickets.rs`
  change: Rewrite the text branch of `ticket show` (tickets.rs:368-384). Show State, Kind, Objective, Milestone, Depends on, Children as `T-4, T-5` (or `none`), Budget via budget_label, Authority via authority_label. Collapse Resources, Executor, Verification and Retry Policy to one plain line each (e.g. "Verification: tests must pass", "Retries: up to 3 attempts"), or omit when default. JSON output unchanged except budget renders as null/"unlimited" in text only. Also fix the objective-truncation byte-slice at tickets.rs:327-328 to use `.chars().take(47)` instead of `&[..47]`, which panics on multi-byte UTF-8 (emoji/accents). Add a unit test asserting no `{`, `Some(` or `18446744073709551615` in the output, and a test that a multi-byte objective doesn't panic.
  acceptance: `tm ticket show T-1` in a tempdir project has no braces, `Some(` or u64::MAX; lines read like `Budget: unlimited`. `tm ticket new "😀😀😀…30 chars"` then `tm ticket list` doesn't panic.
  test: `mise run test:crate -- tm-cli`
  evidence: `grep -n 'Authority:\|Budget:' crates/tm-cli/src/tickets.rs` -> 378/382 debug format; multiple independent trial agents confirmed the same `Authority { repository: RepoAuthority { ... } }` output; byte-slice truncation confirmed at tickets.rs:327-328.

- [ ] **s1-transition-error-copy** — Invalid ticket transitions explain the state and the next command
  model: sonnet · severity: high · builds Rust: yes · area: cli/tickets · deps: s1-display-labels
  files: `crates/tm-cli/src/tickets.rs`, `crates/tm-types/src/error.rs`, `crates/tm-core/src/machine.rs`
  change: `InvalidTransition { from, trigger }` (tm-core/src/machine.rs:16) carries no ticket id, so build the friendly text in the CLI layer: a helper in tickets.rs mapping a TmError::InvalidTransition from accept/reject/retry/activate/close/cancel/reopen/submit to a sentence like "T-3 is a draft, so it can't be accepted. Activate it first: tm ticket activate T-3", using state_label plus a small table of which verb is valid from each state. Change the `#[error("invalid transition: {0}")]` prefix (error.rs:25) to "can't do that from this state: {0}". Cover submit-without-evidence ("invariant violated: submission requires at least one evidence artifact") with "Provide at least one piece of evidence (code changes, test results, or documentation) with `tm ticket attach T-N --evidence <path>` before submitting."
  acceptance: `tm ticket accept T-1` on a draft prints a sentence naming the state and the next command; no `InvalidTransition`, `trigger`, `invariant violated`, or raw enum Debug (e.g. "Ready on Activate") anywhere in the output.
  test: `mise run test:crate -- tm-cli && mise run test:crate -- tm-core`
  evidence: `grep -n 'invalid transition' crates/tm-types/src/error.rs` -> 25: `#[error("invalid transition: {0}")]`; five independent trial agents hit the same "no transition from Draft on VerificationStarted"/"Ready on Activate" text.

- [ ] **s1-dep-graph-human** — tm dep graph prints DependencyGraph Debug and ignores its root argument
  model: sonnet · severity: high · builds Rust: yes · area: cli/tickets · deps: s1-display-labels, s1-dep-rm-emit-removed-event
  files: `crates/tm-cli/src/tickets.rs`, `crates/tm-cli/src/tui.rs`
  change: In `dep_graph` (tickets.rs ~884-905), replace `{:?}` of view.graph with an indented, human list: each edge as `T-A (objective) -> T-B (objective)` using dep_kind_label, sourced from view.tickets. When a TICKET argument is given, filter to only its transitive dependencies/dependents (today the code computes a subgraph label but still returns the full graph). Validate the root ticket exists (TmError::not_found otherwise). With no edges: "No dependencies yet. Add one: tm dep add <ticket> <depends-on>". Fix the Kanban column title at tui.rs:337 (`format!("{state:?}")`) to use state_label.
  acceptance: `tm dep graph T-2` prints only T-2's subgraph as readable lines, no Rust struct syntax. `tm dep graph T-99` errors "not found: ticket T-99" instead of returning the full graph. Kanban columns read `ready`, not `Ready`.
  test: `mise run test:crate -- tm-cli`
  evidence: `sed -n 896,904p crates/tm-cli/src/tickets.rs` -> `format!("Dependency graph (subgraph from {}):\n{:?}", ticket, view.graph)`; confirmed `tm dep graph T-2` still returns the unfiltered graph.

- [ ] **s1-milestone-new-show** — Add tm milestone new/show and validate --milestone on ticket new
  model: sonnet · severity: critical · builds Rust: yes · area: cli/milestones · deps: s1-display-labels
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/tickets.rs`, `CLAUDE.md`
  change: Add `MilestoneCommand::New { title, --ticket <T>... }` with `create` as a visible alias, calling the existing `Store::create_milestone` (store.rs:1694) and printing "Created M-1: <title>". Add `Show(MilestoneRefArgs)` printing title, state and member tickets with state labels plus a done/total count. When `milestone list` is empty: "No milestones yet. Create one: tm milestone new \"<title>\"". Make `tm ticket new --milestone M-9` fail with "no milestone M-9. Run `tm milestone list`..." when M-9 doesn't exist (today it's silently dropped). Add a CLAUDE.md line.
  acceptance: In a tempdir: `tm milestone new "v0" --ticket T-1` then `tm milestone show M-1` lists T-1. `tm ticket new x --milestone M-99` errors clearly instead of silently creating the ticket with no milestone.
  test: `mise run test:crate -- tm-cli`
  evidence: `grep -n 'List\|Close\|Reopen'` in MilestoneCommand (args.rs:535-541) -> only List/Close/Reopen, no New/Create/Show; `Store::create_milestone` exists at store.rs:1694 with no CLI caller; confirmed `tm ticket new "..." --milestone M-1` with no milestones creates the ticket and silently drops the milestone.

- [ ] **s1-help-text-scrub-and-hygiene** — Strip D-NNN, crate paths and type names from user-facing help, and add a hygiene check for it
  model: sonnet · severity: high · builds Rust: yes · area: cli/copy · deps: s1-milestone-new-show
  files: `crates/tm-cli/src/args.rs`, `crates/xtask/src/hygiene.rs`
  change: Rewrite every `///` doc comment on clap items in args.rs mentioning `D-0NN`, `crates/`, `docs/decisions`, `tm_*::`, backticked crate names, or jargon ("honestly refused", "bare-`tm` loop", "snapshot-tested", "assimilate", "chunks"). Move rationale to plain `//` comments for maintainers. E.g. Tickets becomes "Open the tickets view (or print tickets with --json)"; the `--project` flag drops `[crate::project::resolve_scope]`/`[crate::project::locate]`/"(unchanged since before D-003)" for plain English ("walking up for a `.tm` directory, then falling back to a project kept under $TM_HOME"); `--plain` drops "(D-002, ...)"; `--json` drops "snapshot-tested"; `tm computer`'s `--headless` help changes "honestly refused on macOS" to "not supported on macOS" (4 occurrences), and its module doc drops the backticks around `tm-computer`. Add a hygiene check flagging `D-\d{3}|crates/|docs/decisions|tm_[a-z]+::` inside `///` lines of crates/tm-cli/src/args.rs only.
  acceptance: `tm --help` and every `tm <verb> --help` contain no D-0, crates/ or `tm_` paths. `mise run hygiene` fails when one is reintroduced.
  test: `mise run hygiene && mise run test:crate -- tm-cli`
  evidence: `grep -nE 'D-0[0-9]{2}' crates/tm-cli/src/args.rs` -> 114, 324, plus `--project`'s rustdoc-style `[crate::project::resolve_scope]`/`[crate::project::locate]` links, confirmed by seven independent trial agents across different CLI verbs; `honestly refused on macOS` at 4 sites in ComputerSnapshotArgs/ComputerClickArgs/ComputerTypeArgs/ComputerKeyArgs.

- [ ] **s1-cli-tree-regroup** — Group tm --help into daily/planning/serving/more and hide plumbing verbs as aliases
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: s1-help-text-scrub-and-hygiene
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/tests/tui_launch.rs`, `CLAUDE.md`
  change: Use `display_order` so daily verbs (`tm`, `-p`, `init`, `status`, `tickets`, `ticket`, `run`, `search`, `symbol`, `doctor`) come first, then planning (`milestone`, `dep`, `decision`), then serving (`serve`, `mcp`). Add `#[command(hide = true)]` to `lease`, `harness`, `bench`, `browser`, `computer`, `sched plan/tick`, `events replay/verify`, `ticket submit/delegate` — all must still run. Add an `after_help` "More commands" list. Give `tm search` `--exact`/`--semantic` flags; keep `--mode` as a hidden alias. Don't touch `provider` or `auth`.
  acceptance: `tm --help` lists ~16 commands, grouped. `tm lease list` and `tm sched tick` still work. `tm search --exact foo` equals `tm search --mode exact foo`.
  test: `mise run test:crate -- tm-cli`
  evidence: current CLI has 31 top-level verbs, all visible; finding: "Search command UX: --mode flag less discoverable than subcommand style."

- [ ] **s1-tickets-json-shape** — tm tickets --json and tm ticket list --json disagree on fields
  model: haiku · severity: medium · builds Rust: yes · area: cli · deps: s1-display-labels
  files: `crates/tm-cli/src/tickets/overview.rs`, `crates/tm-cli/src/tickets.rs`
  change: Make `tm tickets --json` serialize the same view struct as `tm ticket list --json`, including `title` (short_title of the objective, overview.rs:531) and state_label strings. Extend whichever struct is missing fields rather than creating a third one. Add a test deserializing both outputs into one struct.
  acceptance: `tm tickets --json | jq '.[0]|keys'` and `tm --json ticket list | jq '.[0]|keys'` have identical key sets, and `title` is populated (not null) in both.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm tickets --json` shows `"title": "Fix the login redirect"` while `tm ticket list --json` shows `"title": null` for the same ticket; overview.rs:69-70 `pub title: String` only exists on the overview view.

- [ ] **s1-ticket-due-date** — Tickets get an optional due date (tm ticket new/edit --due)
  model: sonnet · severity: medium · builds Rust: yes · area: cli/tickets · deps: s1-cli-tree-regroup
  files: `crates/tm-core/src/ticket.rs`, `crates/tm-core/src/materialize.rs`, `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/tickets.rs`, `CLAUDE.md`
  change: Add `due: Option<chrono::NaiveDate>` (serde default) to Ticket (near `milestone`, ticket.rs:367). Persist through the existing ticket.created/ticket.updated field paths, migrating materialize.rs if tickets are stored per column. Add `--due YYYY-MM-DD` to `ticket new`/`ticket edit` (`--due none` clears it). Show it in `ticket show`/`ticket list`. A milestone's due date is the max of its tickets' due dates in `milestone show`. Parse errors say "use YYYY-MM-DD, e.g. 2026-10-01".
  acceptance: `tm ticket new x --due 2026-10-01` then `tm ticket show T-1` shows `Due: 2026-10-01`, surviving a replay/view rebuild.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`
  evidence: `grep -n 'due' crates/tm-types/src` -> no matches; probe deps-sched: "Add due-date field and CLI support to tickets."

- [ ] **s1-tui-hub-tabs** — Tickets hub gets a tab strip: Tickets, Board, Milestones, Timeline, Graph
  model: sonnet · severity: high · builds Rust: yes · area: tui · deps: s1-dep-graph-human
  files: `crates/tm-tui/src/screens/tickets.rs`, `crates/tm-cli/src/tui.rs`, `crates/tm-cli/src/tui/tickets_view.rs`, `crates/tm-cli/tests/tui_navigation.rs`
  change: Render a tab strip in the tickets screen header. Tab/Shift+Tab (currently ignored at tickets.rs:786) cycle ScreenId among Tickets, Kanban, Milestones, Timeline, Graph. The last three show a "coming next" placeholder until their own tasks land. Ctrl+B still jumps to Board; Esc from any tab goes back to chat. Fix the stale module doc at tui.rs:18 ("`b` opens the Kanban board"; it is actually Ctrl+B). Extend tui_navigation.rs with Tab-cycling coverage.
  acceptance: In `mise run tui` (tempdir project), `/tickets` then Tab moves the highlighted tab and the screen. The navigation test passes.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `grep -n 'Tab' crates/tm-tui/src/screens/tickets.rs` -> 786: `KeyCode::Tab | KeyCode::BackTab => {}`; ScreenId (tui.rs:405) has only Chat/Tickets/Kanban/Detail; tester (TUI): "missing the project-management views (milestones, timeline, calendar)."

- [ ] **s1-tui-milestones-view** — Milestones tab: progress per milestone, Enter filters tickets
  model: sonnet · severity: medium · builds Rust: yes · area: tui · deps: s1-tui-hub-tabs, s1-milestone-new-show, s1-ticket-due-date
  files: `crates/tm-tui/src/screens/milestones.rs`, `crates/tm-tui/src/screens/mod.rs`, `crates/tm-cli/src/tui.rs`
  change: New screen, one row per milestone: title, state label, a done/total progress bar, derived due date, sourced from ProjectView.milestones. Enter returns to Tickets filtered to that milestone (header shows the filter); Esc clears it. Empty state: "No milestones. Create one with tm milestone new". Add a buffer-render unit test at the bottom of the file.
  acceptance: With two milestones in a tempdir project, the Milestones tab shows both with correct counts; Enter narrows the Tickets list.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `ls crates/tm-tui/src/screens` -> no milestone screen exists yet.

- [ ] **s1-tui-timeline-view** — Timeline tab: ticket bars from the event log, today line, due markers, month grid
  model: sonnet · severity: medium · builds Rust: yes · area: tui · deps: s1-tui-hub-tabs, s1-ticket-due-date
  files: `crates/tm-tui/src/screens/timeline.rs`, `crates/tm-tui/src/screens/mod.rs`, `crates/tm-cli/src/tickets/overview.rs`, `crates/tm-cli/src/tui.rs`
  change: Ticket has no created/closed timestamps, so extend the existing event fold in tickets/overview.rs (`apply(kind, subject, ts, payload)`, line 220) to record first-seen and closed-at per ticket. Render one bar per ticket from created to closed (or now), grouped by milestone, with a today line and a due-date mark. `+`/`-` zoom between day/week/month; month zoom is the calendar grid (due tickets listed per day). Timeline has no text input so plain keys are safe. Use a fixed clock for unit tests.
  acceptance: A render test with three tickets at fixed timestamps shows bars in the right columns with a due marker; the Timeline tab shows real tickets in a tempdir project.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `grep 'created_at|closed_at' crates/tm-core/src/ticket.rs` -> none; overview.rs:220 already folds events with timestamps.

- [ ] **s1-tui-graph-tab-prune-dead-screens** — Wire the unused ticket_graph screen as the Graph tab; delete dead dashboard/command_palette
  model: sonnet · severity: medium · builds Rust: yes · area: tui · deps: s1-tui-hub-tabs
  files: `crates/tm-tui/src/screens/ticket_graph.rs`, `crates/tm-tui/src/screens/dashboard.rs`, `crates/tm-tui/src/screens/command_palette.rs`, `crates/tm-tui/src/screens/mod.rs`, `crates/tm-tui/src/screens/kanban.rs`, `crates/tm-tui/src/screens/ticket_detail.rs`, `crates/tm-cli/src/tui.rs`
  change: Feed `screens/ticket_graph.rs` from view.graph with labelled nodes and make it the Graph tab; Enter on a node opens ticket detail. Delete `dashboard.rs` and `command_palette.rs` (grep shows no users outside their own files besides doc links in kanban.rs:5,37 and ticket_detail.rs:121 — fix those doc references too). Re-grep `dashboard\|command_palette` across `crates` once more right before deleting.
  acceptance: The Graph tab shows the dependency edges of a tempdir project; dashboard.rs and command_palette.rs are gone with no dangling doc links, and the workspace builds.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `grep -rn 'dashboard::\|ticket_graph::\|command_palette' crates` (excluding own files) -> only doc comments in kanban.rs:5 and ticket_detail.rs:121; ScreenId has no Graph variant.

- [ ] **s1-slash-pm-views** — Slash commands /board /milestones /timeline /deps /ticket /run
  model: sonnet · severity: medium · builds Rust: yes · area: tui/chat · deps: s1-tui-milestones-view, s1-tui-timeline-view, s1-tui-graph-tab-prune-dead-screens
  files: `crates/tm-tui/src/chat/commands.rs`, `crates/tm-cli/src/tui/slash_views.rs`, `crates/tm-cli/src/tui/chat_ops.rs`, `crates/tm-cli/src/tui.rs`, `CLAUDE.md`
  change: This is the first of four chained slash tasks that all edit commands.rs — run them in order. Add CommandIds/COMMANDS rows for board, milestones, timeline, deps, `ticket <T>` (prints ticket-show text inline in the transcript) and `run <T>` (activates and queues the ticket, replying "Queued T-3; watch it in /tickets"). Put handlers in a new file, slash_views.rs; keep each arm in chat_ops.rs's match (~line 582) to one line. chat_ops.rs has uncommitted edits in the (untouched, per CLAUDE.md) `odw-integrate` worktree, so keep that diff minimal — this task must not touch `.claude/worktrees/odw-*`. Any count test must use `COMMANDS.len()`. Update CLAUDE.md's slash list.
  acceptance: Typing `/mil` in the chat shows /milestones in the popup, and Enter opens the Milestones tab. `/ticket T-1` prints a readable summary.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `crates/tm-tui/src/chat/commands.rs` CommandId has 17 variants (Help..Exit) with no PM views, no /ticket and no /run.

- [ ] **s1-slash-context-todos** — /context shows token use by section and the ticket's prefetched context pack; /todos
  model: sonnet · severity: high · builds Rust: yes · area: tui/chat · deps: s1-slash-pm-views
  files: `crates/tm-tui/src/chat/commands.rs`, `crates/tm-cli/src/tui/slash_views.rs`, `crates/tm-cli/src/tui/chat_ops.rs`, `CLAUDE.md`
  change: `/context` prints a table into the transcript: context-window size, and tokens used by system prompt, instructions (AGENTS.md), tools, conversation, free space, from the session's real token counts. When a ticket is attached, also list that ticket's context-pack sections (prefetched files/symbols) with a token count each, reusing tm-context's section accounting (`crates/tm-context/src/tokens.rs` SectionKind) — don't invent numbers. `/todos` toggles the Ctrl+T checklist.
  acceptance: In a chat attached to a ticket, `/context` lists the prefetched items with token counts that sum to the displayed total.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: probe prefetch (pass=false): "Add a way to see a ticket's context pack (what was prefetched) and its token cost."

- [ ] **s1-slash-search-review** — /search <q> inline hybrid code search, and /review of the working-tree diff
  model: sonnet · severity: medium · builds Rust: yes · area: tui/chat · deps: s1-slash-context-todos
  files: `crates/tm-tui/src/chat/commands.rs`, `crates/tm-cli/src/tui/slash_views.rs`, `crates/tm-cli/src/tui/chat_ops.rs`, `CLAUDE.md`
  change: `/search <q>` runs the same hybrid search `tm search` uses (crates/tm-cli/src/search.rs) against the project and prints the top 10 as `path:line  snippet`. When the index is not built yet, say so instead of printing an empty list (distinguish "not indexed" from "no matches" — same fix needed in s1-search-hybrid-empty-snippet's snippet plumbing). `/review [focus]` sends a turn asking the agent to review `git diff HEAD`, the way `/init` sends init_prompt.
  acceptance: `/search dependency graph` in a tempdir project with code lists matching paths with snippets. `/review` starts a turn whose prompt includes the diff.
  test: `mise run test:crate -- tm-cli`
  evidence: probe nav-semantic: "Distinguish 'index not built yet' from 'no matches' in tm search output"; the chat has no search command today.

- [ ] **s1-slash-memory-export-doctor-perms-workflow** — /memory, /export, /doctor, /permissions, /workflow
  model: sonnet · severity: medium · builds Rust: yes · area: tui/chat · deps: s1-slash-search-review, s1-surfaces-decision-doc
  files: `crates/tm-tui/src/chat/commands.rs`, `crates/tm-cli/src/tui/slash_views.rs`, `crates/tm-cli/src/tui/chat_ops.rs`, `CLAUDE.md`, `docs/decisions/D-0NN-command-surfaces.md`
  change: `/memory` opens the project's AGENTS.md in $EDITOR through the existing Ctrl+G editor path (creating it if absent). `/export [path]` writes the transcript as markdown (default `tm-session-<id>.md` in cwd) and replies with the path. `/doctor` runs the same checks as `tm doctor`, printed pass/fail. `/permissions` shows the current auto/plan/ask mode and what each allows; `/permissions <mode>` sets it, same as Shift+Tab. `/workflow [name]` with no name lists workflows like `tm workflow list`; with a name, starts it as a background ticket. Mark the command-surfaces decision doc "Implemented" for the slash table.
  acceptance: Each command appears in the `/` popup and does what it says in a tempdir project. `/export` writes a readable .md file.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: finding (workflow): "Add /workflow slash command to chat for discovering and running workflows"; CommandId lacks Memory/Export/Doctor/Permissions/Workflow.

- [ ] **s1-id-parse-and-lease-error-copy** — Plain-language errors for milestone/ticket/decision/lease ID parsing and lease conflicts
  model: haiku · severity: low · builds Rust: yes · area: cli/copy · deps: s1-milestone-new-show
  files: `crates/tm-types/src/id.rs`, `crates/tm-core/src/lease.rs`, `crates/tm-cli/src/tickets.rs`
  change: In the `id_newtype!` macro (id.rs ~109-114), replace the internal type name in the format string with a user-friendly noun per type ("Ticket identifier", "Milestone identifier", "Decision identifier", "Lease identifier", "Actor") so e.g. `tm decision show invalid-id` says "Decision identifier must look like D-<n>, got \"invalid-id\"" instead of "DecisionId must look like...". Do the same for ParticipantId/LeaseId's format-string errors ("lease IDs start with L- followed by 12 hex digits" instead of dumping the pattern). Replace the lease-acquire-on-already-leased error (currently AcquireError::NotReady, displaying as "ticket T-3 is not Ready" even when the real cause is an active lease) with "T-3 is already being worked by <holder> until <time>" — check for an existing lease before the Ready check, or special-case the message.
  acceptance: `tm ticket show foo` and `tm decision show foo` print one plain sentence with an example id and no Rust type names. `tm lease acquire <already-leased-ticket>` names the existing lease holder, not "not Ready".
  test: `mise run test:crate -- tm-types && mise run test:crate -- tm-core && mise run test:crate -- tm-cli`
  evidence: findings: "Invalid decision ID error shows internal type name instead of plain language" (`error: parse: DecisionId must look like D-<n>, got "invalid-id"`); "Technical format strings in ID validation error messages" (LeaseId/ParticipantId); lease: `tm lease acquire T-1 --actor agent:mock/worker-2` on an already-leased ticket returns `error: conflict: ticket T-1 is not Ready`.

- [ ] **s1-status-since-hours-error** — tm status --since-hours rejects bad input with a Rust parse error
  model: haiku · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/args.rs`
  change: Replace the default u64 parser for `since_hours` with a custom clap `value_parser` that validates a positive u64 and returns "Expected a positive integer (hours)" on failure, instead of surfacing the raw Rust parse error.
  acceptance: `tm status --since-hours invalid` prints "Expected a positive integer (hours)" (or equivalent plain text), not "invalid digit found in string".
  test: `mise run test:crate -- tm-cli`
  evidence: `tm status --since-hours invalid` -> `error: invalid value 'invalid' for '--since-hours <HOURS>': invalid digit found in string`.

- [ ] **s1-events-sched-copy-and-quiet** — Humanize tm events show output and fix sched plan/tick's --quiet and event-name jargon
  model: sonnet · severity: high · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/sched.rs`
  change: In `tm events show`, change `format!("{:?}", event.kind)` (ops.rs ~2248) to the Display impl (dotted event names like `ticket.leased` instead of debug `TicketLeased`), and humanize field labels ("Sequence"/"Type"/"Related to"/"When" instead of "Seq"/"Kind"/"Subject"/"Timestamp"). In sched.rs, add `if !renderer.is_quiet()` guards before `renderer.emit()` in `sched_plan` (~line 90) and `sched_tick` (~line 117), matching the pattern already used in `sched_run` (~line 185). Reword `event_to_summary` (sched.rs:728-755): 'Ticked' -> 'Scheduler ticked', with detail "No work to do" (0 actions), "1 action queued", or "N actions queued" instead of "N actions planned at <ISO timestamp>".
  acceptance: `tm events show 1` prints `Type: ticket.leased`, not `Kind: TicketLeased`. `tm sched plan --quiet`/`tm sched tick --quiet` produce no output on success. `tm sched tick` reads "Scheduler ticked: No work to do right now" instead of "Ticked: 0 actions planned at 2026-...".
  test: `mise run test:crate -- tm-cli`
  evidence: `tm events show 1` -> `Kind: TicketLeased` (should be `ticket.leased`); `tm sched tick --quiet` still prints `Ticked: 0 actions planned at 2026-09-23T21:01:58.400868Z`.

- [ ] **s1-search-hybrid-empty-snippet** — Hybrid search results show an empty snippet column
  model: haiku · severity: medium · builds Rust: yes · area: cli/search · deps: none
  files: `crates/tm-cli/src/search.rs`, `crates/tm-codeintel/src/api.rs`, `crates/tm-codeintel/src/hybrid.rs`
  change: Hybrid search's human output shows an empty Snippet column (JSON confirms `"snippet": ""` always). Exact/regex modes properly show the matched line (`h.line_text`). Fix hybrid to extract the snippet from the hit data or source file at the matched line, same pattern as exact mode (search.rs ~line 180).
  acceptance: `tm search --mode hybrid "println"` shows code text in the Snippet column, matching what exact mode shows for the same hit; JSON output's `snippet` field is non-empty.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm search --mode hybrid "println"` -> Location/Score/Snippet with an empty Snippet column, while `tm search --mode exact "println"` shows the code.

- [ ] **s1-symbol-error-exit-codes** — Symbol lookups fail silently: "Symbol not found" exits 0
  model: haiku · severity: medium · builds Rust: yes · area: cli/symbol · deps: none
  files: `crates/tm-cli/src/search.rs`
  change: In symbol_def/refs/callers/callees (search.rs ~384/429/473/519), replace "Symbol not found"/"No outline entries found" with "No symbol named `{name}` in this project", and exit 1 instead of 0. In symbol_outline (~545), distinguish "File {path} not found" from "File {path} contains no top-level definitions" and exit 1 in both cases. --json output keeps the same exit-1 behavior with an empty array/null.
  acceptance: `tm symbol def nonexistent_fn` prints the plain message and exits 1. `tm symbol outline no/such/file.rs` distinguishes missing-file from no-definitions and exits 1.
  test: `mise run test:crate -- tm-cli`
  evidence: "Symbol not found" and "No outline entries found" both exit 0 in four separate trial runs across the symbol/init/attach groups.

- [ ] **s1-decision-supersedes-and-list-copy** — Superseding decision loses the supersedes link; decision list shows the wrong column
  model: sonnet · severity: high · builds Rust: yes · area: cli/decision · deps: none
  files: `crates/tm-events/src/payload.rs`, `crates/tm-core/src/store.rs`, `crates/tm-core/src/materialize.rs`, `crates/tm-cli/src/tickets.rs`
  change: Add `supersedes: Option<DecisionId>` to `DecisionCreatedPayload`; populate it in `store.supersede()` and INSERT the real value in materialize (instead of always NULL). Also fix `decision_list()`'s Summary column, which renders `d.subject` (the semantic class, e.g. "decision") instead of `d.decision` (the actual decision text) — use `d.decision`, truncated to 40 chars as before.
  acceptance: `tm decision supersede D-001 'Use MongoDB'` creates D-002; `tm decision show D-002` (text and `--json`) shows "Supersedes: D-001" / `"supersedes": "D-001"`. `tm decision list` shows the decision text in Summary, not "decision".
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`
  evidence: after `decision supersede D-001 'Use MongoDB'`, both text and JSON `decision show D-002` omit D-001, and the DB stores `supersedes=NULL`; `tm decision list` shows `D-001  Superseded  decision` / `D-002  Active  decision` instead of the actual text.

- [ ] **s1-workflow-error-message-ux** — Workflow-not-found error dumps an internal filesystem path
  model: haiku · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/workflow.rs`
  change: Replace the raw io-error message on a missing workflow definition ("Failed to read workflow \"<name>\" at /path/...: No such file or directory (os error 2)") with "Workflow '<name>' not found. Define it in .tm/workflows/<name>.toml".
  acceptance: `tm workflow show nonexistent` prints the plain sentence, with no filesystem path or "os error" text.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm workflow show nonexistent` -> `error: storage: Failed to read workflow "nonexistent" at /path/.../nonexistent.toml: No such file or directory (os error 2)`.

- [ ] **s1-mirror-status-and-push-clarity** — mirror status hides configured adapters; push/pull give no reason for a zero count; link doesn't warn on unset credentials
  model: sonnet · severity: high · builds Rust: yes · area: cli/mirror · deps: none
  files: `crates/tm-cli/src/ops.rs`
  change: `tm mirror status` reports only ticket-level external-id links, so after `tm mirror link github` it still says "No active mirror links" even though `mirror.toml` has a real, enabled adapter — add an adapters section to status output (JSON too) showing each configured adapter and its enabled state, distinct from ticket sync status. Make `mirror push`/`mirror pull`'s "0 pushed, 0 degraded" output name the reason (no mirrors configured vs no tickets eligible vs all already synced) instead of a bare count. In `mirror link`, check whether the env vars referenced by `--credential field=ENV_VAR` are actually set, and if not, print a non-fatal warning ("credential field \"owner\" references env var TM_GITHUB_OWNER which is not set") so the misconfiguration surfaces at link time, not first at push time.
  acceptance: After `tm mirror link github` with no tickets synced, `tm mirror status` shows the github adapter as configured. `tm mirror push` with nothing to do explains why. `tm mirror link github --credential owner=TM_GITHUB_OWNER` with TM_GITHUB_OWNER unset prints a warning naming the unset var, and still succeeds.
  test: `mise run test:crate -- tm-cli`
  evidence: after linking github, `tm mirror status` still returns `{"mirrors": []}` and "No active mirror links" despite `mirror.toml` containing `[adapters.github]` with `enabled=true`; `mirror push` prints "Mirror push completed: 0 pushed, 0 degraded" with no explanation; `mirror link` succeeds silently with unset `TM_GITHUB_OWNER`/`TM_GITHUB_REPO`, which only surfaces later as "Skipped adapter \"github\": ... is not set" on push.

- [ ] **s1-templates-io-error-message** — Missing template source file shows a raw OS error
  model: haiku · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-templates/src/manifest.rs`
  change: Change the `TmError::Io(format!("reading {}: {e}", manifest_path.display()))` at manifest.rs:63 to "template source not found: {} (manifest.toml)" — no OS error code, no "io:" prefix — matching the style of other not-found errors in this codebase.
  acceptance: `tm templates list` against a missing template directory shows "error: template source not found: /path/to/template (manifest.toml)", no OS error code.
  test: `mise run test:crate -- tm-templates`
  evidence: `tm templates list` with a missing template dir -> `error: io: reading /path/to/missing-template/manifest.toml: No such file or directory (os error 2)`.

- [ ] **s1-provider-project-quiet-flags** — provider and project commands ignore --quiet
  model: haiku · severity: high · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/project.rs`
  change: `provider_list`, `provider_detect`, `provider_status` and `provider_test` print their table unconditionally; wrap the table emission with `if !renderer.is_quiet()`, only suppressing `provider test`'s "ok" lines while keeping errors. `project_list()`/`project_show()` use `renderer.emit(&entries, "no global projects yet")`, which always prints the fallback narration regardless of `--quiet` — switch to the pattern used elsewhere (`renderer.emit(&json, "")`) so `--quiet` suppresses narration but not the payload, per render.rs's own documented contract (lines 5-8). While here, reword "no global projects yet" to explain what a global project is in plain terms ("no projects in $TM_HOME yet").
  acceptance: `tm provider list --quiet`, `tm provider detect --quiet`, `tm provider status --quiet`, `tm project list --quiet`, `tm project show --quiet` produce no output on success; `--json` output is unaffected; non-quiet behavior is unchanged.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm provider list --quiet` still prints the full table; `tm project list --quiet` still prints "no global projects yet"; `tm project show --quiet` still prints the full path.

- [ ] **s1-harness-missing-file-errors** — tm harness show/set/promote crash on a raw OS error when harness.toml doesn't exist
  model: haiku · severity: high · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/ops.rs`
  change: `harness_show` (~line 1130), `harness_set` (~1162) and `harness_promote` (~1325) all propagate "storage: Failed to read harness.toml: No such file or directory (os error 2)" instead of handling the missing-file case, unlike `config_cmd.rs::load_harness_config` (~line 200) which already checks `!path.is_file()` and returns `HarnessConfig::default()`. Make `harness_show` follow that same pattern (display defaults). Make `harness_set` use `HarnessConfig::default()` as its base when the file is missing, so it can bootstrap a fresh harness.toml with one key set. Make `harness_promote` give a clear message instead ("No harness configuration found. Use `tm harness set <key> <value>` to create one before promoting an epoch."), since promote has no sensible default to act on.
  acceptance: `tm harness show` on a fresh project prints default config in TOML, not an OS error. `tm harness set routing_weights.recency 0.5` on a fresh project succeeds and creates harness.toml. `tm harness promote 1 --force` on a fresh project explains it needs a harness config first.
  test: `mise run test:crate -- tm-cli`
  evidence: all three commands currently return `error: storage: Failed to read harness.toml: No such file or directory (os error 2)` on a freshly-initialized project.

- [ ] **s1-bench-compare-human-output** — tm bench compare's human output omits the actual comparison
  model: haiku · severity: high · builds Rust: yes · area: cli/bench · deps: none
  files: `crates/tm-cli/src/ops.rs`
  change: `bench_compare` (ops.rs:1672-1694) computes a `PromotionReport` with `aggregate_gain`, `candidate_improved` and per-task deltas, but the human-readable branch (~1688-1691) only prints epoch numbers ("Comparison: X vs Y"). Render whether the candidate improved (yes/no), the aggregate gain as a signed delta, and a brief per-task summary (or at least an improved/regressed count) when `task_deltas` is non-empty. The JSON output already has everything; just expose it in text.
  acceptance: `tm bench compare a.json b.json` shows candidate-improved status and the aggregate score delta in plain text, not just epoch numbers.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm bench compare report1.json report2.json` -> "Comparison: 0 vs 0" while the JSON shows `{"aggregate_gain": 0.0, "candidate_improved": false, "task_deltas": [["hello-world", 0.0]]}`.

- [ ] **s1-browser-toml-error-message** — Missing browser.toml error teaches TOML syntax instead of pointing at docs
  model: haiku · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/drive.rs`
  change: Replace the current message ("add a [managed] table with `version` and `sha256`, or a [remote_cdp] table with `ws_url`, and list the ones you configure in `fallback_order`") with something that names the file and points at documentation instead of teaching TOML inline: "Create a browser.toml file with a provider configuration. See SPEC.md §19.1a for examples (managed: pinned Chrome download, remote-cdp: existing browser endpoint)."
  acceptance: `tm browser open <url>` without browser.toml names the file to create and points to SPEC.md §19.1a, without using `[managed]`/`fallback_order`-style TOML syntax in the error text itself.
  test: manual: `S=$(mktemp -d); export TM_HOME=$(mktemp -d) TM_TEST_MOCK_PROVIDER=1; cd $S && git init -q && git config user.email t@t && git config user.name t && echo x>r && git add r && git commit -qm x && tm init -q && tm browser open https://example.com`
  evidence: current error: "not found: browser.toml /path/to/browser.toml; add a [managed] table with `version` and `sha256`, or a [remote_cdp] table with `ws_url`, and list the ones you configure in `fallback_order`".

- [ ] **s1-mcp-exit-code-on-parse-error** — tm mcp exits 0 on a JSON-RPC parse error / EOF
  model: haiku · severity: high · builds Rust: yes · area: cli/mcp · deps: none
  files: `crates/tm-mcp/src/main.rs`
  change: When `tm mcp` hits EOF or invalid JSON on stdin before handling any successful JSON-RPC call, it prints an error but exits 0, breaking script error handling. Exit 1 in that case.
  acceptance: `echo '' | tm mcp` and `echo 'invalid' | tm mcp` both exit non-zero.
  test: `echo '' | tm mcp; test $? -ne 0`
  evidence: `echo '' | tm mcp` prints "error: parse: EOF while parsing a value at line 1 column 0" and exits 0.

- [ ] **s1-attach-output-copy-and-dedup** — tm attach output uses unexplained jargon and double-lists case-variant filenames
  model: haiku · severity: medium · builds Rust: yes · area: cli/attach · deps: none
  files: `crates/tm-cli/src/ops.rs`
  change: Replace "indexed 1 files, 1 chunks, 1 commits ingested (symbol graph built: true)" with plain wording, e.g. "Indexed 1 file with 1 commit. Code navigation is ready." Separately, the attach doc-listing table shows both `README.md` and `readme.md` as distinct rows on a case-insensitive filesystem; dedupe by case-insensitive path before printing.
  acceptance: `tm attach .` output has no "chunks"/"symbol graph" jargon. On a project with README.md, the doc table lists it once.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm attach .` -> "indexed 1 files, 1 chunks, 1 commits ingested (symbol graph built: true)"; doc table shows both "README.md  Readme" and "readme.md  Readme" for the same file.

- [ ] **s1-io-error-wrapping-and-empty-objective** — Wrap low-level IO/git errors for users; reject empty ticket objectives
  model: sonnet · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/main.rs`, `crates/tm-core/src/history.rs`, `crates/tm-cli/src/tickets.rs`
  change: Wrap the bare IO error path in main.rs so e.g. `tm attach /nonexistent/path` says "The path /nonexistent/path does not exist. Check the path and try again." instead of "error: io: No such file or directory (os error 2)". Wrap git2 NotFound errors at the `tm history why <path>` call site as "File does not exist in repository history."; other git2 errors become "Unable to read file history — repository may be corrupted." (log the raw error via tracing for debugging). Separately, in `ticket_new` (tickets.rs ~392), reject an empty or whitespace-only `--objective`/positional objective with "Objective cannot be empty — describe what the ticket should accomplish." instead of silently creating a blank-objective ticket.
  acceptance: `tm attach /nonexistent/path` gives the plain-English path message. `tm history why nonexistent.txt` gives "File does not exist in repository history.", no git2 class/code numbers. `tm ticket new ''` and `tm ticket new '   '` both fail with the objective error; `tm ticket new 'Valid'` still succeeds.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm attach /nonexistent/path` -> "error: io: No such file or directory (os error 2)"; `tm history why nonexistent.txt` -> "error: storage: git2: the path ... does not exist in the given tree; class=Tree (14); code=NotFound (-3)"; `tm ticket new ''` creates a ticket with an empty objective column.

## Owner asks (2026-09-23)

- Actual ticket flow works end to end: request → implementation → verification → finished, from the chat, the tickets screen, `tm serve` and `tm mcp`.
- Genesis works and makes real projects.
- Remove jank and machine-speak from every user-facing string; plain, friendly, specific wording.
- Configurable, with good options: settings that matter are discoverable and have sane defaults.
- Expand the parts that are thin; find and fix bugs, broken UX, confused concepts, stubbed or unintegrated features.
- Project management: milestones, timelines, dependencies, a calendar/timeline TUI view.

## B1 Wiring: code-index freshness + symbol self-reference; release packaging; TS SDK + vscode bundling

Gate: `mise run verify && pnpm -C clients/ts install && pnpm -C clients/ts test && pnpm -C clients/ts build && pnpm -C clients/vscode install && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test && mise x aqua:rhysd/actionlint@latest -- actionlint .github/workflows/release.yml`

- [ ] **nav-fix-project-codeintel-freshness** — Refresh the code index in Project::code_intel() and in the dispatch context-pack source
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-cli/src/project.rs`, `crates/tm-cli/src/dispatch.rs`
  change: Project::code_intel() (project.rs:79-81) only calls CodeIntel::open_at and never calls update_incremental. ProjectContextPackSource::compile (dispatch.rs:28-52, used by tm run/tm sched run) opens its own CodeIntel and does not refresh either. Change: (1) in code_intel(), call update_incremental(self.clock.as_ref()) after open_at. (2) Add a clock: Arc<dyn Clock> field to ProjectContextPackSource, pass project.clock.clone() at its construction in build_dispatcher, and call update_incremental in compile() before building the pack. Also refresh the long-lived Arc<CodeIntel> built at dispatch.rs:267 once when it is constructed. Degrade, never fail: update_incremental's history ingest needs a git HEAD, so on Err log tracing::warn! and return the opened index. If the root is not inside a git work tree, skip the refresh entirely, so a global-scope project in an arbitrary directory never walks something like $HOME. Leave tm doctor's own explicit update_incremental call (project.rs:1920) as it is. This task absorbs nav-fix-dispatch-context-pack-freshness.
  acceptance: New tests: (a) a git tempdir with one commit containing a Rust fn, opened without tm doctor, resolves `symbol def` for that fn; (b) ProjectContextPackSource::compile over the same never-doctored project includes that file's outline or retrieval content; (c) code_intel() in a non-git tempdir and in a git repo with zero commits returns Ok and logs a warning. The handoff reports the measured no-op refresh time on this repo's tree. If it exceeds about 500ms, say so explicitly so a follow-up can gate the refresh on mtime.
  test: `mise run test:crate -- tm-cli`

- [ ] **nav-fix-codeintel-self-reference** — Stop treating a definition's own name token as a reference to itself
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-codeintel/src/symbols.rs`
  change: In SymbolIndex::parse_file (around symbols.rs:399-421), after collecting name_ranges for this file's symbols, drop any captured reference whose (path, range) exactly equals a definition's own name range before pushing it onto self.references. Tighten rename_preview_edits_definition_and_confident_references (around symbols.rs:930-947) from `edits.len() >= 2` to `== 2`. Add a regression test that idx.callers(id) does not include the symbol's own definition when the function never calls itself.
  acceptance: In the two-function case (fn helper; fn main calls helper), refs(helper) returns exactly one hit, the call site, and callers(helper) is exactly [main]. The rename_preview test passes with == 2, and the new self-caller test passes.
  test: `mise run test:crate -- tm-codeintel`

- [~] **release-build-artifact-and-install-guide** — Release workflow producing tm binaries, a mise install task, and README install guidance
  superseded on main by the parallel session: `.github/workflows/release.yml`, README install paths and release v0.1.0 exist. Remaining gap tracked as `release-web-assets` in section T.
  model: sonnet · size: S · builds Rust: no · area: release/packaging (user request) · deps: none
  files: `.github/workflows/release.yml`, `README.md`, `mise.toml`, `CLAUDE.md`
  change: (1) Add .github/workflows/release.yml, triggered on `v*` tag pushes plus workflow_dispatch, with permissions contents: write. Use a matrix of macos-14 (aarch64-apple-darwin) and ubuntu-latest (x86_64-unknown-linux-gnu). Steps: checkout, dtolnay/rust-toolchain honoring rust-toolchain.toml, Swatinem/rust-cache, `cargo build --release --locked -p tm-cli`, package tm-<tag>-<target>.tar.gz containing the `tm` binary plus a SHA256SUMS entry, then upload with `gh release create/upload` using GITHUB_TOKEN. Do not run the test suite here; ci.yml already does. (2) Add a mise `install` task: `cargo install --path crates/tm-cli --locked -j 2`, keeping the repo's -j 2 cap. (3) Write a root README.md; none exists today. Include a one-paragraph description pointing to SPEC.md, docs/ and CLAUDE.md, then three install paths for a PRIVATE repo, where plain curl will not work: (a) `gh release download vX --repo alliecatowo/ticket-master --pattern '*aarch64-apple-darwin*'`, verify with shasum -a 256 -c, extract to ~/.local/bin, and note the binary is unsigned so macOS needs `xattr -d com.apple.quarantine`; (b) `cargo install --git ssh://git@github.com/alliecatowo/ticket-master.git tm-cli --locked`; (c) from source with `mise install && mise run install`. Add a quickstart (`tm doctor`, `tm`, `tm ticket new`, `tm serve`, noting that the web client needs a pnpm build) and provider credential setup that links docs/providers.md and D-005 rather than duplicating them. (4) Add CLAUDE.md lines for `mise run install` and for the release process (tag push triggers release.yml).
  acceptance: actionlint passes on release.yml, `mise tasks` lists install, and the README gives all three install paths plus the quarantine note. No Rust code changes.
  test: `mise x aqua:rhysd/actionlint@latest -- actionlint .github/workflows/release.yml && mise tasks | grep -qw install`

- [ ] **ts-sdk-add-accept-reject-retry** — Add accept/reject/retry variants to clients/ts TransitionCommand
  model: haiku · size: XS · builds Rust: no · area: clients/ts · deps: none
  files: `clients/ts/src/domain.ts`, `clients/ts/test/client.test.ts`
  change: Add three variants to the TransitionCommand union (domain.ts:460-469) matching tm-server's TransitionRequest::Accept/Reject/Retry (routes.rs:388-404): `{ accept: { note?: string | null; actor: ParticipantId } }`, `{ reject: { reason: string; actor: ParticipantId } }`, `{ retry: { guidance?: string | null; actor: ParticipantId } }`.
  acceptance: New client.test.ts cases call transition() with each of the three variants and assert the exact JSON body posted to the mock fetch. pnpm test and build pass.
  test: `pnpm -C clients/ts install && pnpm -C clients/ts test && pnpm -C clients/ts build`

- [ ] **vscode-bundle-and-link-shared-sdk** — Bundle the VS Code extension with esbuild and link @ticketmaster/client
  model: sonnet · size: S · builds Rust: no · area: clients/vscode · deps: none
  files: `clients/vscode/package.json`, `clients/vscode/pnpm-lock.yaml`, `clients/vscode/tsconfig.json`, `clients/vscode/esbuild.mjs`
  change: @ticketmaster/client is ESM-only ("type":"module", exports with import only), while the extension compiles to CommonJS (tsconfig module commonjs, main ./out/extension.js). A plain dependency therefore cannot be require()d on VS Code 1.85's Node. Add "@ticketmaster/client": "link:../ts". Each client has its own pnpm-workspace.yaml, so workspace:* will not resolve. Add an esbuild devDependency and a small esbuild.mjs that bundles src/extension.ts to out/extension.js with format cjs, platform node and `vscode` external. Change the compile script to `tsc --noEmit -p ./ && node esbuild.mjs`. Adjust tsconfig (for example module esnext + moduleResolution bundler, with noEmit) so imports from @ticketmaster/client typecheck. Make no source changes in this task.
  acceptance: After building clients/ts, `pnpm -C clients/vscode install && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test` pass. out/extension.js is a single CJS bundle that still contains require("vscode") and does not contain require("@ticketmaster/client").
  test: `pnpm -C clients/ts install && pnpm -C clients/ts build && pnpm -C clients/vscode install && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test`

## B2 Wiring: in-run index refresh, real events replay; vscode on the shared SDK; macOS transitions; ship v0.1.0

Gate: `mise run verify && pnpm -C clients/ts build && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test && swift test --package-path clients/macos && gh release view v0.1.0`

- [ ] **nav-agent-tool-index-refresh** — Refresh the index inside the agent tool path after mutating tools
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: nav-fix-project-codeintel-freshness
  files: `crates/tm-agent/src/tools.rs`
  change: The agent tool set holds one Arc<CodeIntel> for a whole session or dispatcher lifetime (tools.rs:793; built once at tm-cli dispatch.rs:267 and once per chat session). symbol_index() and search read the files table, which only update_incremental populates. So a file the agent creates mid-run never shows up in search.*/symbol.* results, and chunks for edited files stay stale. Add a dirty flag, set whenever a mutating tool completes (file write, edit, patch apply, shell exec). Before executing any search.*, symbol.* or history.* tool, if the flag is set, call ci.update_incremental(clock) and clear it. On error, warn and continue, the same policy as nav-fix-project-codeintel-freshness. Use the clock the tool set already has, or add one to its constructor.
  acceptance: New tools.rs test in a git tempdir with one commit: a write tool creates new.rs containing `fn fresh_fn`, then a symbol-definition tool call finds fresh_fn (this fails today). Two consecutive search calls with no write between them trigger no refresh, asserted through a counter or a no-op delta.
  test: `mise run test:crate -- tm-agent`

- [ ] **replay-fix-events-replay-stub** — Make `tm events replay` actually replay through materialize::replay
  model: sonnet · size: S · builds Rust: yes · area: replay · deps: none
  files: `crates/tm-core/src/store.rs`, `crates/tm-cli/src/ops.rs`
  change: Add a public `Store::view_as_of(&self, seq: u64) -> tm_types::Result<ProjectView>`. It generalizes the private ticket_and_goal_as_of scratch-replay pattern (store.rs:2639): tempfile EventLog, create_views, materialize::replay, read_view. Rewrite events_replay (ops.rs:2260-2289), whose doc comment currently misdescribes it, to call view_as_of(to) and render ticket counts and states in human mode, or the full view in --json. The live store is never mutated.
  acceptance: A tm-core unit test exercises view_as_of: create two tickets, transition one, and replay to a seq before the transition shows the old state. The tm-cli test events_replay_empty_range still passes, and a new CLI-level test asserts the rendered states rather than an event count.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`

- [ ] **vscode-migrate-to-shared-ts-sdk** — Move clients/vscode onto @ticketmaster/client, fixing every wire-shape bug, and delete the stand-in client
  model: sonnet · size: M · builds Rust: no · area: clients/vscode · deps: vscode-bundle-and-link-shared-sdk, ts-sdk-add-accept-reject-retry
  files: `clients/vscode/src/extension.ts`, `clients/vscode/src/commands/commands.ts`, `clients/vscode/src/commands/commands.test.ts`, `clients/vscode/src/ticketmaster/client.ts`, `clients/vscode/src/ticketmaster/types.ts`, `clients/vscode/src/tree/ticketTreeProvider.ts`, `clients/vscode/src/tree/treeModel.ts`, `clients/vscode/src/tree/treeModel.test.ts`, `clients/vscode/src/leases/leaseDecorationProvider.ts`, `clients/vscode/src/leases/leaseDecorations.ts`, `clients/vscode/src/leases/leaseDecorations.test.ts`, `clients/vscode/src/codelens/decisionCodeLensProvider.ts`, `clients/vscode/src/codelens/decisionCodeLens.ts`, `clients/vscode/src/codelens/decisionCodeLens.test.ts`
  change: Replace every import of ./ticketmaster/client and ./ticketmaster/types with TicketmasterClient and the domain types from @ticketmaster/client, then delete the two stand-in files. The SDK's correct types fix these by construction: lowercase snake_case TicketState/TicketKind/Milestone state literals in treeModel's BADGES and in ticketTreeProvider's "closed" check; the field names closed_by, ttl_seconds, affected_tickets, affected_paths (not affectedDocs) and superseded_by; decisionCodeLens filtering on affected_paths. Fix the mutation commands. claimTicket calls acquireLease(id, {holder, ttl_seconds: 600, actor: holder}). submitWithEvidence first calls attachEvidence (EvidenceKind is snake_case on the wire, e.g. human_attestation), then transition(id, {submit: {summary, evidence: [artifactIds], actor}}). recordDecision uses the SDK's createDecision shape. Update the test fixtures. Absorbs vscode-fix-ticket-enum-casing, vscode-fix-field-name-casing, vscode-fix-lease-request-body and vscode-fix-submit-transition-shape.
  acceptance: src/ticketmaster/ no longer exists and compile (tsc --noEmit plus the esbuild bundle) passes. Tests assert: lowercase state literals in the tree model; a decision with affected_paths ['src/auth/**'] produces a CodeLens for src/auth/x.ts; the claimTicket lease body has ttl_seconds and actor (no ttlSeconds); submit posts {submit:{summary,evidence,actor}} with no top-level trigger. A grep for PascalCase state/kind literals under src/ returns nothing outside comments.
  test: `pnpm -C clients/ts build && pnpm -C clients/vscode install && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test`

- [ ] **macos-extend-transition-methods** — Add submit/accept/reject to TicketmasterKit's APIClient
  model: sonnet · size: S · builds Rust: no · area: clients/macos · deps: none
  files: `clients/macos/Sources/TicketmasterKit/APIClient.swift`, `clients/macos/Tests/TicketmasterKitTests/ModelDecodingTests.swift`
  change: Add submitTicket(id:summary:evidence:actor:), acceptTicket(id:note:actor:) and rejectTicket(id:reason:actor:) to the TicketmasterAPI protocol and to APIClient. Each POSTs an externally tagged transition body ({"submit": {...}}, {"accept": {...}}, {"reject": {...}}) whose field names match tm-server routes.rs:353-404 exactly, following the activateTicket pattern (APIClient.swift:95-98).
  acceptance: New unit tests assert that the encoded JSON body for each new method matches the server's shape, and swift test passes.
  test: `swift test --package-path clients/macos`

- [~] **release-cut-v0-1-0** — Push main and cut v0.1.0 with release artifacts
  superseded: release v0.1.0 was published on 2026-09-23 from the parallel session.
  model: haiku · size: XS · builds Rust: no · area: release/packaging (user request) · deps: release-build-artifact-and-install-guide
  files: 
  change: Run only after B1's gate passes on main. Do not commit the untracked HELLO.md or HELLO_VIOLET.md; wait for the owner's answer. Then: `git push origin main`, `git tag -a v0.1.0 -m 'tm v0.1.0'`, `git push origin v0.1.0`, and `gh run watch` the Release workflow run. Confirm with `gh release view v0.1.0` that both target tarballs and SHA256SUMS are attached. Then follow README install path (a) in a fresh `mktemp -d` directory, never the primary checkout: download, verify the checksum, extract and run `./tm --version`. Make no source edits. If the workflow fails, report the failing step's log instead of patching around it.
  acceptance: `gh release view v0.1.0` lists the aarch64-apple-darwin and x86_64-unknown-linux-gnu tarballs plus SHA256SUMS, and the downloaded macOS binary prints its version.
  test: `gh release view v0.1.0 --json assets --jq '.assets[].name'`

## B3 Genesis termination ([new decision: genesis termination]); symbol/history CLI output fixes

Gate: `mise run verify`

- [ ] **genesis-stop-infinite-maturity-loop** — Stop tm genesis from looping forever on a failing maturity gate ([new decision: genesis termination])
  model: opus · size: M · builds Rust: yes · area: genesis · deps: none
  files: `crates/tm-cli/src/project.rs`, `crates/tm-genesis/src/stages.rs`, `docs/decisions/D-NNN-genesis-cli-stops-for-work.md`, `CLAUDE.md`
  change: The loop is verified: compile.rs:469 commits tickets as Draft, and nothing in genesis() (project.rs:1297-1315) activates them, so V1 never closes. A failed MaturityGate then goes back to Stabilization, which re-enters MaturityGate unconditionally (stages.rs:183-190), and every pass makes a real judge_maturity provider call. Recommended design is exit-and-resume. Extract the CLI loop into a testable function with an explicit stop policy. Stop after Ignition has committed the Draft graph, or at the latest on the first failed MaturityGate, and also when the V0 or V1 stage cannot advance because its milestone is not closed; never spin. On stopping, print or emit the status: N tickets committed under milestone <id>, run `tm sched run` (or `tm run <T>`) to work them, then re-run genesis to resume. Exit 0. Write [new decision: genesis termination], following the D-002 template, with the three alternatives and why exit-and-resume won. Add a CLAUDE.md line on tm genesis behavior.
  acceptance: A test starts the extracted loop from a GenesisState already at Stabilization, with a MockProvider whose default response is a mature=false judgment. It asserts the loop returns Ok within a small bounded number of advances, makes at most one maturity provider call, and reports next steps. A second test asserts that a V0 stage with an unclosed milestone stops rather than spinning. mise run hygiene resolves [new decision: genesis termination].
  test: `mise run test:crate -- tm-cli && mise run test:crate -- tm-genesis && mise run hygiene`

- [ ] **nav-fix-cli-symbol-output-bugs** — Fix tm symbol refs/callers/callees output, the double parse, and `tm history why <path>` defaulting to line 1
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-cli/src/search.rs`
  change: (1) symbol_refs (search.rs:~411) and symbol_callers (~455) put an absolute byte offset in ReferenceInfo.col. Compute a real line-relative column, or drop the field. (2) symbol_callers throws away the caller's name and kind; build SymbolInfo the way symbol_callees does (~480-524). (3) In refs/callers/callees, call code_intel.symbol_index() exactly once and resolve the symbol from that index, instead of also calling CodeIntel::resolve_symbol, which builds a second full tree-sitter parse. (4) In history_why (~583-591), when the locator has no :line, use the file's full line range (1..=line_count). Fall back to line 1 only if the file cannot be read, and say so in the output. Absorbs nav-fix-history-why-whole-file-default.
  acceptance: Tests: callers --json includes each caller's name and kind; no field labeled col holds a byte offset; `history why <path>` with no line surfaces a commit that touched only a later line. The handoff includes a timing note showing callers no longer costs about twice as much as def.
  test: `mise run test:crate -- tm-cli`

## B4 Ordered MockProvider sequence; real `tm events tail`

Gate: `mise run verify`

- [ ] **genesis-provider-mock-ordered-sequence** — Add a FIFO ordered-response mode to MockProvider
  model: haiku · size: S · builds Rust: yes · area: provider (serves genesis and replay) · deps: none
  files: `crates/tm-provider/src/mock.rs`
  change: Add script_sequence(Vec<Completion>). It answers successive complete() calls in order regardless of request content; once exhausted, it falls through to the existing hash-match, then default, then Unscripted behavior. Follow the existing Mutex<Script> pattern. Also expose sequence_remaining(), and keep recording each served request in the call log so a replay caller can compare request hashes. The later replay-cassette-types task relies on this.
  acceptance: A unit test scripts three distinct completions, issues three non-matching requests, and gets them back in order. A fourth call falls through to the default or Unscripted behavior.
  test: `mise run test:crate -- tm-provider`

- [ ] **tel-events-kind-ticket-filter** — Implement tm events tail for real, with --kind/--ticket/--no-follow
  model: sonnet · size: S · builds Rust: yes · area: telemetry · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`
  change: events_tail (ops.rs:2199-2211) is a stub: it computes `from`, prints a note and returns. Implement what its doc comment describes. Read events from `from` to the head with EventLog::read_from, in pages. Then, unless --no-follow is given, subscribe (EventLog::subscribe) and stream until ctrl-c, emitting JSON Lines in --json mode. Add --kind (parse with EventKind::from_str and reject unknown kinds with a clear error), --ticket and --no-follow to EventsTailArgs (args.rs:1015). Filter through a pure `fn event_matches(&Event, &EventFilter) -> bool`. The filter part absorbs the original tel-events-kind-ticket-filter.
  acceptance: Unit tests cover event_matches for kind, ticket and combined filters. A --no-follow test over a scratch store with mixed events returns only the matching events and exits. `--kind not.a.real.kind` returns an error.
  test: `mise run test:crate -- tm-cli`

## B5 Genesis offline fixtures; history blame fallback

Gate: `mise run verify`

- [ ] **genesis-shared-offline-fixtures** — Extract reusable, schema-valid canned JSON for each Genesis provider stage
  model: haiku · size: S · builds Rust: yes · area: genesis · deps: none
  files: `crates/tm-genesis/src/fixtures.rs`, `crates/tm-genesis/src/lib.rs`
  change: Add an always-compiled (not cfg(test)) module in tm-genesis exposing the minimal canned JSON that each provider-calling stage needs (seed, vision, spec, graph compilation, maturity), taken from what crates/tm-e2e/tests/genesis_e2e.rs writes by hand (from line 82 on). Also add `fn offline_sequence() -> Vec<String>` in stage call order, so a CLI mock can feed MockProvider::script_sequence. Leave genesis_e2e.rs as it is.
  acceptance: A tm-genesis unit test deserializes each fixture into the exact type that stage's parser expects and asserts success.
  test: `mise run test:crate -- tm-genesis`

- [ ] **nav-fix-history-commits-table-fallback** — Don't silently drop blamed commits that are missing from the commits table
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-codeintel/src/history.rs`
  change: In HistoryIndex::why (history.rs:214-256), when a blamed sha has no row in the commits table, build its CommitSummary from a live git2 find_commit instead of skipping it.
  acceptance: New test: a commit made after the last ingest_incremental still appears in why()'s results with its sha, author and message.
  test: `mise run test:crate -- tm-codeintel`

## B6 tm genesis honors TM_TEST_MOCK_PROVIDER; docs state persistence

Gate: `mise run verify`

- [ ] **genesis-cli-wire-mock-provider** — Honor TM_TEST_MOCK_PROVIDER in resolve_genesis_provider
  model: sonnet · size: S · builds Rust: yes · area: genesis · deps: genesis-provider-mock-ordered-sequence, genesis-shared-offline-fixtures, genesis-stop-infinite-maturity-loop
  files: `crates/tm-cli/src/project.rs`, `crates/tm-cli/src/agent.rs`, `CLAUDE.md`
  change: At the top of resolve_genesis_provider (project.rs:1174, before the local-backend probe), check TEST_MOCK_PROVIDER_ENV (agent.rs:1553) and, when it is set, return a MockProvider scripted through script_sequence with tm_genesis::fixtures::offline_sequence(). Put the check in a function that takes the env value as a parameter, so tests never mutate process-global env. Update agent.rs's TEST_MOCK_PROVIDER_ENV doc comment (around 1550-1560), which claims nothing outside agent.rs reads the variable. Update the CLAUDE.md note on offline testing.
  acceptance: A unit test shows the mock branch returns a provider that serves the fixture sequence. Running tm genesis in a scratch tempdir with TM_TEST_MOCK_PROVIDER=1 and no credentials no longer errors on the missing key, and it stops cleanly per [new decision: genesis termination].
  test: `mise run test:crate -- tm-cli`

- [ ] **docs-persist-state-across-invocations** — Persist DocState (loaded state + Reconciling) and mark Review tickets human_required
  model: sonnet · size: M · builds Rust: yes · area: docs (SPEC §9) · deps: none
  files: `crates/tm-cli/src/ops.rs`, `crates/tm-docs/src/registry.rs`, `crates/tm-core/src/store.rs`, `crates/tm-cli/src/tickets.rs`
  change: (1) In load_and_sync_doc_registry (ops.rs:50), give each already-registered doc its persisted state and last_verified from project.store, extending Store's doc read path if needed. Only newly discovered docs get DocRecord::new's Unverified default (registry.rs:219-228). (2) In docs_reconcile (ops.rs:187-238), after opening each reconciliation ticket, persist the Reconciling transition through a new Store method that writes both an event and the materialized docs row. Today only the function-local registry is mutated (ops.rs:229-231). (3) For ReconciliationKind::Review (Maintained or Human docs), build ExecutorRequirements with human_required: true; Regeneration keeps default_executor_requirements(). Absorbs docs-persist-reconciling-transition and docs-differentiate-review-vs-regeneration-tickets.
  acceptance: Tests: a doc whose state is persisted as Stale still reads Stale in a fresh docs_list or docs_check call. After docs_reconcile, a fresh docs list shows Reconciling. With one Generated and one Maintained doc both stale, reconcile produces tickets with human_required false and true respectively. The existing docs_check and docs_reconcile tests pass.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`

## B7 tm genesis offline e2e test; mirror push idempotency

Gate: `mise run verify`

- [ ] **genesis-cli-offline-integration-test** — Real-binary subprocess test: tm genesis runs offline end to end
  model: sonnet · size: M · builds Rust: yes · area: genesis · deps: genesis-stop-infinite-maturity-loop, genesis-cli-wire-mock-provider
  files: `crates/tm-cli/tests/genesis_offline.rs`
  change: Follow crates/tm-cli/tests/promotion.rs's pattern: Command::new(env!("CARGO_BIN_EXE_tm")) in a git-initialized tempdir, with TM_HOME set to a tempdir, TM_NOTIFY=0 and TM_TEST_MOCK_PROVIDER=1. Run `tm genesis --prompt ...` with a wall-clock timeout guard. Assert exit 0, that stdout reports the committed tickets and the next steps, and that `tm tickets --json --all` shows the committed Draft tickets.
  acceptance: The test passes deterministically in a few seconds, makes no network calls and cannot hang, because the timeout guard fails the test instead of hanging.
  test: `mise run test:crate -- tm-cli`

- [ ] **mirror-persist-content-hash-for-idempotency** — Add content_hash to mirror_links so push idempotency survives restarts
  model: sonnet · size: M · builds Rust: yes · area: mirror (SPEC §28) · deps: none
  files: `crates/tm-core/src/schema.rs`, `crates/tm-core/src/store.rs`, `crates/tm-cli/src/ops.rs`
  change: Add a content_hash TEXT column to mirror_links (schema.rs:271-277), with a schema-version bump that follows the file's existing migration convention. Thread the column through MirrorLinkRow's read and write helpers in store.rs. Change mirror_push (ops.rs:1870-1893) to look up the existing row and pass Some(MirrorLink{..., content_hash}) to SyncEngine::push instead of None. Update the doc comment that explains why it was None.
  acceptance: A test runs mirror push twice over an unchanged ticket set, and the second run makes zero calls to the recording Tracker adapter. A migration test opens a pre-bump database and succeeds.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`

## B8 tm genesis resume; MCP navigation toolset

Gate: `mise run verify`

- [ ] **genesis-cli-resume-flag** — Let tm genesis resume an in-progress run instead of always starting fresh
  model: sonnet · size: S · builds Rust: yes · area: genesis · deps: genesis-stop-infinite-maturity-loop, genesis-cli-offline-integration-test
  files: `crates/tm-cli/src/project.rs`, `crates/tm-cli/src/args.rs`, `crates/tm-cli/tests/genesis_offline.rs`, `CLAUDE.md`
  change: genesis() always calls GenesisState::new (project.rs:1292). Add --resume to GenesisArgs (args.rs:266), and auto-detect a persisted snapshot when --prompt is omitted. Resume through the existing GenesisDriver::resume (stages.rs:520-550) and continue from the persisted stage. Update the next-steps message from [new decision: genesis termination] and CLAUDE.md to name the real flag.
  acceptance: Extend genesis_offline.rs: run genesis until it stops, then run it again with --resume. Exactly one GraphCompilation artifact exists afterward (no duplicate graph), and the run continues from the persisted stage.
  test: `mise run test:crate -- tm-cli`

- [ ] **nav-expand-mcp-navigation-toolset** — Expose the remaining tm-codeintel read tools over tm-mcp, with a fresh index
  model: sonnet · size: M · builds Rust: yes · area: code-navigation / MCP · deps: nav-fix-project-codeintel-freshness, nav-fix-codeintel-self-reference
  files: `crates/tm-mcp/src/server.rs`
  change: tm mcp and the underscore tool-name scheme have already landed (the tools are ticket_list, search_exact, symbol_def and so on). Add definitions and handlers for search_regex, search_semantic, symbol_references, symbol_callers, symbol_callees, history_why, history_search and history_deleted, following the existing tool_definitions() and execute_tool (server.rs:425-433) pattern. Leave out rename_preview, since it is write-shaped. McpServer::code_intel() (server.rs:251) also never refreshes: call update_incremental with warn-and-continue, the same policy as nav-fix-project-codeintel-freshness.
  acceptance: tool_definitions_names_match_the_dispatch_table and every_tool_name_is_a_valid_host_tool_name still pass. Each new tool has a test over a small fixture project. symbol_def followed by symbol_references and symbol_callers round-trips a real symbol_id in one server session.
  test: `mise run test:crate -- tm-mcp`

## B9 Server schema + TS snapshot; real dollar cost in usage.recorded

Gate: `mise run verify && pnpm -C clients/ts run gen && git diff --exit-code clients/ts/src/generated.ts clients/ts/schema/snapshot.json && pnpm -C clients/ts test && pnpm -C clients/ts build`

- [ ] **ts-sdk-schema-snapshot-drift-note** — Document accept/reject/retry in GET /schema and regenerate the TS snapshot
  model: haiku · size: XS · builds Rust: yes · area: tm-server / clients/ts · deps: ts-sdk-add-accept-reject-retry
  files: `crates/tm-server/src/routes.rs`, `clients/ts/schema/snapshot.json`, `clients/ts/src/generated.ts`
  change: get_schema's TransitionRequest description (routes.rs:595) still reads 'activate|trigger|submit|verify|audit|close|cancel|reopen|fail'; add accept, reject and retry. First check whether the server-fixes track has already landed this, and if so skip the routes.rs edit. Update schema/snapshot.json to match, then run `pnpm -C clients/ts run gen`, which falls back to the snapshot when no server is running, to regenerate src/generated.ts.
  acceptance: The tm-server tests pass, and a second `pnpm gen` produces no diff in generated.ts or snapshot.json.
  test: `mise run test:crate -- tm-server && pnpm -C clients/ts run gen && git diff --exit-code clients/ts/src/generated.ts`

- [ ] **tel-completion-cost-field** — Put the real priced cost into usage.recorded (no new Completion field)
  model: sonnet · size: S · builds Rust: yes · area: telemetry · deps: none
  files: `crates/tm-provider/src/fabric.rs`, `crates/tm-agent/src/agent_loop.rs`
  change: Fabric already computes cost_micros (fabric.rs:358-374) and then drops it, and agent_loop hardcodes dollars_micros: 0 (agent_loop.rs:~812). Do not add a field to Completion: it has about 40 struct-literal sites across tm-provider, tm-agent, tm-cli, tm-genesis and tm-e2e. Instead: extract `pub fn cost_micros(price: &Price, usage: &Usage) -> u64`; add `Fabric::execute_priced(role, req) -> Result<(Completion, Option<u64>)>` and have execute() delegate to it; switch agent_loop.rs:785 to execute_priced and set step_spend.dollars_micros from its result (0 when unpriced). Do not cite [new decision: telemetry and cost attribution] yet; it is created in B10. Absorbs tel-wire-real-cost-usage-recorded.
  acceptance: Fabric tests: a priced candidate yields Some(expected), an unpriced one None. Update run_records_usage_recorded_with_the_completions_actual_spend (agent_loop.rs:~1877) to use a priced mock candidate and assert a nonzero, correct dollars_micros.
  test: `mise run test:crate -- tm-provider && mise run test:crate -- tm-agent`

## B10 Cassette format + RecordingProvider ([new decision: record/replay cassettes]); usage attribution ([new decision: telemetry and cost attribution])

Gate: `mise run verify`

- [ ] **replay-cassette-types** — Cassette format, RecordingProvider and ordered cassette replay in tm-provider ([new decision: record/replay cassettes])
  model: sonnet · size: M · builds Rust: yes · area: replay · deps: genesis-provider-mock-ordered-sequence
  files: `crates/tm-provider/src/cassette.rs`, `crates/tm-provider/src/lib.rs`, `crates/tm-provider/src/mock.rs`, `docs/decisions/D-NNN-record-replay-harness.md`
  change: New cassette module, JSONL format. The first line is a header {format_version, harness_epoch: Option<u64>, recorded_at}; one entry per call follows: {seq, role, provider_id, request_hash, request, completion}. request_hash is computed over a normalized request in which the project-root or tempdir path prefix is replaced by a placeholder, so identical starting state in a different tempdir hashes the same. Provide Cassette::read_jsonl and write_jsonl. RecordingProvider<P: Provider> appends an entry on each successful complete(); embed() delegates and is not recorded. Add MockProvider::script_from_cassette, which loads entries into the ordered sequence API from genesis-provider-mock-ordered-sequence. It compares each served request's normalized hash with the recorded one and records mismatches, readable through divergences(), instead of failing. Replay is ordered because exact-hash replay into a fresh project diverges on the first call whenever prompts carry paths, timestamps or ids. Write [new decision: record/replay cassettes] (D-002 template) covering the format, ordered replay with divergence reporting, and path normalization; later tasks amend it. Absorbs replay-recording-provider-wrapper and replay-harness-epoch-metadata.
  acceptance: Round-trip test: write a header plus three entries, read them back and get equal values. RecordingProvider over a scripted MockProvider writes two lines that read back matching. Replay serves entries in order, reports exactly one divergence when one recorded request is mutated, and returns Unscripted once exhausted. Two requests that differ only in the root path hash equal. Hygiene resolves [new decision: record/replay cassettes].
  test: `mise run test:crate -- tm-provider && mise run hygiene`

- [ ] **tel-usage-payload-model-field** — Attribute usage.recorded to provider/model; create [new decision: telemetry and cost attribution]
  model: sonnet · size: M · builds Rust: yes · area: telemetry · deps: tel-completion-cost-field
  files: `crates/tm-events/src/payload.rs`, `crates/tm-core/src/store.rs`, `crates/tm-core/src/materialize.rs`, `crates/tm-agent/src/agent_loop.rs`, `docs/decisions/D-NNN-local-telemetry.md`
  change: Add provider: Option<String> and model: Option<String> to UsageRecordedPayload (payload.rs:342). They must be Option: the payload_kinds! macro cannot carry per-field serde attributes, serde treats a missing Option field as None, and the log is immutable and hash-chained, so old events must keep decoding. Add Store::record_usage_attributed(..., served_by: Option<(String, String)>) and have the existing record_usage delegate to it with None, so tm-core budget.rs's 11 call sites stay untouched. agent_loop passes the provider and model from completion.model. Create [new decision: telemetry and cost attribution] covering real dollars_micros (from tel-completion-cost-field) and these attribution fields, and state that FabricState/LedgerEntry stays process-local (link ops.rs's PROVIDER_LIVE_STATE_NOTE). Later tasks amend [new decision: telemetry and cost attribution].
  acceptance: Old-shape usage.recorded JSON without provider or model still decodes. A payload with the fields round-trips. An agent_loop test asserts the emitted model matches completion.model. Existing budget tests pass untouched, and hygiene resolves [new decision: telemetry and cost attribution].
  test: `mise run test:crate -- tm-events && mise run test:crate -- tm-core && mise run test:crate -- tm-agent && mise run hygiene`

## B11 tool_call.completed events; workspace snapshot restore

Gate: `mise run verify`

- [ ] **tel-tool-call-event-kind** — Add and emit tool_call.completed events
  model: sonnet · size: M · builds Rust: yes · area: telemetry · deps: tel-usage-payload-model-field
  files: `crates/tm-events/src/kind.rs`, `crates/tm-events/src/payload.rs`, `crates/tm-agent/src/agent_loop.rs`, `docs/decisions/D-NNN-local-telemetry.md`
  change: Add EventKind::ToolCallCompleted ("tool_call.completed"): its serde rename, Display and FromStr arms, and the exhaustive kind lists in the tests. Follow the command.* precedent for its category. Add ToolCallCompletedPayload {ticket: Option<TicketId>, session: Option<SessionId>, tool_name: String, duration_ms: u64, outcome: String}, where outcome is completed, denied or error, mirroring ToolCallResolution. In agent_loop's tool-call loop (~878-964), append one event per ToolCallRecord, batched with the step's other appends where the code already batches. materialize.rs's `_ => {}` arm already tolerates new kinds. Amend [new decision: telemetry and cost attribution]. Absorbs tel-emit-tool-call-events.
  acceptance: tm-events exhaustiveness and round-trip tests pass. A new agent_loop test runs a scripted step with two tool calls, one succeeding and one denied, and asserts exactly two tool_call.completed events with the right tool_name and outcome.
  test: `mise run test:crate -- tm-events && mise run test:crate -- tm-agent && mise run hygiene`

- [ ] **replay-workspace-snapshot-restore** — Add restore_workspace_snapshot (into an isolated git worktree)
  model: sonnet · size: S · builds Rust: yes · area: replay · deps: none
  files: `crates/tm-scheduler/src/snapshot.rs`
  change: Add `pub fn restore_workspace_snapshot(repo_root: &Path, snapshot: &WorkspaceSnapshot, target_dir: &Path) -> Option<()>`. It runs `git worktree add <target_dir> <snapshot.git_ref>`, following the D-012 convention, and never mutates repo_root. Keep the same infallible Option convention as capture_workspace_snapshot: log and return None on git failure.
  acceptance: A test captures a snapshot from a dirty scratch repo, restores it into a fresh target dir, and asserts the restored contents equal the dirty contents. Restoring against a non-repo returns None.
  test: `mise run test:crate -- tm-scheduler`

## B12 `tm run --record`; TicketMetrics fold

Gate: `mise run verify`

- [ ] **replay-cli-record-flag** — tm run <T> --record <path>: capture a cassette and store it as a Transcript artifact
  model: sonnet · size: M · builds Rust: yes · area: replay · deps: replay-cassette-types, nav-fix-project-codeintel-freshness
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/sched.rs`, `crates/tm-cli/src/dispatch.rs`, `crates/tm-cli/src/agent.rs`, `docs/decisions/D-NNN-record-replay-harness.md`, `CLAUDE.md`
  change: Add --record <path> to RunArgs (args.rs:663). Thread an optional recording sink through build_dispatcher (dispatch.rs:~266) into build_fabric (agent.rs:1574), and wrap every registered provider, the TM_TEST_MOCK_PROVIDER mock included, in RecordingProvider. The header takes the dispatched AgentTask's harness_epoch. When the run finishes, store the cassette bytes as ArtifactKind::Transcript tied to the ticket through Store::store_artifact; this is the first use of that declared-but-unused kind. Amend [new decision: record/replay cassettes]'s Implemented section and add a CLAUDE.md line. Without the flag, behavior is unchanged. Absorbs replay-cassette-artifact-storage.
  acceptance: An integration test in a tempdir (TM_HOME tempdir, TM_TEST_MOCK_PROVIDER=1, TM_NOTIFY=0): `tm ticket new` then `tm run <T> --record <file>` produces a header plus at least one entry, and the ticket has a Transcript artifact whose bytes equal the file.
  test: `mise run test:crate -- tm-cli && mise run hygiene`

- [ ] **tel-ticket-metrics-fold** — Derive TicketMetrics purely from the event log (no new metrics event)
  model: sonnet · size: S · builds Rust: yes · area: telemetry / bench · deps: tel-usage-payload-model-field, tel-tool-call-event-kind
  files: `crates/tm-harness/src/metrics.rs`
  change: Add `pub fn ticket_metrics_from_events(ticket: &TicketId, events: &[Event]) -> TicketMetrics`. It folds usage.recorded (tokens, dollars, wall time), tool_call.completed (tool_calls, failures) and command.completed (commands; commands_rerun counts repeated identical argv). Add a session-level fold if SessionMetrics fits. Fields that cannot be derived stay 0, with a doc comment saying so. This replaces the bench audit's proposed ticket.metrics_recorded event: that duplicated data already in the log, and its premise was wrong, because tm-harness depends on tm-core, not the reverse, so the payload would have created a dependency cycle.
  acceptance: A fixture event vector folds to an exact expected TicketMetrics. Events for other tickets are ignored, and an empty input gives the default.
  test: `mise run test:crate -- tm-harness`

## B13 `tm run --replay`; stable symbol ids ([new decision: stable symbol ids])

Gate: `mise run verify`

- [ ] **replay-cli-replay-flag** — tm run <T> --replay <path>: offline ordered replay with divergence report
  model: sonnet · size: M · builds Rust: yes · area: replay · deps: replay-cli-record-flag
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/sched.rs`, `crates/tm-cli/src/dispatch.rs`, `crates/tm-cli/src/agent.rs`, `docs/decisions/D-NNN-record-replay-harness.md`, `CLAUDE.md`
  change: Add --replay <path>, which conflicts with --record, and --strict-replay. Build the fabric so each recorded role's provider is MockProvider::script_from_cassette, which uses ordered replay; no network is possible. When the run ends, report the divergence count and the first divergent seq, as JSON under --json. Under --strict-replay, any divergence or exhaustion (Unscripted) is a hard error. Amend [new decision: record/replay cassettes] and CLAUDE.md.
  acceptance: Record a mock run in tempdir A, then replay it against a fresh tempdir B with identical starting state. Both reach the same AgentOutcome variant with the same step count and zero divergences, thanks to path normalization. A cassette with one mutated entry reports the divergence, and under --strict-replay it fails.
  test: `mise run test:crate -- tm-cli && mise run hygiene`

- [ ] **nav-design-symbol-index-caching-stable-ids** — Stable content-derived symbol ids, optionally with an incremental cache ([new decision: stable symbol ids])
  model: opus · size: M · builds Rust: yes · area: code-navigation · deps: nav-fix-codeintel-self-reference, nav-agent-tool-index-refresh
  files: `crates/tm-codeintel/src/symbols.rs`, `crates/tm-codeintel/src/api.rs`, `docs/decisions/D-NNN-stable-symbol-ids.md`
  change: Ids come from a per-parse positional counter over path-sorted files (symbols.rs ~373-379), and symbol_index() re-parses the whole workspace on every call (api.rs:444-473). Since agent tools now refresh after writes, ids churn within a single turn. Recommended approach: derive each id from blake3(path, container chain, kind, name, ordinal among same-named siblings), truncated to u64. Deliberately leave out the byte range, which moves whenever lines are edited above the symbol. Optionally cache the parsed index and rebuild only the files in IndexDelta. Pick an approach and write [new decision: stable symbol ids] with the tradeoffs: renames and moves change ids, and cache invalidation.
  acceptance: New test: a symbol id in z.rs is unchanged after update_incremental adds a.rs, which sorts first. Existing symbol and rename tests pass.
  test: `mise run test:crate -- tm-codeintel && mise run hygiene`

## B14 `tm stats`; tool-result replay

Gate: `mise run verify`

- [ ] **tel-stats-cli-command** — tm stats: per-ticket/day/model/tool rollups over local telemetry
  model: sonnet · size: M · builds Rust: yes · area: telemetry · deps: tel-ticket-metrics-fold, tel-tool-call-event-kind, tel-usage-payload-model-field
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/stats.rs`, `crates/tm-cli/src/lib.rs`, `crates/tm-cli/src/main.rs`, `CLAUDE.md`, `docs/decisions/D-NNN-local-telemetry.md`
  change: Add Command::Stats(StatsArgs) with `--by ticket|day|model|tool` (default ticket), `--ticket T` and --json. Read the project's EventLog. Per ticket, use tm_harness ticket_metrics_from_events. Day buckets come from event timestamps. Model grouping uses usage.recorded provider and model, with None shown as 'unattributed'. Tool grouping reports count, failures and average duration_ms. Keep the aggregation in a pure, unit-tested function and render a Table or JSON. Wire it up in main.rs and lib.rs, then amend [new decision: telemetry and cost attribution] and CLAUDE.md. Absorbs tel-stats-tools-rollup.
  acceptance: Unit tests cover the aggregation. Integration test in a tempdir with the mock provider: after tm run, `tm stats --json` shows nonzero tokens for that ticket, and `--by tool` lists at least one tool if the mock script calls one.
  test: `mise run test:crate -- tm-cli && mise run hygiene`

- [ ] **replay-tool-replay-mode-design** — Replay recorded tool resolutions instead of re-executing tools
  model: opus · size: M · builds Rust: yes · area: replay · deps: replay-cli-replay-flag, tel-tool-call-event-kind
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-agent/src/executor.rs`, `docs/decisions/D-NNN-record-replay-harness.md`
  change: Add a ReplayToolSource option on AgentLoop. In replay mode, before dispatching a real tool call, match it against the recorded ToolCallRecord, first by tool_use_id and then by tool_name plus a normalized input hash, and return the recorded ToolCallResolution verbatim. When nothing matches, fail with a distinct divergence error; never fall through to real execution. Decide and document how authority checks interact with this, and whether recorded resolutions come from session StepRecords or tool_call.completed events. Amend [new decision: record/replay cassettes] with a tool-replay section; do not create a new D number.
  acceptance: A replay of a recorded run through a test-double Executor, asserted to receive zero calls, reaches the same AgentOutcome as the original. An unrecorded tool call produces the divergence error.
  test: `mise run test:crate -- tm-agent && mise run hygiene`

## B15 replay-diff; bench fixture schema

Gate: `mise run verify`

- [ ] **replay-diff-outcomes-command** — tm harness replay-diff <a> <b>: structural diff of two session transcripts
  model: sonnet · size: S · builds Rust: yes · area: replay · deps: replay-cassette-types
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/replay_diff.rs`, `crates/tm-cli/src/lib.rs`, `CLAUDE.md`
  change: Add a HarnessCommand::ReplayDiff subcommand and a dispatch_harness arm in ops.rs. Put the logic in the new replay_diff.rs. Load both session JSON files through the existing session deserialization (agent.rs:215), walk the paired turns and StepRecords, and report assistant_text mismatches, tool-call name, input or resolution mismatches, spend deltas and step-count mismatches. If both inputs carry a harness epoch (a session pin or a cassette header), warn when they differ. The comparison is a pure function; add a CLAUDE.md line.
  acceptance: Unit tests: identical step vectors give an empty diff, and one differing tool resolution reports exactly that step index. Differing epochs produce the warning.
  test: `mise run test:crate -- tm-cli`

- [ ] **bench-schema-repo-fixture-fields** — Extend BenchFixture with optional test_command/setup_commands
  model: haiku · size: S · builds Rust: yes · area: bench · deps: none
  files: `crates/tm-harness/src/bench.rs`
  change: Add `#[serde(default)] test_command: Option<Vec<String>>` and `#[serde(default)] setup_commands: Vec<Vec<String>>` to BenchFixture (bench.rs:20-25), keeping deny_unknown_fields intact. Treat a task that has a test_command and no script.txt as live-only: the scripted (non-live) runner skips it with a note instead of failing, so adding real-repo fixtures cannot break `tm bench run`. Update the module docs. Do not cite [new decision: live benchmark mode], which lands in B17.
  acceptance: hello-world.toml parses unchanged. A TOML with the new fields round-trips. The scripted runner skips a live-only task and still reports hello-world.
  test: `mise run test:crate -- tm-harness`

## B16 Bench report; workflow starters; SWE-lite fixtures

Gate: `mise run verify && bash bench/tools/check-fixtures.sh`

- [ ] **bench-report-render** — tm bench report <json> [--out]: render a BenchmarkReport as markdown
  model: sonnet · size: S · builds Rust: yes · area: bench · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/bench_report.rs`, `crates/tm-cli/src/lib.rs`, `CLAUDE.md`
  change: Add BenchCommand::Report(BenchReportArgs{path, out}) and a dispatch_bench arm, with the rendering in bench_report.rs. Read a BenchmarkReport JSON and emit markdown: aggregate score, then a per-task table of pass/fail, score, tokens, cost, tool_calls and wall time. The rendering function is pure. Add a CLAUDE.md line.
  acceptance: A unit test renders a hand-built BenchmarkReport to the expected markdown, and `tm bench run --out r.json && tm bench report r.json` works in a tempdir.
  test: `mise run test:crate -- tm-cli`

- [ ] **workflow-add-missing-starter-definitions** — Add the migrate-sites and research-and-synthesize SPEC §25 starter workflows
  model: sonnet · size: S · builds Rust: yes · area: workflow (SPEC §25) · deps: none
  files: `crates/tm-workflow/fixtures/migrate-sites.toml`, `crates/tm-workflow/fixtures/research-and-synthesize.toml`, `crates/tm-workflow/tests/expand_snapshot.rs`
  change: Write two WorkflowDef fixtures against the existing schema in def.rs. migrate-sites uses a static for_each fan-out. research-and-synthesize uses a FromOutput for_each feeding a join with a merge rule. Add parse and expand tests in expand_snapshot.rs following the review-change and harness-benchmark pattern, including snapshot files if the test uses them.
  acceptance: Both fixtures parse, and expand() produces subgraphs that pass tm-core's invariant checks.
  test: `mise run test:crate -- tm-workflow`

- [ ] **bench-swe-lite-fixture-ingestion** — Vendor 3-5 small hermetic bug-fix-with-failing-test bench tasks
  model: sonnet · size: M · builds Rust: no · area: bench · deps: bench-schema-repo-fixture-fields
  files: `bench/tasks/`, `bench/fixtures/`, `bench/solutions/`, `bench/tools/check-fixtures.sh`, `bench/tools/PROVENANCE.md`
  change: Pick 3 to 5 small, permissively licensed bug-fix tasks, each a few MB at most, runnable with toolchains already present: Python via `uv run --with pytest pytest` (uv is in mise.toml), or single-crate Rust with no dependencies. Do not attempt the full SWE-bench corpus. For each task: a minimal repo snapshot in bench/fixtures/<id>/, a bench/tasks/<id>.toml using test_command/setup_commands, and a reference fix at bench/solutions/<id>.patch, which lives outside the fixture dir so it is never copied into an agent's workspace. Add check-fixtures.sh, which for each task copies the fixture to mktemp, asserts test_command fails, applies the solution patch, and asserts it passes. Record provenance and license per task in PROVENANCE.md. Do not name any file live-smoke; that name is reserved for B17.
  acceptance: check-fixtures.sh passes for every new task (fails before the patch, passes after), and tm bench list parses them.
  test: `bash bench/tools/check-fixtures.sh`

## B17 Live benchmark mode ([new decision: live benchmark mode]); dollar-aware affordability

Gate: `mise run verify`

- [ ] **bench-live-seeded-provider** — tm bench run --live: drive real ticket runs per task and score with real tests ([new decision: live benchmark mode])
  model: opus · size: M · builds Rust: yes · area: bench · deps: bench-schema-repo-fixture-fields, tel-ticket-metrics-fold, replay-cli-record-flag, nav-agent-tool-index-refresh, nav-fix-project-codeintel-freshness
  files: `crates/tm-cli/src/bench_live.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/lib.rs`, `docs/decisions/D-NNN-live-benchmark-mode.md`, `bench/tasks/live-smoke.toml`, `bench/fixtures/live-smoke/`, `CLAUDE.md`
  change: Add --live to BenchRunArgs (args.rs:888) and a LiveSeededProvider implementing tm_harness::SeededProvider in bench_live.rs. For each task: copy bench/<fixture> into a fresh mktemp dir (per-task isolation; git worktrees are not used), then git init and commit so codeintel history works; run setup_commands; create and activate one ticket with Authority::worker() and run it through sched.rs's real run_ticket path (do not reimplement the AgentLoop wiring), always with a per-task cassette recorded next to the report; after the turn, run test_command and emit tests_pass:<suite> only on exit 0; fill TaskResult tokens, cost and tool_calls from ticket_metrics_from_events over the scratch project's log. is_finished returns true after the one run. Use the mock fabric under TM_TEST_MOCK_PROVIDER and a real one through build_fabric only when configured. Add a purpose-built live-smoke task whose test_command is ["true"]. Write [new decision: live benchmark mode] covering what --live does and its cost and determinism tradeoffs, and add a CLAUDE.md line. Absorbs bench-live-flag-plumbing.
  acceptance: With TM_TEST_MOCK_PROVIDER=1 and a scratch project, `tm bench run --live --filter live-smoke` produces a TaskResult whose passed value comes from actually running test_command, with nonzero tokens and a cassette file. The report includes cost and tool_calls. Without --live, output is byte-identical to today's.
  test: `mise run test:crate -- tm-cli && mise run hygiene`

- [ ] **budget-affordability-menu-in-context-pack** — Dollar-aware pre-call affordability, tier-down, and an affordable-tiers menu (SPEC §31)
  model: opus · size: M · builds Rust: yes · area: budget (SPEC §31) · deps: tel-completion-cost-field
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-context/src/sections.rs`, `crates/tm-provider/src/fabric.rs`
  change: The audit's claim that 'no refuse-to-start logic exists' is wrong. A token-only pre-call check already exists (agent_loop.rs:755-770, can_afford and first_unaffordable_dimension), but its estimate hardcodes dollars_micros: 0. Extend the estimate to dollars using candidate prices: prompt tokens times the input price plus MAX_TOKENS_PER_STEP times the output price, via tel-completion-cost-field's cost_micros. Add Fabric::affordable_candidates(role, remaining budget, estimated tokens). When the primary candidate is unaffordable but a cheaper one fits, tier down for that step instead of handing off. Render a compact 'Budget' section listing the affordable tiers with roughly how many steps each allows, as a context-pack section or a system-prompt addendum, whichever fits the compile path. Document the estimate formula in doc comments.
  acceptance: With a MockProvider-backed Fabric holding two priced candidates and a nearly exhausted dollar budget, the step routes to the cheaper candidate, the menu names only affordable tiers, and a step no candidate can afford hands off instead of being attempted.
  test: `mise run test:crate -- tm-provider && mise run test:crate -- tm-context && mise run test:crate -- tm-agent`

## B18 Cross-tool benchmark ([new decision: cross-tool benchmark]); promotion baseline persistence

Gate: `mise run verify`

- [ ] **bench-cross-tool-comparison-harness** — Head-to-head tm vs opencode/Codex/Claude Code benchmark runner ([new decision: cross-tool benchmark])
  model: opus · size: M · builds Rust: yes · area: bench · deps: bench-live-seeded-provider, bench-report-render, bench-swe-lite-fixture-ingestion
  files: `crates/xtask/src/bench_cross.rs`, `crates/xtask/src/main.rs`, `docs/decisions/D-NNN-cross-tool-benchmark.md`, `bench/README.md`, `CLAUDE.md`, `mise.toml`
  change: Add an xtask subcommand, plus a mise bench:cross task, that runs the bench/tasks suite identically through `tm bench run --live` and through each configured external CLI, as sketched in docs/backlog.md:498-517. Credentials default to DevPass; real-Claude auth needs an explicit opt-in flag. Each external tool gets a per-task mktemp copy, runs the same test_command scoring, and has its tokens, wall time and tool calls captured where the tool reports them. Store results as real ticket and event-log state in a scratch project, as the backlog asks, and render them with bench report's shape. Real external runs are opt-in and never part of verify; unit tests use a fake tool adapter. Write [new decision: cross-tool benchmark] and add CLAUDE.md and mise lines. Scope depends on the owner's answer about which tools and credentials to use.
  acceptance: Unit tests with a fake adapter produce a comparable report for tm and one other tool on live-smoke. A documented manual command runs a real comparison when credentials exist.
  test: `cargo test -p xtask -j 2 && mise run hygiene`

- [ ] **bench-promotion-baseline-persistence** — Persist a benchmark per promoted harness epoch as the automatic baseline
  model: sonnet · size: M · builds Rust: yes · area: bench / harness · deps: mirror-persist-content-hash-for-idempotency
  files: `crates/tm-core/src/schema.rs`, `crates/tm-core/src/store.rs`, `crates/tm-core/src/materialize.rs`, `crates/tm-cli/src/ops.rs`
  change: When promoting an epoch (the promote flow at ops.rs ~1257-1360), persist the resolved BenchmarkReport alongside the harness_epochs row: add a benchmark_json column materialized from the promotion event's payload, with a schema-version bump that follows mirror-persist-content-hash's migration. Store::harness_epochs() returns the stored report. When --baseline is not given, promote defaults to the previous epoch's stored report.
  acceptance: A tm-core test promotes with a report and harness_epochs() returns it. A tm-cli test promotes a second epoch without --baseline and the gate compares against the first epoch's report.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`

## B19 Genesis lifecycle events; doc staleness trigger

Gate: `mise run verify`

- [ ] **genesis-emit-lifecycle-events** — Append genesis.* events on each stage transition
  model: sonnet · size: M · builds Rust: yes · area: genesis · deps: genesis-stop-infinite-maturity-loop
  files: `crates/tm-core/src/store.rs`, `crates/tm-genesis/src/stages.rs`
  change: The genesis.started, genesis.stage_entered and genesis.stage_completed kinds already exist (tm-events kind.rs:273-280). GenesisDriver cannot append them only because Store exposes no API for it (stages.rs:12-19). Add a narrow Store method, e.g. record_genesis_stage(stage, artifact ref, actor), and call it from GenesisDriver::advance. Update stages.rs's module doc.
  acceptance: After advance() runs, the project's event log, read the way tm events reads it, has one stage event per completed stage.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-genesis`

- [ ] **docs-wire-real-staleness-trigger** — Feed tm_docs::Assessor a real git ChangeSet so tm docs check can fail
  model: sonnet · size: M · builds Rust: yes · area: docs (SPEC §9) · deps: docs-persist-state-across-invocations
  files: `crates/tm-cli/src/ops.rs`, `crates/tm-docs/src/assess.rs`, `crates/tm-docs/src/provenance.rs`
  change: In docs_check and docs_reconcile, build a ChangeSet from a git2 diff between each doc's last-verified commit (from doc_provenance, recorded at verify or attest time) and HEAD plus the working tree. Run it through Assessor::assess before Assessor::check, and persist the resulting state through the storage added in docs-persist-state-across-invocations. Update ops.rs's admission comment (146-149).
  acceptance: Editing a file matched by a doc's derived_from glob makes tm docs check exit 1 and name that doc Stale. Editing an unrelated file exits 0.
  test: `mise run test:crate -- tm-docs && mise run test:crate -- tm-cli`

## B20 Genesis explicit V0/V1; tm docs attest

Gate: `mise run verify`

- [ ] **genesis-explicit-v0-v1-milestone-marking** — Select V0/V1 milestones by explicit tag, not position
  model: sonnet · size: M · builds Rust: yes · area: genesis · deps: genesis-emit-lifecycle-events, genesis-shared-offline-fixtures
  files: `crates/tm-genesis/src/compile.rs`, `crates/tm-genesis/src/spec.rs`, `crates/tm-genesis/src/stages.rs`, `crates/tm-genesis/src/fixtures.rs`
  change: Thread a release marker (v0 or v1) from Specification's ReleaseDefinitions through the graph-compilation prompt and its JSON schema onto the committed milestones. Make the marker optional in the schema so older fixtures keep working, and update fixtures.rs to include it. Change Stage::Ignition and Stage::MaturityGate (stages.rs:430-437, 459-467) to select by marker, falling back to the current positional heuristic only when no marker exists.
  acceptance: A test with milestones ordered so the positional heuristic would pick wrong confirms the tagged milestone is selected. The existing genesis and e2e tests pass.
  test: `mise run test:crate -- tm-genesis && mise run test:crate -- tm-e2e`

- [ ] **docs-wire-attestation-cli-path** — tm docs attest <doc> --note: close a Review ticket with a human Attestation
  model: sonnet · size: S · builds Rust: yes · area: docs (SPEC §9) · deps: docs-persist-state-across-invocations
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-docs/src/reconcile.rs`, `CLAUDE.md`
  change: Add a DocsCommand::Attest variant and its dispatch_docs arm. Build a tm_docs::reconcile::Attestation, record it as Evidence{kind: HumanAttestation} against the doc's open Review ticket, close that ticket (human actor only), set the doc's persisted state to Fresh and record the new last-verified commit. Add a CLAUDE.md line.
  acceptance: On a doc with an open Review ticket, attest closes the ticket, writes a HumanAttestation evidence row, and a following docs list shows Fresh.
  test: `mise run test:crate -- tm-cli`

## B21 Genesis template catalog; tm acp

Gate: `mise run verify`

- [ ] **genesis-wire-template-catalog** — Pass a real template catalog into GenesisDriver's graph compilation
  model: sonnet · size: M · builds Rust: yes · area: genesis · deps: genesis-explicit-v0-v1-milestone-marking, genesis-cli-resume-flag
  files: `crates/tm-genesis/src/stages.rs`, `crates/tm-cli/src/project.rs`, `crates/tm-templates/src/lib.rs`
  change: stages.rs:401-416 passes `&[]` as the catalog. Recommended: tm-templates exposes a bundled catalog (manifests from templates/starter embedded at compile time) with a TM_TEMPLATES_DIR runtime override. GenesisDriver takes the catalog as a constructor argument, and the CLI supplies it. Remove the 'no catalog wired' comment.
  acceptance: A test advances GraphCompilation with a non-empty catalog and a Specification whose prose matches one template's tags, and GraphSummary.selected_template is populated. tm-templates' starter tests still pass.
  test: `mise run test:crate -- tm-templates && mise run test:crate -- tm-genesis && mise run test:crate -- tm-cli`

- [ ] **acp-wire-tm-acp-serve-command** — tm acp: serve the project as an ACP agent over stdio
  model: sonnet · size: M · builds Rust: yes · area: clients / ACP (SPEC §28.1) · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/acp.rs`, `crates/tm-cli/src/main.rs`, `crates/tm-cli/src/lib.rs`, `CLAUDE.md`
  change: Add Command::Acp(AcpArgs), modeled on McpArgs, and a new acp.rs modeled on mcp.rs. Open the project with project::open_for_command, build tm_acp::AcpServer::new(Arc::new(ProjectAgentBackend::new(project.store.clone(), ids))), attach stdin and stdout, keep diagnostics on stderr, and await until stdin closes. Add a main.rs arm next to Command::Mcp and a CLAUDE.md line. tm-cli already depends on tm-acp (dispatch.rs uses AcpExecutor).
  acceptance: A subprocess test in a tempdir project (TM_TEST_MOCK_PROVIDER=1) sends initialize on stdin and receives a valid InitializeResponse, mirroring tm-acp's server_roundtrip.rs.
  test: `mise run test:crate -- tm-cli`

## B22 Swift symbol navigation

Gate: `mise run verify`

- [ ] **nav-add-swift-language-support** — tree-sitter Swift support so clients/macos gets symbol navigation
  model: sonnet · size: M · builds Rust: yes · area: code-navigation · deps: nav-design-symbol-index-caching-stable-ids
  files: `crates/tm-codeintel/src/walk.rs`, `crates/tm-codeintel/src/symbols.rs`, `crates/tm-codeintel/Cargo.toml`, `Cargo.toml`, `Cargo.lock`
  change: Add Language::Swift and map .swift to it in from_extension (walk.rs:58-69). Add a tree-sitter-swift dependency pinned compatibly with the workspace's tree-sitter version, and wire grammar_for. Write symbol and reference queries covering func, class, struct, enum, protocol and extension declarations and call sites, applying the same self-reference exclusion. Ids come from the [new decision: stable symbol ids] scheme.
  acceptance: On a scratch Swift file, outline lists the function, def resolves it, and refs finds exactly the call site.
  test: `mise run test:crate -- tm-codeintel`

## D-020 (deferred)

6 owner-gated batches exist in the audit plan (journal of `wf_87a85411-848`). Not scheduled until the owner brings D-020 back.

## Dropped by the planner

- nav-fix-dispatch-context-pack-freshness: merged into nav-fix-project-codeintel-freshness. It is the same one-call fix with the same degrade-on-error policy; project.rs and dispatch.rs change together.
- nav-fix-history-why-whole-file-default: merged into nav-fix-cli-symbol-output-bugs. Both are small fixes in crates/tm-cli/src/search.rs.
- vscode-fix-ticket-enum-casing: merged into vscode-migrate-to-shared-ts-sdk. Fixing the stand-in client that the migration deletes would be wasted work; the SDK's correct types fix the casing by construction, and the regression checks stay in that task's acceptance.
- vscode-fix-field-name-casing: merged into vscode-migrate-to-shared-ts-sdk (same reason; the affected_paths CodeLens test is kept).
- vscode-fix-lease-request-body: merged into vscode-migrate-to-shared-ts-sdk (claimTicket uses the SDK's acquireLease with actor and ttl_seconds; the body assertion is kept).
- vscode-fix-submit-transition-shape: merged into vscode-migrate-to-shared-ts-sdk (attachEvidence, then {submit:{summary,evidence,actor}}; the body assertion is kept).
- vscode-migrate-to-shared-ts-sdk (build part): split out as vscode-bundle-and-link-shared-sdk. The SDK is ESM-only and the extension is CJS, so it needs an esbuild bundle and link:../ts; workspace:* cannot resolve because each client has its own pnpm-workspace.yaml.
- ts-sdk-schema-snapshot-drift-note: kept under the same id, expanded to include the one-line routes.rs get_schema description fix (routes.rs:595 still omits accept/reject/retry). It checks for the server-fixes track first.
- replay-recording-provider-wrapper: merged into replay-cassette-types. It is the same new cassette.rs module, and serial tasks on one file cannot run in parallel anyway.
- replay-harness-epoch-metadata: merged into replay-cassette-types (the header carries harness_epoch from the start), with the value threaded in by replay-cli-record-flag and the mismatch warning in replay-diff-outcomes-command.
- replay-cassette-artifact-storage: merged into replay-cli-record-flag. It is the first use of ArtifactKind::Transcript and belongs to the CLI run that produced the cassette, not to tm-scheduler's report_outcome.
- tel-wire-real-cost-usage-recorded: merged into tel-completion-cost-field, which was redesigned. Adding a field to Completion would touch about 40 struct-literal sites in 5 crates; Fabric::execute_priced plus the one-line agent_loop change touches 2 files.
- tel-emit-tool-call-events: merged into tel-tool-call-event-kind (define and emit in one task; agent_loop.rs and payload.rs would otherwise be edited in two serial batches).
- tel-stats-tools-rollup: merged into tel-stats-cli-command, which now runs after tool_call.completed lands.
- tel-decision-doc: merged. [new decision: telemetry and cost attribution] is created by tel-usage-payload-model-field and amended by tel-tool-call-event-kind and tel-stats-cli-command, so the docs land with the changes (CLAUDE.md rule) and the hygiene cross-reference check never sees a [new decision: telemetry and cost attribution] citation before the file exists. Renumbered from [new decision: genesis termination], which three audits all claimed.
- bench-live-flag-plumbing: merged into bench-live-seeded-provider. A stub 'not yet implemented' error that the next task immediately replaces is wasted work.
- bench-worktree-isolation-live-runs: dropped. Vendored fixtures are copied into a fresh mktemp directory per task in bench-live-seeded-provider, which already isolates runs. D-012 git worktrees only apply to checkouts of this repo, which bench fixtures are not.
- bench-metrics-event-schema: replaced by tel-ticket-metrics-fold. A ticket.metrics_recorded event would duplicate usage.recorded and tool_call.completed data already in the log, and the audit's premise was false: tm-harness depends on tm-core, not the reverse, so the proposed tm-core-side TicketMetrics payload would have created a dependency cycle.
- bench-metrics-recording-call-site: replaced by tel-ticket-metrics-fold. Metrics are derived purely from events on demand, used by tm stats and by bench-live-seeded-provider's TaskResult, so no extra write call is needed in sched.rs or agent.rs.
- docs-persist-reconciling-transition: merged into docs-persist-state-across-invocations (same functions in ops.rs, same new Store write path).
- docs-differentiate-review-vs-regeneration-tickets: merged into docs-persist-state-across-invocations (a one-line ExecutorRequirements change in the same docs_reconcile loop; ops.rs is a serialization hotspot).
- budget-affordability-menu-in-context-pack: kept under the same id but rescoped. The audit said no refuse-to-start logic exists, but a token-only pre-call check already does (agent_loop.rs:755-770, can_afford and first_unaffordable_dimension). What remains is the dollar dimension, tier-down and the menu, so it depends on tel-completion-cost-field.
- replay-tool-replay-mode-design: kept under the same id, but it amends [new decision: record/replay cassettes] (created by replay-cassette-types) instead of creating [new decision: genesis termination]. Decision numbers are pre-assigned: [new decision: genesis termination] genesis, [new decision: telemetry and cost attribution] telemetry, [new decision: record/replay cassettes] record/replay, [new decision: stable symbol ids] symbol ids, [new decision: live benchmark mode] live bench, [new decision: cross-tool benchmark] cross-tool.
- genesis-stop-infinite-maturity-loop: kept; its decision doc is [new decision: genesis termination] (it lands first, in B3).
- d020-gate-owner-approval: not a task (no files, no test). Converted into the gate string on all six D-020 (OWNER-GATED) batches.
- d020-providers-toml-decider-role: merged into d020-role-decider. It is the same role_config.rs file; parsing is generic over Role, so the extra work is one fixture round-trip test.

## Owner questions (also in docs/backlog.md when asked)

- Repo and release: origin is already a private GitHub repo (alliecatowo/ticket-master, created 2026-09-22 per CLAUDE.md), so nothing new needs creating. release-cut-v0-1-0 will push main, tag v0.1.0, and publish a GitHub Release with macOS arm64 and Linux x86_64 tarballs. Is a GitHub Release the build artifact you want, or do you want a local tarball or Homebrew tap instead?
- HELLO.md and HELLO_VIOLET.md are untracked in the primary checkout. Should they be committed, gitignored or deleted before main is pushed? The plan will not commit them silently.
- GitHub Actions cost: macOS runners on private repos bill at a multiple of Linux minutes. The release workflow builds on macos-14 only when a tag is pushed, and ci.yml already runs macos-latest on every push. Is that acceptable?
- D-020: do you approve building the six owner-gated D-020 batches (the shadow-mode slice)? Separately, d020-docs-vercel-gateway-findings is docs-only and needs no approval per its own audit; may it run now instead of waiting at the end? Do you want to create an AI_GATEWAY_API_KEY?
- Genesis: the plan recommends exit-and-resume for [new decision: genesis termination]. tm genesis would stop after committing the Draft graph and tell you to run `tm sched run`, then `tm genesis --resume`. The alternatives are driving the scheduler in-process inside genesis, or a bounded number of retries followed by a report. Which do you want?
- Benchmarks: which external CLIs should the cross-tool harness drive (opencode, codex, claude), with which credentials (DevPass by default, per docs/backlog.md:498), and what real-API spend is acceptable for `tm bench run --live` and cross-tool runs?
- SWE-lite fixtures: which toolchains are allowed (Python via uv, Node, dependency-free Rust), and are there licensing constraints on vendoring third-party code snapshots into this private repo?
- Semantic search currently uses LocalHashEmbedder, a deterministic hash-based stand-in, so any benchmark that scores retrieval quality will hit a ceiling. Do you want a real embedding provider wired in, and if so which one? It is not in this plan.
- Swift symbol support (B22) adds a tree-sitter-swift C grammar, which costs compile time and disk on the 8GB machine. Build it now or defer?
