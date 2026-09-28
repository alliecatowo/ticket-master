# TASKS — the working task list

Conventions: `docs/tasks/README.md`. Source: the 2026-09-23 benchmark-readiness audit (workflow `wf_87a85411-848`, planner output), plus tasks found by live trials of every surface (section **T**). Worked by the `tasks-all` workflow: Sonnet/Haiku implementers in worktrees, at most two Rust builds at once, merged one at a time with a crate test + clippy, and a full `mise run verify` every few merges.

Decision numbers: D-021 and D-022 are the provider/config docs from the parallel provider-overhaul session, D-023 is the capacity-wait decision. A task marked `[new decision: X]` writes the next free `docs/decisions/D-NNN-*.md` when it lands.

Not here: D-020/jev (deferred, `docs/backlog.md` "Ask the owner later"); the web e2e suite (parked); provider/model/config UX (owned by the provider-overhaul workflow in `workflows/`, don't duplicate it).

Plan summary: tm is close to benchmark-ready on primitives, but the pieces are not wired together. The event log is hash-chained, tm-codeintel's four retrieval modes are real, and tm bench list/run/compare, D-012 worktrees, D-008 snapshots, and MockProvider scripting all exist. What blocks a trustworthy benchmark is a set of verified wiring bugs. (1) Nothing on the CLI, chat, dispatch, agent-tool or MCP paths calls CodeIntel::update_incremental. On a project that was never doctored, symbol and history queries come back empty, and files an agent creates mid-run never enter the index. The tool set holds a single Arc<CodeIntel> for the whole session (tools.rs:793, dispatch.rs:267). (2) Symbol refs and callers count a definition's own name as a reference, and ids are positional, so they shift when the file set changes. (3) `tm events replay` and `tm events tail` are stubs; I read both (ops.rs:2199-2289). (4) `tm genesis` cannot run offline, and with a real key it never terminates: tickets are committed as Draft and the failed MaturityGate/Stabilization cycle loops forever (stages.rs:183-190, project.rs:1297-1315). (5) usage.recorded always has dollars_micros 0 and names no model, and tool calls are never logged as events. (6) Nothing records or replays provider traffic. (7) `tm bench run` only replays scripted text and scores TestsPass by matching a string; there is no --live mode. Client surfaces have separate wire bugs: the vscode extension's stand-in client uses the wrong casing and request shapes, and the TS SDK lacks accept/reject/retry. SPEC §9 doc staleness can never fire, and mirror push re-sends every ticket on every run. The plan fixes wiring and correctness first (B1-B9: index freshness on every path including in-run tools, symbol accuracy, the events replay/tail stubs, genesis termination and offline mock/resume/e2e, docs state, mirror idempotency, MCP navigation tools, client wire fixes). Batch 1 also covers the user's direct request: a release workflow, install guidance and `mise run install`. The GitHub repo is already private (origin, per CLAUDE.md), and B2 pushes main and cuts v0.1.0. Telemetry and record/replay come next (B9-B15). Real cost and model attribution, tool_call.completed events, a pure TicketMetrics fold and `tm stats` are covered by [new decision: telemetry and cost attribution]. The cassette format, `tm run --record/--replay`, tool-result replay and replay-diff are covered by [new decision: record/replay cassettes]. Replay is ordered with per-entry divergence reporting, because matching by request hash would diverge on the first call in a fresh tempdir. Only then does the benchmark come (B15-B18): the fixture schema, report rendering, SWE-lite-style fixtures, the live mode ([new decision: live benchmark mode], reusing the fold and the cassettes) and the cross-tool harness ([new decision: cross-tool benchmark]). Remaining features follow in B19-B22: genesis events, V0/V1 tags and the template catalog, docs staleness and attest, tm acp, Swift. D-020 comes last, in six owner-gated batches. Decision numbers are pre-assigned so tasks running in parallel don't collide: [new decision: genesis termination] genesis, [new decision: telemetry and cost attribution] telemetry, [new decision: record/replay cassettes] record/replay, [new decision: stable symbol ids] symbol ids, [new decision: live benchmark mode] live bench, [new decision: cross-tool benchmark] cross-tool. The orchestrator, not a subagent, should write this plan into docs/tasks/TASKS.md, leaving out D-020 as that README requires.

## T — Found in live trials

Trial agents drive each surface for real (ticket request → implementation → verification → accepted, Genesis, TUI, server/web, MCP, code navigation) and append concrete tasks here. These run before the plan batches below.

- [x] **nav-potion-embedder** (landed a0bb73e) — Real semantic search: potion static embeddings, zvec-grep style
  model: sonnet · severity: high · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-codeintel/src/embed.rs`, `crates/tm-codeintel/src/potion.rs`, `crates/tm-codeintel/src/api.rs`, `crates/tm-codeintel/src/hybrid.rs`, `crates/tm-codeintel/Cargo.toml`, `Cargo.toml`, `Cargo.lock`, `CLAUDE.md`
  change: The semantic half of hybrid search runs on `LocalHashEmbedder`, a hash stand-in, so "semantic" search is lexical in disguise. The owner wants search like zvec-grep's (the tool this repo already uses), which embeds with model2vec static embeddings. Add a `PotionEmbedder` implementing `embed::Embedder` in a new `potion.rs`, using the `model2vec-rs` crate (MinishLab's official Rust port; pure Rust, CPU only, no ONNX). It uses `minishlab/potion-code-16M-v2` by default, the same model zvec-grep uses. Load it from the local HuggingFace cache (`$HF_HOME` or `~/.cache/huggingface/hub/models--minishlab--potion-code-16M-v2/snapshots/*`, already present on this machine). Download it at runtime only if it's missing and the user hasn't opted out; never download in tests. Make it the default for `CodeIntel` when the model is available; fall back to `LocalHashEmbedder`, logging once through tracing, when it isn't. Keep the hash embedder for tests: hygiene forbids network in tests. The vectors table is already keyed by `embedder.identifier()`, so switching embedders must re-embed the stale chunks on the next `update_incremental` rather than mixing vector spaces. Make sure `hybrid` fuses FTS and vector hits with RRF the way zvec-grep does, and that code chunks are symbol- or line-window-sized, not whole files. Add an `index.embedder = "potion" | "hash"` project config key, plus `TM_EMBEDDER` for overrides. Record the choice in a new decision doc (next free D-NNN; D-002's format).
  acceptance: On a clone of this repo, `tm search hybrid "where are tickets moved between states"` puts `crates/tm-core/src/machine.rs` in the top 3, and "how is the provider chosen for a request" puts `crates/tm-provider/src/fabric.rs` in the top 3; show before/after in the handoff. Unit tests cover the fallback path and re-embedding on an identifier change, and use a tiny fixture model or the hash embedder, never the network. Indexing this repo from scratch with potion takes under 60s on this machine.
  test: `mise run test:crate -- tm-codeintel`
  evidence: owner, 2026-09-23: "use potion like zvec grep for semantic search, basically zvec grep inspired"; `crates/tm-codeintel/src/embed.rs` only has `LocalHashEmbedder`.

- [x] **release-web-assets** (landed 6065b21) — Ship the web client in the release tarball, and let an installed `tm serve` find it
  model: sonnet · size: S · builds Rust: yes · area: release · deps: none
  files: `crates/tm-cli/src/serve.rs`, `mise.toml`, `.github/workflows/release.yml`, `scripts/install.sh`, `docs/install.md`, `README.md`, `CLAUDE.md`
  change: Branch `worktree-agent-a279c9fa9d538d6a2` (commit 5c7f7db) holds unfinished work: a `mise run release` task that packs `bin/tm` plus `share/tm/web` and a sha256 (refusing to pack `.env` or `.tm`), `serve.rs` looking for the web client at `<exe>/../share/tm/web`, `scripts/install.sh` (via `gh release download`, since the repo is private) and `docs/install.md`. Merge that branch into your worktree and reconcile it with main, which already has `.github/workflows/release.yml`, README install paths (edd3d7e) and release v0.1.0: keep main's README and add to it rather than replacing it, and make `release.yml` build the web client (pnpm) and pack it the same way the mise task does. Don't publish a release.
  acceptance: `mise run release` builds a tarball whose `share/tm/web/index.html` exists and which contains no `.env`/`.tm`; a test covers the installed-layout lookup in `serve.rs`; README and docs/install.md agree with each other and with release.yml.
  test: `mise run test:crate -- tm-cli`

- [x] **s1-surfaces-decision-doc** (landed 1910989) — Write the command-surfaces decision doc (next free D-NNN)
  model: sonnet · severity: high · builds Rust: no · area: docs · deps: none
  files: `docs/decisions/D-0NN-command-surfaces.md`, `docs/decisions/D-019-claude-code-parity-shell.md`, `CLAUDE.md`
  change: Run `ls docs/decisions` and take the next free number (another track may claim the next-lowest first — re-check before naming the file). Write the design in the D-002 format (Status, Date, Supersedes, then Context, Decision, Why, "What this costs, stated plainly"). The Decision section covers: the visible/hidden CLI tree (daily/planning/serving/more groups, `#[command(hide = true)]` plumbing verbs), the slash-command table (existing plus `/context`, `/todos`, `/memory`, `/export`, `/doctor`, `/permissions`, `/review`, `/search`, `/run`, `/ticket`, `/board`, `/milestones`, `/timeline`, `/deps`, `/workflow`), the Tickets-hub tab strip (Tickets · Board · Milestones · Timeline · Graph), the one-set-of-display-labels rule, and ticket due dates. Add a one-line pointer in D-019's command list and in CLAUDE.md's surface paragraph.
  acceptance: The new file exists with the D-002 section headings. D-019 and CLAUDE.md reference it by its real number. `mise run hygiene` passes (no dangling D-NNN).
  test: `mise run hygiene`
  evidence: `ls docs/decisions | tail -2` -> D-022-unified-provider-model-config-ux.md, D-023-capacity-wait-is-not-a-failed-attempt.md

- [x] **s1-dep-rm-emit-removed-event** (landed 8982edb) — tm dep rm silently does nothing: emit ticket.dependency_removed
  model: sonnet · severity: critical · builds Rust: yes · area: cli/tickets · deps: s1-surfaces-decision-doc
  files: `crates/tm-core/src/store.rs`, `crates/tm-cli/src/tickets.rs`
  change: `dep_rm` in tickets.rs (~line 835) writes a `dependencies` field through `update_ticket`, but materialize never reads that field — edges live in `ticket_deps` and are only removed by the TicketDependencyRemoved event (materialize.rs:210, payload TicketDependencyRemovedPayload). Add `Store::remove_dependency(ticket, depends_on, actor)` next to `add_dependency` (store.rs:728); it returns not_found when the edge is absent and otherwise emits TicketDependencyRemovedPayload. Rewrite `dep_rm` to call it. Add a store unit test (add, rm, then view.graph has no edge).
  acceptance: In a tempdir project: `tm dep add T-2 T-1`, `tm dep rm T-2 T-1`, then `tm --json dep graph` lists no T-2->T-1 edge. Running rm a second time gives a clear "no dependency T-2 -> T-1" error.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`
  evidence: `sed -n 847,872p crates/tm-cli/src/tickets.rs` -> `let fields = serde_json::json!({"dependencies": updated_edges}); project.store.update_ticket(&ticket, fields, ...)` (materialize only deletes on EventKind::TicketDependencyRemoved). Confirmed independently: "tm dep rm T-2 T-1 --plain followed by tm dep graph --json still shows the edge."

- [x] **s1-display-labels** (landed 8c01ef5) — One set of human display labels for ticket state, kind, milestone state, dep kind, budget and authority
  model: sonnet · severity: critical · builds Rust: yes · area: cli · deps: s1-surfaces-decision-doc
  files: `crates/tm-cli/src/render.rs`, `crates/tm-cli/src/tickets.rs`
  change: Add `pub fn state_label(TicketState)`, `kind_label`, `milestone_state_label`, `dep_kind_label` (Hard -> "blocks", Loop -> "loop"), `budget_label` ("unlimited" for u64::MAX, else e.g. "50k tokens, 20 steps, $2.00") and `authority_label` in render.rs. Labels are lowercase and match exactly what `tm ticket list --state` parses (tickets.rs:590). Replace `format!("{:?}", t.kind/t.state)` at tickets.rs:336-337, 654, and the milestone list (~941). Add a test that feeds every TicketState's label back through the `--state` parser.
  acceptance: `tm ticket list` shows `draft`/`ready`/`in progress`-style lowercase states. `tm milestone list` shows `open`, not `Open`. Every label round-trips through `--state`.
  test: `mise run test:crate -- tm-cli`
  evidence: `grep -n '{:?}' crates/tm-cli/src/tickets.rs` -> 336/337/941 debug-format State/Kind/milestone state.

- [x] **s1-ticket-show-human** (landed 2eb53da) — tm ticket show prints Rust Debug structs; render a readable summary
  model: sonnet · severity: critical · builds Rust: yes · area: cli/tickets · deps: s1-display-labels
  files: `crates/tm-cli/src/tickets.rs`
  change: Rewrite the text branch of `ticket show` (tickets.rs:368-384). Show State, Kind, Objective, Milestone, Depends on, Children as `T-4, T-5` (or `none`), Budget via budget_label, Authority via authority_label. Collapse Resources, Executor, Verification and Retry Policy to one plain line each (e.g. "Verification: tests must pass", "Retries: up to 3 attempts"), or omit when default. JSON output unchanged except budget renders as null/"unlimited" in text only. Also fix the objective-truncation byte-slice at tickets.rs:327-328 to use `.chars().take(47)` instead of `&[..47]`, which panics on multi-byte UTF-8 (emoji/accents). Add a unit test asserting no `{`, `Some(` or `18446744073709551615` in the output, and a test that a multi-byte objective doesn't panic.
  acceptance: `tm ticket show T-1` in a tempdir project has no braces, `Some(` or u64::MAX; lines read like `Budget: unlimited`. `tm ticket new "😀😀😀…30 chars"` then `tm ticket list` doesn't panic.
  test: `mise run test:crate -- tm-cli`
  evidence: `grep -n 'Authority:\|Budget:' crates/tm-cli/src/tickets.rs` -> 378/382 debug format; multiple independent trial agents confirmed the same `Authority { repository: RepoAuthority { ... } }` output; byte-slice truncation confirmed at tickets.rs:327-328.

- [x] **s1-transition-error-copy** — Invalid ticket transitions explain the state and the next command (landed 4a1bfb5)
  model: sonnet · severity: high · builds Rust: yes · area: cli/tickets · deps: s1-display-labels
  files: `crates/tm-cli/src/tickets.rs`, `crates/tm-types/src/error.rs`, `crates/tm-core/src/machine.rs`
  change: `InvalidTransition { from, trigger }` (tm-core/src/machine.rs:16) carries no ticket id, so build the friendly text in the CLI layer: a helper in tickets.rs mapping a TmError::InvalidTransition from accept/reject/retry/activate/close/cancel/reopen/submit to a sentence like "T-3 is a draft, so it can't be accepted. Activate it first: tm ticket activate T-3", using state_label plus a small table of which verb is valid from each state. Change the `#[error("invalid transition: {0}")]` prefix (error.rs:25) to "can't do that from this state: {0}". Cover submit-without-evidence ("invariant violated: submission requires at least one evidence artifact") with "Provide at least one piece of evidence (code changes, test results, or documentation) with `tm ticket attach T-N --evidence <path>` before submitting."
  acceptance: `tm ticket accept T-1` on a draft prints a sentence naming the state and the next command; no `InvalidTransition`, `trigger`, `invariant violated`, or raw enum Debug (e.g. "Ready on Activate") anywhere in the output.
  test: `mise run test:crate -- tm-cli && mise run test:crate -- tm-core`
  evidence: `grep -n 'invalid transition' crates/tm-types/src/error.rs` -> 25: `#[error("invalid transition: {0}")]`; five independent trial agents hit the same "no transition from Draft on VerificationStarted"/"Ready on Activate" text.

- [x] **s1-dep-graph-human** — tm dep graph prints DependencyGraph Debug and ignores its root argument (landed 00d41cf)
  model: sonnet · severity: high · builds Rust: yes · area: cli/tickets · deps: s1-display-labels, s1-dep-rm-emit-removed-event
  files: `crates/tm-cli/src/tickets.rs`, `crates/tm-cli/src/tui.rs`
  change: In `dep_graph` (tickets.rs ~884-905), replace `{:?}` of view.graph with an indented, human list: each edge as `T-A (objective) -> T-B (objective)` using dep_kind_label, sourced from view.tickets. When a TICKET argument is given, filter to only its transitive dependencies/dependents (today the code computes a subgraph label but still returns the full graph). Validate the root ticket exists (TmError::not_found otherwise). With no edges: "No dependencies yet. Add one: tm dep add <ticket> <depends-on>". Fix the Kanban column title at tui.rs:337 (`format!("{state:?}")`) to use state_label.
  acceptance: `tm dep graph T-2` prints only T-2's subgraph as readable lines, no Rust struct syntax. `tm dep graph T-99` errors "not found: ticket T-99" instead of returning the full graph. Kanban columns read `ready`, not `Ready`.
  test: `mise run test:crate -- tm-cli`
  evidence: `sed -n 896,904p crates/tm-cli/src/tickets.rs` -> `format!("Dependency graph (subgraph from {}):\n{:?}", ticket, view.graph)`; confirmed `tm dep graph T-2` still returns the unfiltered graph.

- [x] **s1-milestone-new-show** (landed 047afea) — Add tm milestone new/show and validate --milestone on ticket new
  model: sonnet · severity: critical · builds Rust: yes · area: cli/milestones · deps: s1-display-labels
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/tickets.rs`, `CLAUDE.md`
  change: Add `MilestoneCommand::New { title, --ticket <T>... }` with `create` as a visible alias, calling the existing `Store::create_milestone` (store.rs:1694) and printing "Created M-1: <title>". Add `Show(MilestoneRefArgs)` printing title, state and member tickets with state labels plus a done/total count. When `milestone list` is empty: "No milestones yet. Create one: tm milestone new \"<title>\"". Make `tm ticket new --milestone M-9` fail with "no milestone M-9. Run `tm milestone list`..." when M-9 doesn't exist (today it's silently dropped). Add a CLAUDE.md line.
  acceptance: In a tempdir: `tm milestone new "v0" --ticket T-1` then `tm milestone show M-1` lists T-1. `tm ticket new x --milestone M-99` errors clearly instead of silently creating the ticket with no milestone.
  test: `mise run test:crate -- tm-cli`
  evidence: `grep -n 'List\|Close\|Reopen'` in MilestoneCommand (args.rs:535-541) -> only List/Close/Reopen, no New/Create/Show; `Store::create_milestone` exists at store.rs:1694 with no CLI caller; confirmed `tm ticket new "..." --milestone M-1` with no milestones creates the ticket and silently drops the milestone.

- [x] **s1-help-text-scrub-and-hygiene** (landed a9dedde) — Strip D-NNN, crate paths and type names from user-facing help, and add a hygiene check for it
  model: sonnet · severity: high · builds Rust: yes · area: cli/copy · deps: s1-milestone-new-show
  files: `crates/tm-cli/src/args.rs`, `crates/xtask/src/hygiene.rs`
  change: Rewrite every `///` doc comment on clap items in args.rs mentioning `D-0NN`, `crates/`, `docs/decisions`, `tm_*::`, backticked crate names, or jargon ("honestly refused", "bare-`tm` loop", "snapshot-tested", "assimilate", "chunks"). Move rationale to plain `//` comments for maintainers. E.g. Tickets becomes "Open the tickets view (or print tickets with --json)"; the `--project` flag drops `[crate::project::resolve_scope]`/`[crate::project::locate]`/"(unchanged since before D-003)" for plain English ("walking up for a `.tm` directory, then falling back to a project kept under $TM_HOME"); `--plain` drops "(D-002, ...)"; `--json` drops "snapshot-tested"; `tm computer`'s `--headless` help changes "honestly refused on macOS" to "not supported on macOS" (4 occurrences), and its module doc drops the backticks around `tm-computer`. Add a hygiene check flagging `D-\d{3}|crates/|docs/decisions|tm_[a-z]+::` inside `///` lines of crates/tm-cli/src/args.rs only.
  acceptance: `tm --help` and every `tm <verb> --help` contain no D-0, crates/ or `tm_` paths. `mise run hygiene` fails when one is reintroduced.
  test: `mise run hygiene && mise run test:crate -- tm-cli`
  evidence: `grep -nE 'D-0[0-9]{2}' crates/tm-cli/src/args.rs` -> 114, 324, plus `--project`'s rustdoc-style `[crate::project::resolve_scope]`/`[crate::project::locate]` links, confirmed by seven independent trial agents across different CLI verbs; `honestly refused on macOS` at 4 sites in ComputerSnapshotArgs/ComputerClickArgs/ComputerTypeArgs/ComputerKeyArgs.

- [x] **s1-cli-tree-regroup** (landed cddc57f) — Group tm --help into daily/planning/serving/more and hide plumbing verbs as aliases
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: s1-help-text-scrub-and-hygiene
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/tests/tui_launch.rs`, `CLAUDE.md`
  change: Use `display_order` so daily verbs (`tm`, `-p`, `init`, `status`, `tickets`, `ticket`, `run`, `search`, `symbol`, `doctor`) come first, then planning (`milestone`, `dep`, `decision`), then serving (`serve`, `mcp`). Add `#[command(hide = true)]` to `lease`, `harness`, `bench`, `browser`, `computer`, `sched plan/tick`, `events replay/verify`, `ticket submit/delegate` — all must still run. Add an `after_help` "More commands" list. Give `tm search` `--exact`/`--semantic` flags; keep `--mode` as a hidden alias. Don't touch `provider` or `auth`.
  acceptance: `tm --help` lists ~16 commands, grouped. `tm lease list` and `tm sched tick` still work. `tm search --exact foo` equals `tm search --mode exact foo`.
  test: `mise run test:crate -- tm-cli`
  evidence: current CLI has 31 top-level verbs, all visible; finding: "Search command UX: --mode flag less discoverable than subcommand style."

- [x] **s1-tickets-json-shape** — tm tickets --json and tm ticket list --json disagree on fields (landed 1b55c83)
  model: haiku · severity: medium · builds Rust: yes · area: cli · deps: s1-display-labels
  files: `crates/tm-cli/src/tickets/overview.rs`, `crates/tm-cli/src/tickets.rs`
  change: Make `tm tickets --json` serialize the same view struct as `tm ticket list --json`, including `title` (short_title of the objective, overview.rs:531) and state_label strings. Extend whichever struct is missing fields rather than creating a third one. Add a test deserializing both outputs into one struct.
  acceptance: `tm tickets --json | jq '.[0]|keys'` and `tm --json ticket list | jq '.[0]|keys'` have identical key sets, and `title` is populated (not null) in both.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm tickets --json` shows `"title": "Fix the login redirect"` while `tm ticket list --json` shows `"title": null` for the same ticket; overview.rs:69-70 `pub title: String` only exists on the overview view.

- [x] **s1-ticket-due-date** — Tickets get an optional due date (tm ticket new/edit --due) (landed c841058)
  model: sonnet · severity: medium · builds Rust: yes · area: cli/tickets · deps: s1-cli-tree-regroup
  files: `crates/tm-core/src/ticket.rs`, `crates/tm-core/src/materialize.rs`, `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/tickets.rs`, `CLAUDE.md`
  change: Add `due: Option<chrono::NaiveDate>` (serde default) to Ticket (near `milestone`, ticket.rs:367). Persist through the existing ticket.created/ticket.updated field paths, migrating materialize.rs if tickets are stored per column. Add `--due YYYY-MM-DD` to `ticket new`/`ticket edit` (`--due none` clears it). Show it in `ticket show`/`ticket list`. A milestone's due date is the max of its tickets' due dates in `milestone show`. Parse errors say "use YYYY-MM-DD, e.g. 2026-10-01".
  acceptance: `tm ticket new x --due 2026-10-01` then `tm ticket show T-1` shows `Due: 2026-10-01`, surviving a replay/view rebuild.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`
  evidence: `grep -n 'due' crates/tm-types/src` -> no matches; probe deps-sched: "Add due-date field and CLI support to tickets."

- [x] **s1-tui-hub-tabs** (landed 8ce1818) — Tickets hub gets a tab strip: Tickets, Board, Milestones, Timeline, Graph
  model: sonnet · severity: high · builds Rust: yes · area: tui · deps: s1-dep-graph-human
  files: `crates/tm-tui/src/screens/tickets.rs`, `crates/tm-cli/src/tui.rs`, `crates/tm-cli/src/tui/tickets_view.rs`, `crates/tm-cli/tests/tui_navigation.rs`
  change: Render a tab strip in the tickets screen header. Tab/Shift+Tab (currently ignored at tickets.rs:786) cycle ScreenId among Tickets, Kanban, Milestones, Timeline, Graph. The last three show a "coming next" placeholder until their own tasks land. Ctrl+B still jumps to Board; Esc from any tab goes back to chat. Fix the stale module doc at tui.rs:18 ("`b` opens the Kanban board"; it is actually Ctrl+B). Extend tui_navigation.rs with Tab-cycling coverage.
  acceptance: In `mise run tui` (tempdir project), `/tickets` then Tab moves the highlighted tab and the screen. The navigation test passes.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `grep -n 'Tab' crates/tm-tui/src/screens/tickets.rs` -> 786: `KeyCode::Tab | KeyCode::BackTab => {}`; ScreenId (tui.rs:405) has only Chat/Tickets/Kanban/Detail; tester (TUI): "missing the project-management views (milestones, timeline, calendar)."

- [x] **s1-tui-milestones-view** (landed 9d68e8c) — Milestones tab: progress per milestone, Enter filters tickets
  model: sonnet · severity: medium · builds Rust: yes · area: tui · deps: s1-tui-hub-tabs, s1-milestone-new-show, s1-ticket-due-date
  files: `crates/tm-tui/src/screens/milestones.rs`, `crates/tm-tui/src/screens/mod.rs`, `crates/tm-cli/src/tui.rs`
  change: New screen, one row per milestone: title, state label, a done/total progress bar, derived due date, sourced from ProjectView.milestones. Enter returns to Tickets filtered to that milestone (header shows the filter); Esc clears it. Empty state: "No milestones. Create one with tm milestone new". Add a buffer-render unit test at the bottom of the file.
  acceptance: With two milestones in a tempdir project, the Milestones tab shows both with correct counts; Enter narrows the Tickets list.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `ls crates/tm-tui/src/screens` -> no milestone screen exists yet.

- [x] **s1-tui-timeline-view** (landed 08efa61) — Timeline tab: ticket bars from the event log, today line, due markers, month grid
  model: sonnet · severity: medium · builds Rust: yes · area: tui · deps: s1-tui-hub-tabs, s1-ticket-due-date
  files: `crates/tm-tui/src/screens/timeline.rs`, `crates/tm-tui/src/screens/mod.rs`, `crates/tm-cli/src/tickets/overview.rs`, `crates/tm-cli/src/tui.rs`
  change: Ticket has no created/closed timestamps, so extend the existing event fold in tickets/overview.rs (`apply(kind, subject, ts, payload)`, line 220) to record first-seen and closed-at per ticket. Render one bar per ticket from created to closed (or now), grouped by milestone, with a today line and a due-date mark. `+`/`-` zoom between day/week/month; month zoom is the calendar grid (due tickets listed per day). Timeline has no text input so plain keys are safe. Use a fixed clock for unit tests.
  acceptance: A render test with three tickets at fixed timestamps shows bars in the right columns with a due marker; the Timeline tab shows real tickets in a tempdir project.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `grep 'created_at|closed_at' crates/tm-core/src/ticket.rs` -> none; overview.rs:220 already folds events with timestamps.

- [x] **s1-tui-graph-tab-prune-dead-screens** — Wire the unused ticket_graph screen as the Graph tab; delete dead dashboard/command_palette (landed 7b1f642)
  model: sonnet · severity: medium · builds Rust: yes · area: tui · deps: s1-tui-hub-tabs
  files: `crates/tm-tui/src/screens/ticket_graph.rs`, `crates/tm-tui/src/screens/dashboard.rs`, `crates/tm-tui/src/screens/command_palette.rs`, `crates/tm-tui/src/screens/mod.rs`, `crates/tm-tui/src/screens/kanban.rs`, `crates/tm-tui/src/screens/ticket_detail.rs`, `crates/tm-cli/src/tui.rs`
  change: Feed `screens/ticket_graph.rs` from view.graph with labelled nodes and make it the Graph tab; Enter on a node opens ticket detail. Delete `dashboard.rs` and `command_palette.rs` (grep shows no users outside their own files besides doc links in kanban.rs:5,37 and ticket_detail.rs:121 — fix those doc references too). Re-grep `dashboard\|command_palette` across `crates` once more right before deleting.
  acceptance: The Graph tab shows the dependency edges of a tempdir project; dashboard.rs and command_palette.rs are gone with no dangling doc links, and the workspace builds.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `grep -rn 'dashboard::\|ticket_graph::\|command_palette' crates` (excluding own files) -> only doc comments in kanban.rs:5 and ticket_detail.rs:121; ScreenId has no Graph variant.

- [x] **s1-slash-pm-views** — Slash commands /board /milestones /timeline /deps /ticket /run (landed 132369d)
  model: sonnet · severity: medium · builds Rust: yes · area: tui/chat · deps: s1-tui-milestones-view, s1-tui-timeline-view, s1-tui-graph-tab-prune-dead-screens
  files: `crates/tm-tui/src/chat/commands.rs`, `crates/tm-cli/src/tui/slash_views.rs`, `crates/tm-cli/src/tui/chat_ops.rs`, `crates/tm-cli/src/tui.rs`, `CLAUDE.md`
  change: This is the first of four chained slash tasks that all edit commands.rs — run them in order. Add CommandIds/COMMANDS rows for board, milestones, timeline, deps, `ticket <T>` (prints ticket-show text inline in the transcript) and `run <T>` (activates and queues the ticket, replying "Queued T-3; watch it in /tickets"). Put handlers in a new file, slash_views.rs; keep each arm in chat_ops.rs's match (~line 582) to one line. chat_ops.rs has uncommitted edits in the (untouched, per CLAUDE.md) `odw-integrate` worktree, so keep that diff minimal — this task must not touch `.claude/worktrees/odw-*`. Any count test must use `COMMANDS.len()`. Update CLAUDE.md's slash list.
  acceptance: Typing `/mil` in the chat shows /milestones in the popup, and Enter opens the Milestones tab. `/ticket T-1` prints a readable summary.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: `crates/tm-tui/src/chat/commands.rs` CommandId has 17 variants (Help..Exit) with no PM views, no /ticket and no /run.

- [x] **s1-slash-context-todos** (landed 144e269; note: landed ahead of its declared dep
  s1-slash-pm-views, which hadn't landed in this worktree yet — CommandId/COMMANDS gained
  Context/Todos as new variants appended to the existing list, so a later s1-slash-pm-views merge
  should be a straightforward textual conflict, not a semantic one) — /context shows token use by
  section and the ticket's prefetched context pack; /todos
  model: sonnet · severity: high · builds Rust: yes · area: tui/chat · deps: s1-slash-pm-views
  files: `crates/tm-tui/src/chat/commands.rs`, `crates/tm-cli/src/tui/slash_views.rs`, `crates/tm-cli/src/tui/chat_ops.rs`, `CLAUDE.md`
  change: `/context` prints a table into the transcript: context-window size, and tokens used by system prompt, instructions (AGENTS.md), tools, conversation, free space, from the session's real token counts. When a ticket is attached, also list that ticket's context-pack sections (prefetched files/symbols) with a token count each, reusing tm-context's section accounting (`crates/tm-context/src/tokens.rs` SectionKind) — don't invent numbers. `/todos` toggles the Ctrl+T checklist.
  acceptance: In a chat attached to a ticket, `/context` lists the prefetched items with token counts that sum to the displayed total.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: probe prefetch (pass=false): "Add a way to see a ticket's context pack (what was prefetched) and its token cost."

- [x] **s1-slash-search-review** (landed 2e0c21a) — /search <q> inline hybrid code search, and /review of the working-tree diff
  model: sonnet · severity: medium · builds Rust: yes · area: tui/chat · deps: s1-slash-context-todos
  files: `crates/tm-tui/src/chat/commands.rs`, `crates/tm-cli/src/tui/slash_views.rs`, `crates/tm-cli/src/tui/chat_ops.rs`, `CLAUDE.md`
  change: `/search <q>` runs the same hybrid search `tm search` uses (crates/tm-cli/src/search.rs) against the project and prints the top 10 as `path:line  snippet`. When the index is not built yet, say so instead of printing an empty list (distinguish "not indexed" from "no matches" — same fix needed in s1-search-hybrid-empty-snippet's snippet plumbing). `/review [focus]` sends a turn asking the agent to review `git diff HEAD`, the way `/init` sends init_prompt.
  acceptance: `/search dependency graph` in a tempdir project with code lists matching paths with snippets. `/review` starts a turn whose prompt includes the diff.
  test: `mise run test:crate -- tm-cli`
  evidence: probe nav-semantic: "Distinguish 'index not built yet' from 'no matches' in tm search output"; the chat has no search command today.

- [x] **s1-slash-memory-export-doctor-perms-workflow** — /memory, /export, /doctor, /permissions, /workflow (landed 75bc2dd)
  model: sonnet · severity: medium · builds Rust: yes · area: tui/chat · deps: s1-slash-search-review, s1-surfaces-decision-doc
  files: `crates/tm-tui/src/chat/commands.rs`, `crates/tm-cli/src/tui/slash_views.rs`, `crates/tm-cli/src/tui/chat_ops.rs`, `CLAUDE.md`, `docs/decisions/D-0NN-command-surfaces.md`
  change: `/memory` opens the project's AGENTS.md in $EDITOR through the existing Ctrl+G editor path (creating it if absent). `/export [path]` writes the transcript as markdown (default `tm-session-<id>.md` in cwd) and replies with the path. `/doctor` runs the same checks as `tm doctor`, printed pass/fail. `/permissions` shows the current auto/plan/ask mode and what each allows; `/permissions <mode>` sets it, same as Shift+Tab. `/workflow [name]` with no name lists workflows like `tm workflow list`; with a name, starts it as a background ticket. Mark the command-surfaces decision doc "Implemented" for the slash table.
  acceptance: Each command appears in the `/` popup and does what it says in a tempdir project. `/export` writes a readable .md file.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: finding (workflow): "Add /workflow slash command to chat for discovering and running workflows"; CommandId lacks Memory/Export/Doctor/Permissions/Workflow.

- [x] **s1-id-parse-and-lease-error-copy** — Plain-language errors for milestone/ticket/decision/lease ID parsing and lease conflicts (landed e4a94ac)
  model: haiku · severity: low · builds Rust: yes · area: cli/copy · deps: s1-milestone-new-show
  files: `crates/tm-types/src/id.rs`, `crates/tm-core/src/lease.rs`, `crates/tm-cli/src/tickets.rs`
  change: In the `id_newtype!` macro (id.rs ~109-114), replace the internal type name in the format string with a user-friendly noun per type ("Ticket identifier", "Milestone identifier", "Decision identifier", "Lease identifier", "Actor") so e.g. `tm decision show invalid-id` says "Decision identifier must look like D-<n>, got \"invalid-id\"" instead of "DecisionId must look like...". Do the same for ParticipantId/LeaseId's format-string errors ("lease IDs start with L- followed by 12 hex digits" instead of dumping the pattern). Replace the lease-acquire-on-already-leased error (currently AcquireError::NotReady, displaying as "ticket T-3 is not Ready" even when the real cause is an active lease) with "T-3 is already being worked by <holder> until <time>" — check for an existing lease before the Ready check, or special-case the message.
  acceptance: `tm ticket show foo` and `tm decision show foo` print one plain sentence with an example id and no Rust type names. `tm lease acquire <already-leased-ticket>` names the existing lease holder, not "not Ready".
  test: `mise run test:crate -- tm-types && mise run test:crate -- tm-core && mise run test:crate -- tm-cli`
  evidence: findings: "Invalid decision ID error shows internal type name instead of plain language" (`error: parse: DecisionId must look like D-<n>, got "invalid-id"`); "Technical format strings in ID validation error messages" (LeaseId/ParticipantId); lease: `tm lease acquire T-1 --actor agent:mock/worker-2` on an already-leased ticket returns `error: conflict: ticket T-1 is not Ready`.

- [x] **s1-status-since-hours-error** (landed 84fa0b7) — tm status --since-hours rejects bad input with a Rust parse error
  model: haiku · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/args.rs`
  change: Replace the default u64 parser for `since_hours` with a custom clap `value_parser` that validates a positive u64 and returns "Expected a positive integer (hours)" on failure, instead of surfacing the raw Rust parse error.
  acceptance: `tm status --since-hours invalid` prints "Expected a positive integer (hours)" (or equivalent plain text), not "invalid digit found in string".
  test: `mise run test:crate -- tm-cli`
  evidence: `tm status --since-hours invalid` -> `error: invalid value 'invalid' for '--since-hours <HOURS>': invalid digit found in string`.

- [x] **s1-events-sched-copy-and-quiet** (landed a3a30b2) — Humanize tm events show output and fix sched plan/tick's --quiet and event-name jargon
  model: sonnet · severity: high · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/sched.rs`
  change: In `tm events show`, change `format!("{:?}", event.kind)` (ops.rs ~2248) to the Display impl (dotted event names like `ticket.leased` instead of debug `TicketLeased`), and humanize field labels ("Sequence"/"Type"/"Related to"/"When" instead of "Seq"/"Kind"/"Subject"/"Timestamp"). In sched.rs, add `if !renderer.is_quiet()` guards before `renderer.emit()` in `sched_plan` (~line 90) and `sched_tick` (~line 117), matching the pattern already used in `sched_run` (~line 185). Reword `event_to_summary` (sched.rs:728-755): 'Ticked' -> 'Scheduler ticked', with detail "No work to do" (0 actions), "1 action queued", or "N actions queued" instead of "N actions planned at <ISO timestamp>".
  acceptance: `tm events show 1` prints `Type: ticket.leased`, not `Kind: TicketLeased`. `tm sched plan --quiet`/`tm sched tick --quiet` produce no output on success. `tm sched tick` reads "Scheduler ticked: No work to do right now" instead of "Ticked: 0 actions planned at 2026-...".
  test: `mise run test:crate -- tm-cli`
  evidence: `tm events show 1` -> `Kind: TicketLeased` (should be `ticket.leased`); `tm sched tick --quiet` still prints `Ticked: 0 actions planned at 2026-09-23T21:01:58.400868Z`.

- [x] **s1-search-hybrid-empty-snippet** (landed 5c0ae42) — Hybrid search results show an empty snippet column
  model: haiku · severity: medium · builds Rust: yes · area: cli/search · deps: none
  files: `crates/tm-cli/src/search.rs`, `crates/tm-codeintel/src/api.rs`, `crates/tm-codeintel/src/hybrid.rs`
  change: Hybrid search's human output shows an empty Snippet column (JSON confirms `"snippet": ""` always). Exact/regex modes properly show the matched line (`h.line_text`). Fix hybrid to extract the snippet from the hit data or source file at the matched line, same pattern as exact mode (search.rs ~line 180).
  acceptance: `tm search --mode hybrid "println"` shows code text in the Snippet column, matching what exact mode shows for the same hit; JSON output's `snippet` field is non-empty.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm search --mode hybrid "println"` -> Location/Score/Snippet with an empty Snippet column, while `tm search --mode exact "println"` shows the code.

- [x] **s1-symbol-error-exit-codes** — Symbol lookups fail silently: "Symbol not found" exits 0 (landed 3ae3537)
  model: haiku · severity: medium · builds Rust: yes · area: cli/symbol · deps: none
  files: `crates/tm-cli/src/search.rs`
  change: In symbol_def/refs/callers/callees (search.rs ~384/429/473/519), replace "Symbol not found"/"No outline entries found" with "No symbol named `{name}` in this project", and exit 1 instead of 0. In symbol_outline (~545), distinguish "File {path} not found" from "File {path} contains no top-level definitions" and exit 1 in both cases. --json output keeps the same exit-1 behavior with an empty array/null.
  acceptance: `tm symbol def nonexistent_fn` prints the plain message and exits 1. `tm symbol outline no/such/file.rs` distinguishes missing-file from no-definitions and exits 1.
  test: `mise run test:crate -- tm-cli`
  evidence: "Symbol not found" and "No outline entries found" both exit 0 in four separate trial runs across the symbol/init/attach groups.

- [x] **s1-decision-supersedes-and-list-copy** — Superseding decision loses the supersedes link; decision list shows the wrong column (landed 666a113)
  model: sonnet · severity: high · builds Rust: yes · area: cli/decision · deps: none
  files: `crates/tm-events/src/payload.rs`, `crates/tm-core/src/store.rs`, `crates/tm-core/src/materialize.rs`, `crates/tm-cli/src/tickets.rs`
  change: Add `supersedes: Option<DecisionId>` to `DecisionCreatedPayload`; populate it in `store.supersede()` and INSERT the real value in materialize (instead of always NULL). Also fix `decision_list()`'s Summary column, which renders `d.subject` (the semantic class, e.g. "decision") instead of `d.decision` (the actual decision text) — use `d.decision`, truncated to 40 chars as before.
  acceptance: `tm decision supersede D-001 'Use MongoDB'` creates D-002; `tm decision show D-002` (text and `--json`) shows "Supersedes: D-001" / `"supersedes": "D-001"`. `tm decision list` shows the decision text in Summary, not "decision".
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`
  evidence: after `decision supersede D-001 'Use MongoDB'`, both text and JSON `decision show D-002` omit D-001, and the DB stores `supersedes=NULL`; `tm decision list` shows `D-001  Superseded  decision` / `D-002  Active  decision` instead of the actual text.

- [x] **s1-workflow-error-message-ux** (landed 35d6b3e) — Workflow-not-found error dumps an internal filesystem path
  model: haiku · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/workflow.rs`
  change: Replace the raw io-error message on a missing workflow definition ("Failed to read workflow \"<name>\" at /path/...: No such file or directory (os error 2)") with "Workflow '<name>' not found. Define it in .tm/workflows/<name>.toml".
  acceptance: `tm workflow show nonexistent` prints the plain sentence, with no filesystem path or "os error" text.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm workflow show nonexistent` -> `error: storage: Failed to read workflow "nonexistent" at /path/.../nonexistent.toml: No such file or directory (os error 2)`.

- [x] **s1-mirror-status-and-push-clarity** (landed d78bc0e) — mirror status hides configured adapters; push/pull give no reason for a zero count; link doesn't warn on unset credentials
  model: sonnet · severity: high · builds Rust: yes · area: cli/mirror · deps: none
  files: `crates/tm-cli/src/ops.rs`
  change: `tm mirror status` reports only ticket-level external-id links, so after `tm mirror link github` it still says "No active mirror links" even though `mirror.toml` has a real, enabled adapter — add an adapters section to status output (JSON too) showing each configured adapter and its enabled state, distinct from ticket sync status. Make `mirror push`/`mirror pull`'s "0 pushed, 0 degraded" output name the reason (no mirrors configured vs no tickets eligible vs all already synced) instead of a bare count. In `mirror link`, check whether the env vars referenced by `--credential field=ENV_VAR` are actually set, and if not, print a non-fatal warning ("credential field \"owner\" references env var TM_GITHUB_OWNER which is not set") so the misconfiguration surfaces at link time, not first at push time.
  acceptance: After `tm mirror link github` with no tickets synced, `tm mirror status` shows the github adapter as configured. `tm mirror push` with nothing to do explains why. `tm mirror link github --credential owner=TM_GITHUB_OWNER` with TM_GITHUB_OWNER unset prints a warning naming the unset var, and still succeeds.
  test: `mise run test:crate -- tm-cli`
  evidence: after linking github, `tm mirror status` still returns `{"mirrors": []}` and "No active mirror links" despite `mirror.toml` containing `[adapters.github]` with `enabled=true`; `mirror push` prints "Mirror push completed: 0 pushed, 0 degraded" with no explanation; `mirror link` succeeds silently with unset `TM_GITHUB_OWNER`/`TM_GITHUB_REPO`, which only surfaces later as "Skipped adapter \"github\": ... is not set" on push.

- [x] **s1-templates-io-error-message** (landed c52ef07) — Missing template source file shows a raw OS error
  model: haiku · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-templates/src/manifest.rs`
  change: Change the `TmError::Io(format!("reading {}: {e}", manifest_path.display()))` at manifest.rs:63 to "template source not found: {} (manifest.toml)" — no OS error code, no "io:" prefix — matching the style of other not-found errors in this codebase.
  acceptance: `tm templates list` against a missing template directory shows "error: template source not found: /path/to/template (manifest.toml)", no OS error code.
  test: `mise run test:crate -- tm-templates`
  evidence: `tm templates list` with a missing template dir -> `error: io: reading /path/to/missing-template/manifest.toml: No such file or directory (os error 2)`.

- [x] **s1-provider-project-quiet-flags** — provider and project commands ignore --quiet (landed c98c1c7)
  model: haiku · severity: high · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/project.rs`
  change: `provider_list`, `provider_detect`, `provider_status` and `provider_test` print their table unconditionally; wrap the table emission with `if !renderer.is_quiet()`, only suppressing `provider test`'s "ok" lines while keeping errors. `project_list()`/`project_show()` use `renderer.emit(&entries, "no global projects yet")`, which always prints the fallback narration regardless of `--quiet` — switch to the pattern used elsewhere (`renderer.emit(&json, "")`) so `--quiet` suppresses narration but not the payload, per render.rs's own documented contract (lines 5-8). While here, reword "no global projects yet" to explain what a global project is in plain terms ("no projects in $TM_HOME yet").
  acceptance: `tm provider list --quiet`, `tm provider detect --quiet`, `tm provider status --quiet`, `tm project list --quiet`, `tm project show --quiet` produce no output on success; `--json` output is unaffected; non-quiet behavior is unchanged.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm provider list --quiet` still prints the full table; `tm project list --quiet` still prints "no global projects yet"; `tm project show --quiet` still prints the full path.

- [x] **s1-harness-missing-file-errors** — tm harness show/set/promote crash on a raw OS error when harness.toml doesn't exist (landed c0a52cd)
  model: haiku · severity: high · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/ops.rs`
  change: `harness_show` (~line 1130), `harness_set` (~1162) and `harness_promote` (~1325) all propagate "storage: Failed to read harness.toml: No such file or directory (os error 2)" instead of handling the missing-file case, unlike `config_cmd.rs::load_harness_config` (~line 200) which already checks `!path.is_file()` and returns `HarnessConfig::default()`. Make `harness_show` follow that same pattern (display defaults). Make `harness_set` use `HarnessConfig::default()` as its base when the file is missing, so it can bootstrap a fresh harness.toml with one key set. Make `harness_promote` give a clear message instead ("No harness configuration found. Use `tm harness set <key> <value>` to create one before promoting an epoch."), since promote has no sensible default to act on.
  acceptance: `tm harness show` on a fresh project prints default config in TOML, not an OS error. `tm harness set routing_weights.recency 0.5` on a fresh project succeeds and creates harness.toml. `tm harness promote 1 --force` on a fresh project explains it needs a harness config first.
  test: `mise run test:crate -- tm-cli`
  evidence: all three commands currently return `error: storage: Failed to read harness.toml: No such file or directory (os error 2)` on a freshly-initialized project.

- [x] **s1-bench-compare-human-output** — tm bench compare's human output omits the actual comparison (landed 20d2733)
  model: haiku · severity: high · builds Rust: yes · area: cli/bench · deps: none
  files: `crates/tm-cli/src/ops.rs`
  change: `bench_compare` (ops.rs:1672-1694) computes a `PromotionReport` with `aggregate_gain`, `candidate_improved` and per-task deltas, but the human-readable branch (~1688-1691) only prints epoch numbers ("Comparison: X vs Y"). Render whether the candidate improved (yes/no), the aggregate gain as a signed delta, and a brief per-task summary (or at least an improved/regressed count) when `task_deltas` is non-empty. The JSON output already has everything; just expose it in text.
  acceptance: `tm bench compare a.json b.json` shows candidate-improved status and the aggregate score delta in plain text, not just epoch numbers.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm bench compare report1.json report2.json` -> "Comparison: 0 vs 0" while the JSON shows `{"aggregate_gain": 0.0, "candidate_improved": false, "task_deltas": [["hello-world", 0.0]]}`.

- [x] **s1-browser-toml-error-message** — Missing browser.toml error teaches TOML syntax instead of pointing at docs (landed 3e7e17c)
  model: haiku · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/drive.rs`
  change: Replace the current message ("add a [managed] table with `version` and `sha256`, or a [remote_cdp] table with `ws_url`, and list the ones you configure in `fallback_order`") with something that names the file and points at documentation instead of teaching TOML inline: "Create a browser.toml file with a provider configuration. See SPEC.md §19.1a for examples (managed: pinned Chrome download, remote-cdp: existing browser endpoint)."
  acceptance: `tm browser open <url>` without browser.toml names the file to create and points to SPEC.md §19.1a, without using `[managed]`/`fallback_order`-style TOML syntax in the error text itself.
  test: manual: `S=$(mktemp -d); export TM_HOME=$(mktemp -d) TM_TEST_MOCK_PROVIDER=1; cd $S && git init -q && git config user.email t@t && git config user.name t && echo x>r && git add r && git commit -qm x && tm init -q && tm browser open https://example.com`
  evidence: current error: "not found: browser.toml /path/to/browser.toml; add a [managed] table with `version` and `sha256`, or a [remote_cdp] table with `ws_url`, and list the ones you configure in `fallback_order`".

- [x] **s1-mcp-exit-code-on-parse-error** (landed ed2a8c9) — tm mcp exits 0 on a JSON-RPC parse error / EOF
  model: haiku · severity: high · builds Rust: yes · area: cli/mcp · deps: none
  files: `crates/tm-mcp/src/main.rs`
  change: When `tm mcp` hits EOF or invalid JSON on stdin before handling any successful JSON-RPC call, it prints an error but exits 0, breaking script error handling. Exit 1 in that case.
  acceptance: `echo '' | tm mcp` and `echo 'invalid' | tm mcp` both exit non-zero.
  test: `echo '' | tm mcp; test $? -ne 0`
  evidence: `echo '' | tm mcp` prints "error: parse: EOF while parsing a value at line 1 column 0" and exits 0.

- [x] **s1-attach-output-copy-and-dedup** — tm attach output uses unexplained jargon and double-lists case-variant filenames (landed 69cf4d9)
  model: haiku · severity: medium · builds Rust: yes · area: cli/attach · deps: none
  files: `crates/tm-cli/src/ops.rs`
  change: Replace "indexed 1 files, 1 chunks, 1 commits ingested (symbol graph built: true)" with plain wording, e.g. "Indexed 1 file with 1 commit. Code navigation is ready." Separately, the attach doc-listing table shows both `README.md` and `readme.md` as distinct rows on a case-insensitive filesystem; dedupe by case-insensitive path before printing.
  acceptance: `tm attach .` output has no "chunks"/"symbol graph" jargon. On a project with README.md, the doc table lists it once.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm attach .` -> "indexed 1 files, 1 chunks, 1 commits ingested (symbol graph built: true)"; doc table shows both "README.md  Readme" and "readme.md  Readme" for the same file.

- [x] **s1-io-error-wrapping-and-empty-objective** (landed 42c6968) — Wrap low-level IO/git errors for users; reject empty ticket objectives
  model: sonnet · severity: medium · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/main.rs`, `crates/tm-core/src/history.rs`, `crates/tm-cli/src/tickets.rs`
  change: Wrap the bare IO error path in main.rs so e.g. `tm attach /nonexistent/path` says "The path /nonexistent/path does not exist. Check the path and try again." instead of "error: io: No such file or directory (os error 2)". Wrap git2 NotFound errors at the `tm history why <path>` call site as "File does not exist in repository history."; other git2 errors become "Unable to read file history — repository may be corrupted." (log the raw error via tracing for debugging). Separately, in `ticket_new` (tickets.rs ~392), reject an empty or whitespace-only `--objective`/positional objective with "Objective cannot be empty — describe what the ticket should accomplish." instead of silently creating a blank-objective ticket.
  acceptance: `tm attach /nonexistent/path` gives the plain-English path message. `tm history why nonexistent.txt` gives "File does not exist in repository history.", no git2 class/code numbers. `tm ticket new ''` and `tm ticket new '   '` both fail with the objective error; `tm ticket new 'Valid'` still succeeds.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm attach /nonexistent/path` -> "error: io: No such file or directory (os error 2)"; `tm history why nonexistent.txt` -> "error: storage: git2: the path ... does not exist in the given tree; class=Tree (14); code=NotFound (-3)"; `tm ticket new ''` creates a ticket with an empty objective column.

- [x] **p1-mock-provider-scripted-tool-calls** (landed e939a0a) — Mock provider must script real tool calls so `tm run` can reach `submitted`, not just text replies
  model: sonnet · severity: critical · builds Rust: yes · area: agent/testing · deps: none
  files: `crates/tm-cli/src/agent.rs`, `crates/tm-provider/src/mock.rs`
  change: `build_mock_fabric()` scripts a text-only `CompletionRequest` response (agent.rs ~line 244, `script_default_response(...text: "mock provider: this is a scripted reply...")`), so a worker ticket run under `TM_TEST_MOCK_PROVIDER=1` never calls a tool and the ticket stays `ready` forever instead of reaching `submitted`. Change the default script to a short, deterministic scripted turn that actually calls tools: an edit (`edit.apply_patch` or `make_edit` against a trivial, always-present file/change) followed by `ticket.submit` with at least one evidence artifact (satisfying the submission invariant already enforced elsewhere — see `s1-transition-error-copy`'s evidence note). This is the load-bearing fix that every offline ticket-lifecycle test below depends on; without it, no scripted-provider e2e test can reach `submitted`/`closed`.
  acceptance: In a fresh tempdir project (`TM_TEST_MOCK_PROVIDER=1`), `tm ticket new` + `tm ticket activate` + `tm run <T>` exits 0 and leaves the ticket in `submitted` state (not `ready`/`escalated`); `tm ticket accept <T>` then closes it.
  test_command: `cd /tmp/test-tm && TM_TEST_MOCK_PROVIDER=1 /tmp/tm-wide/tm run T-1 && /tmp/tm-wide/tm tickets --json | jq '.[0].state'`
  evidence: Ran `tm run T-1` (exit 0), but ticket stayed in `ready` state instead of transitioning to `submitted`; no git changes made. Mock provider scripted with a text-only response at `crates/tm-cli/src/agent.rs` line 244.

- [x] **p1-sched-run-failure-message-copy** (landed c3e2ca7) — `tm run`'s failure message debug-prints `FailureClass` and falsely claims auto-retry
  model: haiku · severity: high · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: `run_outcome()` in sched.rs formats failures with `.map(|f| format!("{:?}: {}", f.class, f.detail))` and wraps them as `error: agent turn failed: ticket T-N attempt K did not finish: <Debug>: <detail>; it is Ready again and will be retried` (lines ~597-608). This stacks a generic `error: agent turn failed:` prefix, Debug-prints the `FailureClass` enum (e.g. bare `Other:`), and claims `tm run` auto-retries when it does not — the user must run it again by hand. Add a plain-English `Display`/match for `FailureClass`, drop the `did not finish:`/debug-format wrapping, and replace the false "will be retried" claim with the actual next step: for `Ready`/`Blocked`, "Run `tm run T-N` again to retry."; for `Escalated`, "Run `tm ticket retry T-N --guidance \"...\"` to try again."
  acceptance: A failed `tm run` prints one plain sentence naming the ticket and what went wrong, with no `{:?}`/enum variant name (e.g. `Other`), no stacked `error: X: Y:` prefixes, and no "will be retried" claim — the message names the exact command to run next.
  test_command: `mise run test:crate -- tm-cli`
  evidence: `tm run T-1` (mock failure) -> `error: agent turn failed: ticket T-1 attempt 1 did not finish: Other: model ended turn without submitting; it is ready again and will be retried`.

- [x] **p1-http-error-json-format** (landed 7e89c4e) — HTTP API errors return plain text instead of JSON
  model: sonnet · severity: critical · builds Rust: yes · area: server · deps: none
  files: `crates/tm-server/src/main.rs`, `crates/tm-server/src/routes.rs`
  change: Every HTTP error response (404s, malformed-body validation failures, axum/serde deserialization errors) currently renders as plain text (e.g. axum's default rejection body), while the transition endpoints already return structured JSON errors. Add a shared error-response type/middleware that wraps all error paths — including 404 (bad ticket ID), 400/422 (missing/invalid JSON fields) — as `{"error": "<snake_case_code>", "message": "<plain sentence>"}`. For a malformed `POST /tickets` body, collect and name every missing required field together in one message rather than failing on the first.
  acceptance: `curl -s http://localhost:PORT/tickets/NOTFOUND` returns valid JSON with non-empty `error` and `message` fields (message names the expected ID shape, e.g. "T-<n>"). `curl -s -X POST http://localhost:PORT/tickets -d '{}'` returns JSON naming all missing required fields (`objective`, `kind`, `actor`) together, not just the first one serde chokes on.
  test_command: `mise run test:crate -- tm-server`
  evidence: `curl -s http://localhost:17000/tickets/NOTFOUND` -> plain text `Invalid URL: parse: TicketId must look like T-<n>, V-<n> or A-<n>, got "NOTFOUND"`; `curl -s -X POST http://localhost:17000/tickets -d '{"bad":"request"}'` -> plain text `Failed to deserialize the JSON body into the target type: missing field \`kind\` at line 1 column 18`.

- [x] **p1-mcp-error-and-value-copy** — MCP tool responses leak internal type names, u64::MAX budgets, and silent nulls instead of errors (landed 2d0c6d3)
  model: haiku · severity: medium · builds Rust: yes · area: mcp · deps: p1-http-error-json-format
  files: `crates/tm-mcp/src/server.rs`, `crates/tm-core/src/id.rs`, `crates/tm-types/src/lib.rs`
  change: Four related copy/shape bugs in the same server, worth fixing together since they're all in `crates/tm-mcp/src/server.rs`'s response-building code: (1) `ticket_show`/`ticket_dispatch` serialize an unlimited `Budget` as raw `18446744073709551615` (u64::MAX) instead of `null`/`"unlimited"` — add a custom serializer or map at the response-building call site. (2) `symbol_def` (~line 410) returns `Ok(Value::Null)` for an unknown symbol instead of an MCP error (`isError: true`); change the `None` arm to a proper error result so callers can distinguish "found, empty" from "not found". (3) `symbol_def` (~line 405) uses `format!("{:?}", sym.kind)` for the symbol kind, producing Rust Debug output; map to a clean lowercase string (`function`, `struct`, `module`, etc.). (4) Error messages surfaced through MCP (`crates/tm-core/src/id.rs`, `crates/tm-types/src/lib.rs`) carry internal prefixes and type names, e.g. `parse: TicketId must look like T-<n>...`; strip the `parse:` prefix and the `TicketId` type name so the message reads as plain English (`ticket ID must look like T-123 (T-999 is invalid)`), matching this repo's voice rules.
  acceptance: `ticket_show`'s JSON response shows `null`/`"unlimited"` for an unlimited budget, never `18446744073709551615`. `symbol_def` on a nonexistent symbol name returns `isError: true`, not a null-text success. `symbol_def`'s `kind` field is a clean lowercase string, not a Debug dump. Any MCP error message for a malformed ticket ID contains no `parse:` prefix and no `TicketId` type name.
  test_command: `mise run test:crate -- tm-mcp`
  evidence: `ticket_show` returned `"budget":{"dollars_micros":18446744073709551615,...}`; `symbol_def("NonExistentSymbol")` returned `{"content":[{"text":"null","type":"text"}],"isError":false}`; `symbol_def` kind used `format!("{:?}", sym.kind)` (line 405 of server.rs); `ticket_show` on a bad ID returned `parse: TicketId must look like T-<n>, V-<n> or A-<n>, got "NONEXISTENT"`.

- [x] **p1-genesis-fallback-tries-other-ready-providers** (landed 8c18b0c) — Genesis provider fallback only probes local backends, ignoring an already-ready remote provider like devpass
  model: sonnet · severity: high · builds Rust: yes · area: genesis/providers · deps: genesis-cli-wire-mock-provider
  files: `crates/tm-cli/src/project.rs`
  change: In `resolve_genesis_provider` (~line 1172), when the `VisionFrontier` candidate's configured provider (e.g. `anthropic`) isn't configured, the fallback only tries the three entries in `tm_provider::LOCAL_PROVIDER_IDS` (ollama/lm-studio/llama-cpp) before erroring. It never checks whether any other role's candidate provider is already configured and reachable — e.g. `devpass`, which `tm provider list` reports `ready` for `coder.fast`. A user with a working DevPass credential and no Anthropic key or local model gets a hard "install Ollama" failure even though a ready remote provider already exists. Extend the fallback to also try any other configured/ready provider from the full role table before erroring.
  acceptance: With `ANTHROPIC_API_KEY` unset, no local model server running, and a configured ready `devpass` credential, `tm genesis --prompt "..."` uses devpass and produces a project instead of erroring with the "install Ollama" message.
  test_command: `mise run test:crate -- tm-cli`
  evidence: With `DEVPASS_API_KEY` set (`tm provider list` shows `coder.fast devpass ... ready`) and no Anthropic key/local runner, `tm genesis --plain --prompt "..."` errored: `ANTHROPIC_API_KEY is not set (needed by anthropic, Genesis's configured provider) and no local model provider is reachable (ollama: not running; lm-studio: not running; llama-cpp: not running)`.

- [x] **p1-symbol-refs-callers-perf** — `symbol refs`/`symbol callers` take 48-56s, making interactive navigation unusable (landed 0e21feb)
  model: sonnet · severity: critical · builds Rust: yes · area: code-navigation · deps: nav-fix-cli-symbol-output-bugs
  files: `crates/tm-codeintel/src/lib.rs`
  change: `tm symbol refs <name>` and `tm symbol callers <name>` take 48-56 seconds on this repo's own codebase — an order of magnitude too slow for interactive use, and separate from the correctness bugs `nav-fix-cli-symbol-output-bugs` already fixes (including the redundant double-parse it removes, which this task should build on rather than duplicate). Profile the reference/caller analysis path (tree-sitter parse + resolution) to find the actual hot loop — likely re-parsing every file per candidate reference rather than once per file with a cached index — and fix it so it scales with files-containing-matches, not files-in-repo times candidates.
  acceptance: `tm symbol refs transition` and `tm symbol callers transition` (or an equally common symbol) complete in under 5 seconds on a fresh clone of this repo with a built index.
  test_command: `cd /tmp/test && rm -rf repo .tm && git clone --depth 1 file:///Users/allie/Develop/ticket-master repo && cd repo && tm init && tm doctor > /dev/null && time tm symbol refs transition --plain | head -1`
  evidence: `symbol refs transition`: 48.4s user; `symbol callers transition`: 56.5s user (fresh clone, mock provider).

- [x] **p1-search-index-not-built-message** — `tm search --mode semantic` can't distinguish "index not built yet" from "no matches" (landed 0614d69)
  model: sonnet · severity: medium · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-cli/src/search.rs`
  change: In the Semantic and Hybrid arms of `search()`, the `results.is_empty()` branch prints the same "No similar content found"/"No relevant content found" whether the index has never been built (right after `tm init`, before `tm doctor`) or a query genuinely has zero matches against a real index. Add a way to tell the two apart — e.g. a `CodeIntel::stats()`-style indexed-chunk count — and print a distinct message pointing at `tm doctor` when the index is empty, leaving the existing no-match text for a real empty result against a built index.
  acceptance: In a fresh project (`tm init`, no `tm doctor` run), `tm search --mode semantic "foo"` prints a message distinct from the no-match case, naming `tm doctor` as the fix. After `tm doctor`, a genuine no-match query still prints the original "No similar content found" text.
  test_command: `mise run test:crate -- tm-cli`
  evidence: Right after `tm init` (mock provider, isolated tempdir), `tm search --mode semantic --json "where are tickets moved between states"` returned `[]` with no note; after `tm doctor` (which reported repaired incremental drift) the same query returned correctly ranked hits. The empty-index and real-empty-result cases render identically today.

- [x] **p1-cli-ticket-context-command** (landed c82dca4) — Add `tm ticket context <ID>` to make a ticket's prefetched context pack and token cost observable
  model: sonnet · severity: high · builds Rust: yes · area: cli/context · deps: none
  files: `crates/tm-cli/src/ticket.rs`, `crates/tm-context/src/lib.rs`, `crates/tm-agent/src/executor.rs`
  change: `tm-context::ContextPack` already carries sections (outlines/symbols/history/search hits) and a `rent_report`, and `tm-agent`'s executor/agent_loop builds one per attempt, but nothing surfaces it to a person — `tm run`'s output, `tm ticket show --json`, and the event log all omit it (a full ticket run's events, e.g. `UsageRecorded`, serialize as bare `{kind, seq, subject, ts}` with no payload). Add a `tm ticket context <ID>` subcommand rendering the last-compiled `ContextPack`'s sections and `rent_report` in plain words, plus a `--json` form serializing the real `ContextPack` fields. (This is a CLI-surfaced view of the same data the TUI's `s1-slash-context-todos` `/context` command shows; keep the rendering logic shared where practical rather than duplicated.)
  acceptance: After creating, activating and running a ticket about a specific function, `tm ticket context T-1` lists which kinds of material were prefetched (outline, symbol, history, search hits) with a rough token count each; `tm ticket context T-1 --json` prints the real `ContextPack` section data instead of nothing.
  test_command: `mise run test:crate -- tm-cli`
  evidence: `tm events show 13 --json` (a `UsageRecorded` event) returned `{"kind":"UsageRecorded","seq":13,"subject":"T-1","ts":"..."}` — no tokens, no context, no payload of any kind.

- [x] **p1-agent-run-progress-plain-language** (landed 440595c) — Live `tm run` progress prints raw dotted tool identifiers and raw error strings instead of plain-language actions
  model: sonnet · severity: high · builds Rust: yes · area: cli/copy · deps: none
  files: `crates/tm-cli/src/agent.rs`
  change: `format_tool_call` (~lines 1826-1834) prints the raw dotted tool-kind string (`fs.list`, `fs.read`, `edit.apply_patch`, `shell.run`, `artifact.store`, `ticket.submit`) instead of a short plain-language phrase ("Read a file", "Ran a command", "Edited a file"), and surfaces internal error text verbatim (`io: stream did not contain valid UTF-8`, `parse: unknown variant \`test-output\`, expected one of ...`, `invariant violated: submission requires at least one evidence artifact`). Map tool names to plain phrases and either drop or plain-language raw error strings, matching this repo's voice rules.
  acceptance: `tm run <T>` output during a live run contains no dotted tool-kind identifiers and no raw Rust/serde error strings (`io:`, `parse: unknown variant`, `invariant violated`).
  test_command: `mise run test:crate -- tm-cli`
  evidence: `tm run T-1` printed lines like `* fs.read -> error: io: stream did not contain valid UTF-8`, `* artifact.store -> error: parse: unknown variant \`test-output\`, expected one of \`command_output\`, \`patch\`, ...`, and `* ticket.submit -> error: invariant violated: submission requires at least one evidence artifact` directly in the run's live output.

- [x] **p1-tui-double-left-tickets-shortcut** (already satisfied) — Implement (or remove) the documented "press ← twice for tickets" shortcut
  model: sonnet · severity: high · builds Rust: yes · area: tui · deps: none
  files: `crates/tm-tui/src/screens/chat.rs`
  change: The chat screen's tip text (chat.rs:1617) says "Press ← twice for tickets: work tm does in the background", and CLAUDE.md documents the same shortcut, but chat.rs's key handler treats `KeyCode::Left` unconditionally as cursor-left (chat.rs:1317, `self.input.left()`) with no double-press tracking analogous to the existing double-Esc pattern (chat.rs ~1124-1127, built on the timing threshold at chat.rs:130). Either wire a real double-Left detector that switches to the tickets screen on an empty prompt (mirroring double-Esc), or remove the false claim from the tip text and CLAUDE.md.
  acceptance: On an empty chat prompt, pressing ArrowLeft twice within the double-press window used for Esc switches to the tickets screen; a single Left, or Left with existing input text, still moves the cursor. If instead removed, the tip text and CLAUDE.md no longer claim the shortcut exists.
  test_command: `mise run test:crate -- tm-tui`
  evidence: On an empty prompt, two consecutive ArrowLeft key sends left the screen on the chat welcome screen with no ticket view appearing; only `/tickets` + Enter opened it. `chat.rs:1317` handles `KeyCode::Left` as plain cursor movement with no double-press branch, while `chat.rs:1617` and CLAUDE.md both advertise the shortcut as working.

- [x] **p1-cli-prompt-flag-parsing-and-exit-codes** — `tm -p --json "<text>"` fails to parse, and a clap usage error is indistinguishable from a failed turn (landed 6d1fa7c)
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/main.rs`
  change: Two related CLI-parsing bugs in the same one-shot-prompt path: (1) `-p`/`--prompt` (args.rs:39) is a value-taking `Option<String>`, so any flag placed between `-p` and its text (`tm -p --json "text"`, `tm -p --help`) is swallowed as an attempted value and clap errors instead of running. Change `-p`/`--prompt` to a boolean flag plus a trailing positional `TEXT` argument (mirroring `claude -p "text"`), so flag order stops mattering and `tm -p --help` shows help. Keep args.rs's existing tests (e.g. `bare_tm_with_prompt_is_the_scriptable_agent`, ~line 1176) passing under the new shape. (2) CLAUDE.md documents exit 2 for `tm -p` as "the agent failed the task" (`TmError::TurnFailed`), but main.rs's `Cli::parse()` also exits 2 on a plain clap usage error, so a caller scripting against the exit code can't tell a malformed invocation from a real turn failure. Switch to `Cli::try_parse()` and map a clap usage error to a distinct exit code (e.g. 64, matching sysexits `EX_USAGE`).
  acceptance: `tm --json -p "x"` and `tm -p --json "x"` both run the one-shot prompt and produce JSON regardless of flag order; `tm -p --help` prints help instead of a parse error. An invalid CLI invocation (e.g. an unknown flag) exits with a code distinct from 2, while a real failed agent turn still exits 2.
  test_command: `mise run test:crate -- tm-cli`
  evidence: `tm -p --json "Where is the ticket state machine..."` exited 2 with `error: a value is required for '--prompt <TEXT>' but none was supplied`; reordering to `tm --json -p "..."` worked. The same exit code 2 is used for both a bad flag order and a genuinely failed turn.

- [x] **p1-e2e-ticket-lifecycle-scripted-provider** — Offline e2e test: full ticket lifecycle to closed with a scripted provider (landed 0c79232)
  model: sonnet · severity: critical · builds Rust: yes · area: testing · deps: p1-mock-provider-scripted-tool-calls
  files: `crates/tm-e2e/tests/ticket_lifecycle_e2e.rs`
  change: No existing test drives a ticket through `new -> activate -> run -> submitted -> accept -> closed` against a scripted (mock) provider end to end — `crates/tm-e2e`'s current suite (`authority_e2e.rs`, `invariants.rs`, `genesis_e2e.rs`, `crash_recovery.rs`, `concurrency.rs`, `replay.rs`) covers authority, genesis and crash/concurrency invariants, not the plain worker-ticket lifecycle, and this is exactly what the `mock-provider-ticket-submit` probe found broken. Once `p1-mock-provider-scripted-tool-calls` fixes the mock provider's tool-call script, add a real-binary or in-process test that runs a ticket through the full happy path and asserts the final state is `closed`, with the expected events (`TicketSubmitted`, `TicketClosed`) present in the log.
  acceptance: The new test creates a ticket, activates it, runs it to `submitted` under the scripted provider, accepts it, and asserts the final state is `closed` — failing loudly (not silently passing on a `ready`/`escalated` ticket) if the lifecycle stalls.
  test_command: `mise run test:crate -- tm-e2e`
  evidence: No test in `crates/tm-e2e/tests/` or `crates/tm-cli/tests/` currently drives a ticket to `closed`; the `lifecycle-accept` probe found the mock provider stalls the ticket at `ready`.

- [x] **p1-e2e-reject-retry-dependencies** — Offline e2e test: reject/retry cycle and dependency-gated scheduling (landed aa31a4c)
  model: sonnet · severity: high · builds Rust: yes · area: testing · deps: p1-mock-provider-scripted-tool-calls
  files: `crates/tm-e2e/tests/ticket_lifecycle_e2e.rs`
  change: Add offline coverage (same new test file as `p1-e2e-ticket-lifecycle-scripted-provider`, or a sibling in the same crate) for two flows the `lifecycle-reject-retry` and `deps-sched` probes exercised manually but no automated test asserts: (1) a ticket escalated after `max_attempts` failed attempts, retried via `tm ticket retry <T> --guidance "..."`, with guidance appended to the objective and attempts reset; (2) a `Hard` dependency edge that blocks a dependent ticket from being scheduled/leased until its dependency is closed, verified against the scheduler directly (not just CLI text output).
  acceptance: A test escalates a ticket after exhausting attempts, retries it with guidance, and asserts the guidance appears in the ticket's objective and attempts are reset. A separate test (or case in the same file) asserts a `Hard`-dependent ticket is not leased by the scheduler while its dependency is open, and is leasable once the dependency closes.
  test_command: `mise run test:crate -- tm-e2e`
  evidence: The `lifecycle-reject-retry` and `deps-sched` probes both passed manually (guidance appears in ticket state, `max_attempts` resets 3 -> 6, scheduler respects Hard edges) but via ad hoc CLI runs, not an automated regression test.

- [x] **p1-e2e-http-api-lifecycle** — Offline e2e test: `tm serve`'s HTTP API drives a ticket through creation, transitions and error paths (landed af8c895)
  model: sonnet · severity: high · builds Rust: yes · area: testing · deps: p1-http-error-json-format
  files: `crates/tm-server/tests/http_lifecycle.rs`
  change: `crates/tm-server` has no `tests/` directory at all today, so nothing regression-tests the `serve-api` probe's findings: `POST /tickets` creating a ticket, `GET /tickets/{id}` and `/events` reading it back, `GET /schema`/`/health`/`/state`, and the accept/reject/retry transition endpoints, plus the JSON error-shape fix from `p1-http-error-json-format` (a 404 and a malformed `POST` body both return `{error, message}` JSON). Spin up the axum app in-process (or bind an ephemeral port) against a tempdir project with the mock provider and drive it with a real HTTP client.
  acceptance: A new `crates/tm-server/tests/` suite covers ticket creation, read-back, schema/health/state endpoints, at least one transition endpoint, and both a 404 and a malformed-body error case asserting the JSON `{error, message}` shape.
  test_command: `mise run test:crate -- tm-server`
  evidence: `find crates/tm-server -path '*test*'` finds no test files; the `serve-api` probe drove all of this manually with `curl`.

- [x] **p1-e2e-genesis-offline-mock-provider** — Offline e2e test: Genesis runs to completion under the mock provider via the CLI (already satisfied)
  model: haiku · severity: medium · builds Rust: yes · area: testing · deps: genesis-cli-wire-mock-provider, genesis-cli-offline-integration-test
  files: `crates/tm-e2e/tests/genesis_e2e.rs`
  change: `genesis_e2e.rs`'s `seed_to_steady_state_offline_and_deterministic` already drives the `GenesisDriver` in-process offline, and `genesis-cli-offline-integration-test` (existing task) adds a real-binary subprocess test once `genesis-cli-wire-mock-provider` lands. Confirm after both land that the CLI-level path (`tm genesis --prompt ... --plain` under `TM_TEST_MOCK_PROVIDER=1`) is exercised as a regression test, not just the in-process driver test — the `genesis-offline` probe found `resolve_genesis_provider` had no mock-provider check at all, which the in-process test wouldn't have caught since it doesn't go through CLI provider resolution. If `genesis-cli-offline-integration-test`'s acceptance already covers this exact path end to end, mark this task `[~]` (deferred/superseded) with a one-line pointer instead of duplicating it.
  acceptance: A subprocess or CLI-level test runs `tm genesis --prompt "..." --plain` under `TM_TEST_MOCK_PROVIDER=1` in a fresh tempdir/TM_HOME to completion (exit 0, at least one ticket created), covering the CLI provider-resolution path that the existing in-process `genesis_e2e.rs` test does not.
  test_command: `mise run test:crate -- tm-e2e`
  evidence: `tm genesis --prompt 'a Python CLI todo app' --plain` under `TM_TEST_MOCK_PROVIDER=1` failed with `ANTHROPIC_API_KEY is not set ... and no local model provider is reachable` — the mock-provider check in `agent.rs`'s `build_fabric` was never added to `project.rs`'s `resolve_genesis_provider`, and no CLI-level test caught it.

- [x] **p1-e2e-navigation-freshness-search-prefetch** — Offline e2e test: index freshness after edits, search modes, and prefetched context-pack sections (landed 4972b6b)
  model: sonnet · severity: medium · builds Rust: yes · area: testing · deps: nav-fix-project-codeintel-freshness
  files: `crates/tm-e2e/tests/navigation_e2e.rs`
  change: The `nav-fresh`, `nav-semantic` and `prefetch` probes each manually verified real behavior — the index picks up new/edited files, `tm doctor` repairs drift and reports counts, exact/semantic/hybrid search return correct top hits, and a `ContextPack` with sections and a `rent_report` is built per attempt — but none of it is asserted by an automated test. Add an offline e2e test (mock provider, tempdir project, no network) that: edits a file after initial indexing and asserts the next search reflects the edit (freshness); runs at least one exact and one semantic/hybrid query with a known expected top hit; and asserts a ticket run produces a non-empty `ContextPack` (via whatever surface `p1-cli-ticket-context-command` or the internal API exposes) with at least one section populated.
  acceptance: A new test in `crates/tm-e2e` asserts, in one pass: (1) editing a file changes what a subsequent search call returns; (2) a known-content query via `search_exact`/`search_hybrid` (or the CLI equivalent) returns the expected top file; (3) a completed ticket attempt's `ContextPack` has at least one non-empty section.
  test_command: `mise run test:crate -- tm-e2e`
  evidence: The `nav-fresh` probe confirmed indexing picks up new files/edits and `nav-semantic` confirmed correct top-1 hits, and `prefetch` confirmed a `ContextPack`/`rent_report` abstraction exists internally — all verified by hand via CLI probes, none by an automated regression test.

- [x] **d20-decider-trait-and-mock** (landed 6a2f786) — Add DecisionProvider trait, DecideRequest/Response types, MockDecisionProvider, and Role::Decider
  model: sonnet · severity: critical · builds Rust: yes · area: providers (D-020) · deps: none
  files: `crates/tm-provider/src/decide.rs`, `crates/tm-provider/src/lib.rs`, `crates/tm-provider/src/mock.rs`, `crates/tm-types/src/role.rs`
  change: Create `crates/tm-provider/src/decide.rs` with the `DecisionProvider` trait (id, limits, async decide) and `DecideRequest`/`DecideResponse`/`Question(Choice|Score|Noul)`/`DecideLimits` types per D-020 §Decision 1-2 (`docs/decisions/D-020-system-one-decision-providers.md`); add `MockDecisionProvider` (hash-keyed scripted responses, same pattern as `crates/tm-provider/src/mock.rs`'s `MockProvider` — no I/O ever) in the same file or a sibling `decide/mock.rs`; add `Role::Decider` to `crates/tm-types/src/role.rs`'s enum and its `pub const ALL: [Role; 12]` (becomes 13), updating every place that iterates `Role::ALL` and any hardcoded `12` in tests. This is a foundational task all other D-020 tasks below depend on.
  acceptance: cargo builds; `Role::ALL.len() == 13` and includes Decider; MockDecisionProvider returns deterministic answers for the same DecideRequest across two calls and differs for a different request; no cargo test touches the network.
  test: `mise run test:crate -- tm-provider`
  evidence: `grep -rn 'DecisionProvider\|Role::Decider\|classify\.decided' crates --include='*.rs'` (worktrees excluded) → no output, exit 1: nothing exists yet. `crates/tm-types/src/role.rs:39` `pub const ALL: [Role; 12] = [...]` confirms the enum and cardinality to extend.

- [x] **d20-decider-http-systemone-client** (landed 32b3607) — Add an HTTP DecisionProvider for the /v1/systemone wire contract (Jev gateway, TypeSafe direct, jevmlx local)
  model: sonnet · severity: critical · builds Rust: yes · area: providers (D-020) · deps: d20-decider-trait-and-mock
  files: `crates/tm-provider/src/providers/systemone.rs`, `crates/tm-provider/src/providers/mod.rs`, `crates/tm-provider/src/providers/registry.rs`
  change: Add `providers/systemone.rs` implementing `DecisionProvider` (from `d20-decider-trait-and-mock`) with a reqwest-based client (same shape as `providers/openai.rs`) against a configurable `base_url` + `/v1/systemone`, POSTing `{model, state, questions}` and parsing `{answers, usage, provider_metadata}` per Vercel's documented TypeSafe-compatible API (https://vercel.com/docs/ai-gateway/sdks-and-apis/typesafe). Auth: `Authorization: Bearer <token>` where token comes from `AI_GATEWAY_API_KEY`, `TYPESAFE_API_KEY`, or a caller-supplied token (never hardcode a header name assumption beyond Bearer). Do NOT build an OpenAI-chat-shaped client or target `mlx_lm.server` — Jev/TypeSafe never speak chat-completions and Laya is not a generative LM. Register it in `providers/registry.rs` so a config can select it for `Role::Decider`.
  acceptance: A unit test with an injected fake transport (trait-object or a local test double, not real network) proves the request body matches `{model,state,questions}` and a 200 JSON response parses into `DecideResponse` with per-option probabilities; an error body `{message,error_type}` maps to a typed `ProviderError`.
  test: `mise run test:crate -- tm-provider`
  evidence: WebFetch of https://vercel.com/docs/ai-gateway/sdks-and-apis/typesafe: base URL `https://ai-gateway.vercel.sh/typesafe`, POST `/typesafe/v1/systemone`, body `{"model":"typesafe-ai/jev","state":...,"questions":{...}}`, auth `Authorization: Bearer $AI_GATEWAY_API_KEY` or OIDC token.

- [x] **d20-decider-config-selection** — Add config to select the decider backend (mock/http) and its base URL/model per role_config's existing pattern (landed 03cd3e0)
  model: sonnet · severity: high · builds Rust: yes · area: providers (D-020) · deps: d20-decider-trait-and-mock
  files: `crates/tm-provider/src/role_config.rs`
  change: Extend `role_config.rs`'s TOML schema with a Decider role entry (candidates: mock, or systemone-http with base_url/model_id/env var name for the token) reusing the existing `RoleConfigError` variants (`UnknownRole`, `EmptyRole`, `ZeroConcurrency`) rather than inventing a parallel config path, so `tm doctor`/existing role-config validation covers it for free.
  acceptance: A role_config.toml with a `[decider]` section (or equivalent) parses into a routable candidate list; an empty Decider role produces `RoleConfigError::EmptyRole` like any other role; existing role_config tests still pass.
  test: `mise run test:crate -- tm-provider`
  evidence: `crates/tm-provider/src/role_config.rs:90-110` shows the existing `RoleConfigError` enum (InvalidToml, UnknownRole, EmptyRole, ZeroConcurrency) this task reuses rather than duplicating.

- [x] **d20-classify-decided-event-kind** (landed 650cd77) — Add EventKind::ClassifyDecided ("classify.decided") with the D-020 payload shape
  model: sonnet · severity: high · builds Rust: yes · area: events (D-020) · deps: none
  files: `crates/tm-events/src/kind.rs`
  change: Add a `ClassifyDecided` variant to `EventKind` following the existing pattern at `kind.rs:79-80/430` (TicketCreated → "ticket.created") and `:225-226/476` (ApprovalRequested → "approval.requested"): `#[serde(rename = "classify.decided")]`, wire string "classify.decided", with fields `{site, backend, model, model_revision, input_hash, questions_hash, answers, calibrated_confidence, thresholds, disposition, latency_ms, cost_micros}` per D-020's cascade diagram (`docs/vision/system-one-decisions.md` §3). Use the `classify.*` namespace, not `decision.*` (already taken by the unrelated `DecisionId` ticket-decision domain in `crates/tm-core/src/decision.rs`).
  acceptance: `EventKind::ClassifyDecided` round-trips through serde as "classify.decided"; the event log's existing kind-exhaustiveness test (if any) still compiles/passes with the new variant.
  test: `mise run test:crate -- tm-events`
  evidence: `crates/tm-events/src/kind.rs:79-80` `#[serde(rename = "ticket.created")]` and `:225-226` `#[serde(rename = "approval.requested")]` are the two existing precedents this task follows; grep confirmed no `classify.*` or decision-provider event kind exists yet.

- [x] **d20-shadow-triage-new-tickets** — Shadow-classify new tickets at creation: call the decider, record classify.decided(shadow), never act on it (landed 7b363a8)
  model: sonnet · severity: high · builds Rust: yes · area: core (D-020) · deps: d20-decider-trait-and-mock, d20-decider-config-selection, d20-classify-decided-event-kind
  files: `crates/tm-core/src/store.rs`
  change: At the ticket-creation append site in tm-core (grep `'"ticket.created"'` in `crates/tm-core/src` for the exact function), after appending `ticket.created`, call the configured `Role::Decider` candidate (from `d20-decider-config-selection`) with a triage-shaped `DecideRequest` (kind/routing questions over the ticket objective), and append `classify.decided` with `disposition:"shadow"` via `d20-classify-decided-event-kind`'s new `EventKind`. On any decider error or missing candidate, log and continue — ticket creation must never fail or block on this.
  acceptance: Creating a ticket with the `MockDecisionProvider` configured appends both `ticket.created` and a `classify.decided(shadow)` event in the same store transaction/sequence; with no decider configured, ticket creation behaves exactly as before (no new event, no error).
  test: `mise run test:crate -- tm-core`
  evidence: `grep -rln '"ticket.created"' crates --include='*.rs'` (worktrees excluded) → `crates/tm-cli/tests/tui_tickets.rs`, `crates/tm-cli/tests/ticket_fork.rs`, `crates/tm-events/src/kind.rs` — the real append site in tm-core's own src needs its own grep at implementation time since this search only turned up test/event-kind references.

- [x] **d20-redact-decide-request** — Add redact_decide_request through SessionRedactor before any remote DecisionProvider call (landed ae5c687)
  model: sonnet · severity: medium · builds Rust: yes · area: auth (D-020) · deps: d20-decider-trait-and-mock, d20-decider-http-systemone-client
  files: `crates/tm-auth/src/redact.rs`
  change: Per D-020 decision 7 ("Redaction before anything leaves the machine"), add a `redact_decide_request(&DecideRequest) -> DecideRequest` function alongside `SessionRedactor`'s existing redaction entry points in `crates/tm-auth/src/redact.rs`, applying the same secret-pattern scrubbing `SessionRedactor` already does to session/turn content, to the `DecideRequest`'s state and question text. Wire the HTTP decider provider (`d20-decider-http-systemone-client`) to call it before every outbound request; the mock/local backends don't need it (nothing leaves the machine).
  acceptance: A `DecideRequest` whose state contains a fake API-key-shaped string is redacted identically to how `SessionRedactor` redacts the same string in a normal turn; the HTTP systemone provider's outbound body in a unit test never contains the raw secret.
  test: `mise run test:crate -- tm-auth`
  evidence: `grep -rln 'SessionRedactor' crates --include='*.rs'` (worktrees excluded) → `crates/tm-auth/src/lib.rs`, `crates/tm-auth/src/redact.rs`, `crates/tm-provider/src/fabric.rs`, `crates/tm-core/src/store.rs` — the redaction entry point this task extends.

- [x] **d20-bench-decision-eval** — Add a small offline decision-eval task to tm-harness's bench (landed 9ff9a5f)
  model: sonnet · severity: medium · builds Rust: yes · area: bench (D-020) · deps: d20-decider-trait-and-mock
  files: `crates/tm-harness/src/bench.rs`
  change: Extend `crates/tm-harness/src/bench.rs` with a decision-eval task type that replays `classify.decided(shadow)` events against known outcomes (`Session.promote`, `StepRecord`, ticket end-states per D-020's "Why" section) and reports accuracy/ECE, using the `MockDecisionProvider` for a fully offline, hermetic fixture so the eval itself needs no network or real decider.
  acceptance: `tm bench` (or the crate's existing bench entry point) runs the new decision-eval task against a fixture of recorded `classify.decided` + outcome pairs and reports an accuracy number, with zero network calls.
  test: `mise run test:crate -- tm-harness`
  evidence: `crates/tm-harness/src/bench.rs` exists (`find crates -iname '*bench*.rs'` outside worktrees/target → only this file) and is the natural extension point named in D-020 consequence "Plus a `tm bench` decision task scored against `Session.promote` and `StepRecord`."

- [x] **d20-flip-status-and-amend-spec** — Flip D-020 from proposed to accepted and amend SPEC.md §6.1 with the decider role, once the trait/mock lands (landed b6037ec)
  model: haiku · severity: low · builds Rust: no · area: docs (D-020) · deps: d20-decider-trait-and-mock, d20-shadow-triage-new-tickets
  files: `docs/decisions/D-020-system-one-decision-providers.md`, `SPEC.md`
  change: Once `d20-decider-trait-and-mock` (and ideally `d20-shadow-triage-new-tickets`) merge, change D-020's "Status: proposed" line to "Status: accepted", and add the decider role / DecisionProvider trait to SPEC.md §6.1 per D-020's own closing line ("If accepted, SPEC.md ... gains the decider role and the DecisionProvider trait"), per this repo's "keep documentation honest as you change things" rule.
  acceptance: `docs/decisions/D-020-system-one-decision-providers.md`'s Status line reads accepted; SPEC.md §6.1 mentions Role::Decider/DecisionProvider; `mise run hygiene`'s D-NNN cross-reference check still passes.
  test: `mise run hygiene`
  evidence: `docs/decisions/D-020-system-one-decision-providers.md` header line: "**Status:** proposed · **Date:** 2026-09-23" and its closing paragraph naming exactly this SPEC §6.1 amendment as conditional on acceptance.

- [x] **d20-backlog-status-accepted** — Move D-020 out of docs/backlog.md's "Ask the owner later" and record it as owner-accepted (landed c261046)
  model: haiku · severity: low · builds Rust: no · area: docs (D-020) · deps: none
  files: `docs/backlog.md`
  change: The owner approved D-020 on 2026-09-23 (asked directly for Jev/Laya support). Remove the "D-020 (system-one decision providers, proposed): deferred by the owner on 2026-09-23 ('toss jev in backlog')..." entry from `docs/backlog.md`'s "## Ask the owner later" section and replace it with a short "## Accepted, 2026-09-23 (D-020 system-one decision providers)" entry noting the approval, pointing at `docs/tasks/TASKS.md`'s `d20-*` tasks for the shadow-mode slice, and folding in this session's Jev/Laya research findings (Laya is on Hugging Face as `convaiinnovations/laya*`, not Kaggle — Kaggle's role is the free fine-tune notebook; `laya-mlx` has no server mode; Jev's real contract is `POST https://ai-gateway.vercel.sh/typesafe/v1/systemone`, not OpenAI chat-completions, auth via `AI_GATEWAY_API_KEY` or Vercel OIDC, neither available in the research shell so no live call was made and no key was created).
  acceptance: `docs/backlog.md` no longer lists D-020 under "Ask the owner later"; a new section records the acceptance and the Jev/Laya research summary; `mise run hygiene` still passes.
  test: `mise run hygiene`
  evidence: `docs/backlog.md`'s "## Ask the owner later" section currently contains the D-020 deferral entry dated 2026-09-23, now superseded by the owner's direct Jev/Laya request in this same session.

- [x] **critic-b0-ci-green** (already satisfied) — CI is green on main (verified via `gh run view 36118529660`, sha c322641, both `test (ubuntu-latest)` and `test (macos-latest)` succeeded); the Linux SIGTERM/143 root cause was `crates/tm-pty/src/session.rs`'s `kill_process_group` (`kill -TERM -{pid}` without `--`), already fixed at commit 7ea4d4e
  model: sonnet · severity: critical · builds Rust: yes · area: ci · deps: none
  files: `crates/tm-codeintel/src/walk.rs`, `crates/tm-computer/src/linux.rs`, `rust-toolchain.toml`
  change: Two verified, independent CI failures block every other batch. (1) `clippy::for_kv_map` fails on both OSes (CI run 35917628490): `for (path, _hash) in previous_map.iter()` at walk.rs:279 iterates a map only for its keys; change it to `.keys()`. (2) `crates/tm-computer/src/linux.rs` does not compile against x11rb 0.14 on Linux (Release run 35883664234), which is also why the v0.1.0 release shipped only an aarch64 asset; fix the x11rb 0.14 API mismatch. (3) `rust-toolchain.toml` is `channel = "stable"` with no version pinned, while this machine has 1.95, so a local `mise run verify` pass is not proof CI will pass; pin an exact version matching what CI's dtolnay/rust-toolchain action resolves today. Run this before B1 starts, since every later batch's gate assumes CI is green.
  acceptance: `mise run verify` passes (clippy -D warnings clean including for_kv_map). `cargo build -p tm-computer --target x86_64-unknown-linux-gnu` (or the CI Linux job) compiles. `rust-toolchain.toml` names an exact version, and a fresh `rustup show` in a clean checkout resolves to it.
  test: `mise run verify`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 1: CI run 35917628490 (clippy for_kv_map at walk.rs:279) and Release run 35883664234 (linux.rs vs x11rb 0.14); `rust-toolchain.toml` confirmed unpinned (`channel = "stable"`) against a local 1.95 toolchain.

- [x] **critic-merge-odw-integrate-before-b1** — Merge or rebase odw-integrate before B1 starts; it overlaps most of the plan's early files (landed: the odw-integrate WIP, /level and its fallback routing, was ported and merged via port/odw-level)
  model: sonnet · severity: critical · builds Rust: yes · area: repo/merge · deps: critic-b0-ci-green
  files: `crates/tm-cli/src/project.rs`, `crates/tm-cli/src/dispatch.rs`, `crates/tm-cli/src/sched.rs`, `crates/tm-cli/src/agent.rs`, `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/main.rs`, `crates/tm-provider/src/role_config.rs`
  change: The `odw-integrate` worktree (provider-overhaul work, untouched by this plan per CLAUDE.md) has uncommitted edits to project.rs, dispatch.rs, sched.rs, agent.rs, ops.rs, chat_ops.rs and tm-types budget.rs, plus odw-provider's edits to role_config.rs/route.rs/providers/*. B1 (project.rs, dispatch.rs), B2/B4/B6/B7 (ops.rs, args.rs), B3/B6/B8 (project.rs, agent.rs), B12/B13 (args/sched/dispatch/agent.rs) and d020-role-decider (role_config.rs) all collide with it, and main has moved past the `--ff-only` merge the odw README plans. This task is: get the odw owner to land (a normal merge, likely with conflicts) or explicitly abandon odw-integrate/odw-provider before any batch below B1 starts editing the same files — do not touch `.claude/worktrees/odw-*` directly. If the owner instead says to proceed in parallel, record that decision here and have each colliding task rebase onto odw's landed state instead of main.
  acceptance: Either odw-integrate/odw-provider are merged into main (a plain `git log --merges` entry, tests green), or the owner's explicit decision to proceed in parallel is recorded in this task with a one-line rationale, before B1's first PR merges.
  test: `git log --oneline -1 origin/main`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 2, cross-checked against this session's own note that odw-integrate can no longer fast-forward and has uncommitted edits in sched.rs/dispatch.rs/agent.rs/ops.rs/chat_ops.rs.

- [x] **critic-real-prices-and-price-unit** (landed f1cdba4) — Fix the price unit (sub-$1/M rounds to 0) and fill in real configured-model prices
  model: sonnet · severity: high · builds Rust: yes · area: providers/budget · deps: tel-completion-cost-field
  files: `crates/tm-provider/src/role_config.rs`, `crates/tm-provider/src/fabric.rs`, `crates/tm-provider/providers.toml`
  change: No non-test `RoleCandidate` has `price: Some` (only test fixtures at role_config.rs:685). Price is stored as an integer number of micros per token (role_config.rs:48), so any model priced under $1/M tokens (Jev is $0.042/M) truncates to 0 micros/token. That silently zeroes tel-completion-cost-field, `tm stats` dollars, budget tier-down and the bench cost column even after this session's telemetry batches land. Change the unit to micros per 1,000,000 tokens (or an equivalent fixed-point representation with enough precision for sub-cent-per-token models), update `cost_micros`'s math in fabric.rs to match, and fill in real prices for every model this project actually configures in providers.toml. Coordinate with the odw provider-overhaul work, which also owns role_config.rs/providers.toml (see critic-merge-odw-integrate-before-b1).
  acceptance: A unit test prices a $0.042/M-token model over a realistic token count and asserts a nonzero `cost_micros`. `tm stats --by model` on a real run of a configured sub-$1/M model shows nonzero dollars.
  test: `mise run test:crate -- tm-provider`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 4: `rg 'price:\s*Some'` finds only the role_config.rs:685 test fixture; role_config.rs:48 documents price as integer micros/token.

- [x] **critic-worktree-exec-root-indexing** (landed 5906619) — `tm run --worktree` indexes the main checkout, not the worktree it's actually running in
  model: sonnet · severity: high · builds Rust: yes · area: code-navigation · deps: nav-fix-project-codeintel-freshness, nav-agent-tool-index-refresh
  files: `crates/tm-cli/src/dispatch.rs`
  change: `build_dispatcher_with_fabric` builds the CodeIntel `ci` from `project.code_intel()` (dispatch.rs:281), and `ProjectContextPackSource` opens `root: project.root` (dispatch.rs:322-326), while tools actually run at `exec_root` via `with_root` (dispatch.rs:305-307). Under `tm run --worktree` (D-012), search/symbol/prefetch results therefore show the main checkout's index instead of the worktree's own tree, and a per-worktree cache would be wrong to share. Index `exec_root` instead of `project.root` whenever they differ, with a per-worktree index db path (e.g. under the worktree's own state dir) so worktree runs don't fight the main checkout's index for writes.
  acceptance: A new test: create a ticket, run it with `--worktree` against a repo where the worktree branch has a file the main checkout doesn't, and assert a `search.*`/`symbol.*` tool call inside that run finds the worktree-only file.
  test: `mise run test:crate -- tm-cli`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 6: dispatch.rs:281 and :322-326 build/open CodeIntel from `project.root`, while :305-307 execute tools at `exec_root`.

- [x] **critic-nav-quality-eval** — A repeatable search-quality eval (query -> expected file in top-k), not two hand-picked queries (landed c0c6e0b)
  model: sonnet · severity: medium · builds Rust: yes · area: code-navigation · deps: nav-potion-embedder
  files: `crates/tm-codeintel/tests/nav_quality_eval.rs`, `crates/tm-codeintel/fixtures/nav_eval.toml`
  change: SPEC §10 asks for "searches before first relevant hit" and §32.2 asks `tm doctor` to surface rg-fallback counts as a measurement, but nothing computes either, and `nav-potion-embedder`'s acceptance only hand-picks two queries. Add a small fixture set of (query, expected top-k file) pairs against this repo (or a frozen snapshot of it), and a test/binary that runs each query through hybrid search and reports hit rate at k=1/3/5, so the embedder switch (and future retrieval changes) can be scored against a fixed baseline instead of eyeballed.
  acceptance: `cargo test -p tm-codeintel nav_quality_eval` runs at least 10 query/expected-file pairs and reports (and asserts a floor on) hit-rate@3; a regression in retrieval quality fails the test, not just a manual check.
  test: `mise run test:crate -- tm-codeintel`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 10: SPEC §10/§32.2's ask for a nav-quality measurement, and nav-potion-embedder's acceptance having only two hand-picked queries.

- [x] **critic-tool-schema-trim-prompt-caching** (landed 7a7e13a) — Cut per-turn token burn: authority-scoped tool trimming and prompt caching
  model: sonnet · severity: high · builds Rust: yes · area: agent/providers · deps: none
  files: `crates/tm-agent/src/tools.rs`, `crates/tm-provider/src/fabric.rs`, `docs/decisions/D-0NN-tool-schema-trimming-and-caching.md`
  change: `docs/backlog.md:617-620` records roughly 175k tokens per small turn and about 18KB of tool schemas resent on every single step, which will dominate both benchmark cost/score and every real ticket run's budget. This is also the direct cause of the "4 agents burning 200k tokens each" pattern the owner explicitly flagged as unwanted: wide decomposition into many small, cheap turns only pays off if each turn's fixed overhead (full tool schema set, full context) is trimmed, not carried on every step. Add: (1) authority-scoped tool filtering (SPEC §30.1) so a ticket's `Authority` determines which tool schemas are even sent, instead of the full set on every request; (2) provider-side prompt caching for the stable prefix (system prompt + tool schemas) wherever the configured provider supports it (Anthropic's `cache_control`, or the gateway's equivalent), reusing `Fabric::execute`/`execute_priced`'s existing request path. Write a decision doc recording the before/after token measurement. This is in-flight work already noted as depending on the edit-hash fix agents; coordinate rather than duplicate.
  acceptance: A before/after measurement (same scripted multi-step ticket run) in the new decision doc shows a real reduction in tokens-sent-per-step; a unit test asserts a `worker()`-authority request omits tool schemas a `Authority::admin()`-only role would include.
  test: `mise run test:crate -- tm-agent && mise run test:crate -- tm-provider`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 13: `docs/backlog.md:617-620`'s ~175k-tokens/~18KB-schema measurement; owner's own framing in this session ("not 4 agents each taking 200k tokens").

- [x] **critic-ci-client-coverage** — CI only runs Rust; add the pnpm (ts/vscode/web) and swift suites the plan's own gates already assume (landed 03daad1)
  model: haiku · severity: medium · builds Rust: no · area: ci · deps: none
  files: `.github/workflows/ci.yml`
  change: `ci.yml` runs only the Rust workspace, but `docs/tasks/README.md:12-13` requires the `clients/web` suite, and this plan's own B1/B2 gates already run `pnpm -C clients/ts`/`pnpm -C clients/vscode` and `swift test --package-path clients/macos` locally. Add jobs (or steps) running `pnpm -C clients/ts install && pnpm -C clients/ts test && pnpm -C clients/ts build`, the same for `clients/vscode`, `clients/web`'s test/build, and `swift test --package-path clients/macos` on macOS runners, so a client-only regression is caught in CI instead of only when a batch gate happens to run locally.
  acceptance: A pushed PR touching only `clients/ts` fails CI on an introduced TS test failure; `actionlint .github/workflows/ci.yml` passes.
  test: `mise x aqua:rhysd/actionlint@latest -- actionlint .github/workflows/ci.yml`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 14: `ci.yml` runs only Rust; `docs/tasks/README.md:12-13` requires the clients/web suite.

- [x] **critic-format-debug-strings-cleanup** (landed 835e681) — Sweep the remaining machine-speak `format!("{:?}", ...)` output outside s1-display-labels' scope
  model: sonnet · severity: medium · builds Rust: yes · area: cli/copy · deps: s1-display-labels
  files: `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/tui.rs`, `crates/tm-cli/src/search.rs`, `crates/tm-cli/src/sched.rs`
  change: `s1-display-labels` fixes ticket/milestone state and kind formatting in tickets.rs, but there are 33 `format!("{:?}", ...)` sites in tm-cli in total, and several are outside that task's files. `tm events show` (ops.rs:2248) prints the Debug kind name (e.g. `TicketCreated`) instead of the wire string ("ticket.created") and omits the payload its own doc comment promises; also fix ops.rs:105-121. tui.rs:355-356 and search.rs:369/498 debug-format values a user reads directly on screen. Replace each with a purpose-built human string (reuse render.rs's label helpers from s1-display-labels where the type overlaps; add a small local formatter otherwise).
  acceptance: `rg 'format!\("\{:\?\}"' crates/tm-cli/src` (outside test modules) returns nothing at the listed sites; `tm events show <id>` prints the event's wire kind string and its payload, not a Debug struct.
  test: `mise run test:crate -- tm-cli`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 9(1): ops.rs:2248 (kind Debug name, no payload), ops.rs:105-121, tui.rs:355-356, search.rs:369/498; 33 total `format!("{:?}")` sites counted in tm-cli.

- [x] **critic-slash-commands-new-surfaces** — Slash commands for the surfaces this plan adds: stats, bench, events, replay, genesis, search/symbol (landed 9238a4a)
  model: sonnet · severity: medium · builds Rust: yes · area: tui/chat · deps: s1-slash-pm-views, tel-stats-cli-command, bench-report-render, tel-events-kind-ticket-filter, replay-cli-replay-flag
  files: `crates/tm-tui/src/chat/commands.rs`, `crates/tm-tui/src/chat/slash_views.rs`, `CLAUDE.md`
  change: `s1-surfaces-decision-doc`'s slash-command table already adds `/context`, `/todos`, `/memory`, `/export`, `/doctor`, `/permissions`, `/review`, `/search`, `/run`, `/ticket`, `/board`, `/milestones`, `/timeline`, `/deps`, `/workflow`, and `s1-slash-pm-views` implements the project-management subset, but nothing in the plan wires `/stats`, `/bench`, `/events`, `/replay` or `/genesis` once those CLI verbs exist — a user driving the chat has no way to reach them without dropping to `!`-shell. Add COMMANDS rows and handlers in slash_views.rs (same one-line-per-arm pattern as s1-slash-pm-views) for `/stats` (renders `tm stats` output inline), `/bench` (lists/runs bench tasks), `/events` (tails recent events), `/replay <path>` and `/genesis <prompt>` (queues a genesis run). Keep chat_ops.rs's match arm additions minimal, since it has uncommitted edits in the untouched `odw-integrate` worktree.
  acceptance: `/stats`, `/bench`, `/events`, `/replay` and `/genesis` all appear in `/?`'s shortcuts panel and each round-trips to the equivalent CLI command's output inside the chat transcript. `COMMANDS.len()`-based tests still pass.
  test: `mise run test:crate -- tm-tui`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 9(2): the TUI slash-command list gets no entries for stats/bench/events/replay/genesis/search/symbol, cross-checked against `crates/tm-tui/src/chat/commands.rs`'s current command set.

- [x] **critic-secure-kaggle-token** — Store the owner-pasted Kaggle API token safely; never in prompts, commits, or TASKS.md (already satisfied: the token lives only in ~/.kaggle/access_token, mode 600, never in .env, prompts or commits; the owner should still rotate it)
  model: haiku · severity: low · builds Rust: no · area: security · deps: none
  files: `.env`, `docs/backlog.md`
  change: The owner pasted a live Kaggle API token directly into a chat turn during this session (for the Laya fine-tune path per `d20-backlog-status-accepted`'s research notes). Treat it as already exposed: add it only to `.env` (gitignored, never printed/echoed/committed per this repo's hard rules) under a clearly named key (e.g. `KAGGLE_API_TOKEN`), and add a one-line note next to the existing DevPass-key-rotation entry in `docs/backlog.md` recording that a second credential (Kaggle) is pending rotation once the Laya fine-tune work actually needs it. Do not paste the token's value anywhere in `docs/`, `TASKS.md`, commit messages, or agent transcripts going forward.
  acceptance: `.env` (not committed) holds the token under a named key; `docs/backlog.md` records the pending-rotation note without the token's value; a search of tracked files for the token's literal value returns nothing.
  test: `git status --porcelain .env`
  evidence: workflow wf_87a85411-848's critic result, "missing" item 15: security is open, and the owner's own request this session pasted a live Kaggle token in chat.

## Owner asks (2026-09-23)

- Actual ticket flow works end to end: request → implementation → verification → finished, from the chat, the tickets screen, `tm serve` and `tm mcp`.
- Genesis works and makes real projects.
- Remove jank and machine-speak from every user-facing string; plain, friendly, specific wording.
- Configurable, with good options: settings that matter are discoverable and have sane defaults.
- Expand the parts that are thin; find and fix bugs, broken UX, confused concepts, stubbed or unintegrated features.
- Project management: milestones, timelines, dependencies, a calendar/timeline TUI view.

## B1 Wiring: code-index freshness + symbol self-reference; release packaging; TS SDK + vscode bundling

Gate: `mise run verify && pnpm -C clients/ts install && pnpm -C clients/ts test && pnpm -C clients/ts build && pnpm -C clients/vscode install && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test && mise x aqua:rhysd/actionlint@latest -- actionlint .github/workflows/release.yml`

- [x] **nav-fix-project-codeintel-freshness** — Refresh the code index in Project::code_intel() and in the dispatch context-pack source (landed 703d197)
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-cli/src/project.rs`, `crates/tm-cli/src/dispatch.rs`
  change: Project::code_intel() (project.rs:79-81) only calls CodeIntel::open_at and never calls update_incremental. ProjectContextPackSource::compile (dispatch.rs:28-52, used by tm run/tm sched run) opens its own CodeIntel and does not refresh either. Change: (1) in code_intel(), call update_incremental(self.clock.as_ref()) after open_at. (2) Add a clock: Arc<dyn Clock> field to ProjectContextPackSource, pass project.clock.clone() at its construction in build_dispatcher, and call update_incremental in compile() before building the pack. Also refresh the long-lived Arc<CodeIntel> built at dispatch.rs:267 once when it is constructed. Degrade, never fail: update_incremental's history ingest needs a git HEAD, so on Err log tracing::warn! and return the opened index. If the root is not inside a git work tree, skip the refresh entirely, so a global-scope project in an arbitrary directory never walks something like $HOME. Leave tm doctor's own explicit update_incremental call (project.rs:1920) as it is. This task absorbs nav-fix-dispatch-context-pack-freshness.
  acceptance: New tests: (a) a git tempdir with one commit containing a Rust fn, opened without tm doctor, resolves `symbol def` for that fn; (b) ProjectContextPackSource::compile over the same never-doctored project includes that file's outline or retrieval content; (c) code_intel() in a non-git tempdir and in a git repo with zero commits returns Ok and logs a warning. The handoff reports the measured no-op refresh time on this repo's tree. If it exceeds about 500ms, say so explicitly so a follow-up can gate the refresh on mtime.
  test: `mise run test:crate -- tm-cli`

- [x] **nav-fix-codeintel-self-reference** — Stop treating a definition's own name token as a reference to itself (landed 9f52d4e)
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-codeintel/src/symbols.rs`
  change: In SymbolIndex::parse_file (around symbols.rs:399-421), after collecting name_ranges for this file's symbols, drop any captured reference whose (path, range) exactly equals a definition's own name range before pushing it onto self.references. Tighten rename_preview_edits_definition_and_confident_references (around symbols.rs:930-947) from `edits.len() >= 2` to `== 2`. Add a regression test that idx.callers(id) does not include the symbol's own definition when the function never calls itself.
  acceptance: In the two-function case (fn helper; fn main calls helper), refs(helper) returns exactly one hit, the call site, and callers(helper) is exactly [main]. The rename_preview test passes with == 2, and the new self-caller test passes.
  test: `mise run test:crate -- tm-codeintel`

- [x] **release-build-artifact-and-install-guide** — Release workflow producing tm binaries, a mise install task, and README install guidance (already satisfied: .github/workflows/release.yml and `mise run release`)
  superseded on main by the parallel session: `.github/workflows/release.yml`, README install paths and release v0.1.0 exist. Remaining gap tracked as `release-web-assets` in section T.
  model: sonnet · size: S · builds Rust: no · area: release/packaging (user request) · deps: none
  files: `.github/workflows/release.yml`, `README.md`, `mise.toml`, `CLAUDE.md`
  change: (1) Add .github/workflows/release.yml, triggered on `v*` tag pushes plus workflow_dispatch, with permissions contents: write. Use a matrix of macos-14 (aarch64-apple-darwin) and ubuntu-latest (x86_64-unknown-linux-gnu). Steps: checkout, dtolnay/rust-toolchain honoring rust-toolchain.toml, Swatinem/rust-cache, `cargo build --release --locked -p tm-cli`, package tm-<tag>-<target>.tar.gz containing the `tm` binary plus a SHA256SUMS entry, then upload with `gh release create/upload` using GITHUB_TOKEN. Do not run the test suite here; ci.yml already does. (2) Add a mise `install` task: `cargo install --path crates/tm-cli --locked -j 2`, keeping the repo's -j 2 cap. (3) Write a root README.md; none exists today. Include a one-paragraph description pointing to SPEC.md, docs/ and CLAUDE.md, then three install paths for a PRIVATE repo, where plain curl will not work: (a) `gh release download vX --repo alliecatowo/ticket-master --pattern '*aarch64-apple-darwin*'`, verify with shasum -a 256 -c, extract to ~/.local/bin, and note the binary is unsigned so macOS needs `xattr -d com.apple.quarantine`; (b) `cargo install --git ssh://git@github.com/alliecatowo/ticket-master.git tm-cli --locked`; (c) from source with `mise install && mise run install`. Add a quickstart (`tm doctor`, `tm`, `tm ticket new`, `tm serve`, noting that the web client needs a pnpm build) and provider credential setup that links docs/providers.md and D-005 rather than duplicating them. (4) Add CLAUDE.md lines for `mise run install` and for the release process (tag push triggers release.yml).
  acceptance: actionlint passes on release.yml, `mise tasks` lists install, and the README gives all three install paths plus the quarantine note. No Rust code changes.
  test: `mise x aqua:rhysd/actionlint@latest -- actionlint .github/workflows/release.yml && mise tasks | grep -qw install`

- [x] **ts-sdk-add-accept-reject-retry** — Add accept/reject/retry variants to clients/ts TransitionCommand (landed 176e9cc)
  model: haiku · size: XS · builds Rust: no · area: clients/ts · deps: none
  files: `clients/ts/src/domain.ts`, `clients/ts/test/client.test.ts`
  change: Add three variants to the TransitionCommand union (domain.ts:460-469) matching tm-server's TransitionRequest::Accept/Reject/Retry (routes.rs:388-404): `{ accept: { note?: string | null; actor: ParticipantId } }`, `{ reject: { reason: string; actor: ParticipantId } }`, `{ retry: { guidance?: string | null; actor: ParticipantId } }`.
  acceptance: New client.test.ts cases call transition() with each of the three variants and assert the exact JSON body posted to the mock fetch. pnpm test and build pass.
  test: `pnpm -C clients/ts install && pnpm -C clients/ts test && pnpm -C clients/ts build`

- [x] **vscode-bundle-and-link-shared-sdk** — Bundle the VS Code extension with esbuild and link @ticketmaster/client (landed ec24e03)
  model: sonnet · size: S · builds Rust: no · area: clients/vscode · deps: none
  files: `clients/vscode/package.json`, `clients/vscode/pnpm-lock.yaml`, `clients/vscode/tsconfig.json`, `clients/vscode/esbuild.mjs`
  change: @ticketmaster/client is ESM-only ("type":"module", exports with import only), while the extension compiles to CommonJS (tsconfig module commonjs, main ./out/extension.js). A plain dependency therefore cannot be require()d on VS Code 1.85's Node. Add "@ticketmaster/client": "link:../ts". Each client has its own pnpm-workspace.yaml, so workspace:* will not resolve. Add an esbuild devDependency and a small esbuild.mjs that bundles src/extension.ts to out/extension.js with format cjs, platform node and `vscode` external. Change the compile script to `tsc --noEmit -p ./ && node esbuild.mjs`. Adjust tsconfig (for example module esnext + moduleResolution bundler, with noEmit) so imports from @ticketmaster/client typecheck. Make no source changes in this task.
  acceptance: After building clients/ts, `pnpm -C clients/vscode install && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test` pass. out/extension.js is a single CJS bundle that still contains require("vscode") and does not contain require("@ticketmaster/client").
  test: `pnpm -C clients/ts install && pnpm -C clients/ts build && pnpm -C clients/vscode install && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test`

## B2 Wiring: in-run index refresh, real events replay; vscode on the shared SDK; macOS transitions; ship v0.1.0

Gate: `mise run verify && pnpm -C clients/ts build && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test && swift test --package-path clients/macos && gh release view v0.1.0`

- [x] **nav-agent-tool-index-refresh** — Refresh the index inside the agent tool path after mutating tools (landed 2a4511a)
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: nav-fix-project-codeintel-freshness
  files: `crates/tm-agent/src/tools.rs`
  change: The agent tool set holds one Arc<CodeIntel> for a whole session or dispatcher lifetime (tools.rs:793; built once at tm-cli dispatch.rs:267 and once per chat session). symbol_index() and search read the files table, which only update_incremental populates. So a file the agent creates mid-run never shows up in search.*/symbol.* results, and chunks for edited files stay stale. Add a dirty flag, set whenever a mutating tool completes (file write, edit, patch apply, shell exec). Before executing any search.*, symbol.* or history.* tool, if the flag is set, call ci.update_incremental(clock) and clear it. On error, warn and continue, the same policy as nav-fix-project-codeintel-freshness. Use the clock the tool set already has, or add one to its constructor.
  acceptance: New tools.rs test in a git tempdir with one commit: a write tool creates new.rs containing `fn fresh_fn`, then a symbol-definition tool call finds fresh_fn (this fails today). Two consecutive search calls with no write between them trigger no refresh, asserted through a counter or a no-op delta.
  test: `mise run test:crate -- tm-agent`

- [x] **replay-fix-events-replay-stub** (landed 663b2b7) — Make `tm events replay` actually replay through materialize::replay
  model: sonnet · size: S · builds Rust: yes · area: replay · deps: none
  files: `crates/tm-core/src/store.rs`, `crates/tm-cli/src/ops.rs`
  change: Add a public `Store::view_as_of(&self, seq: u64) -> tm_types::Result<ProjectView>`. It generalizes the private ticket_and_goal_as_of scratch-replay pattern (store.rs:2639): tempfile EventLog, create_views, materialize::replay, read_view. Rewrite events_replay (ops.rs:2260-2289), whose doc comment currently misdescribes it, to call view_as_of(to) and render ticket counts and states in human mode, or the full view in --json. The live store is never mutated.
  acceptance: A tm-core unit test exercises view_as_of: create two tickets, transition one, and replay to a seq before the transition shows the old state. The tm-cli test events_replay_empty_range still passes, and a new CLI-level test asserts the rendered states rather than an event count.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`

- [x] **vscode-migrate-to-shared-ts-sdk** — Move clients/vscode onto @ticketmaster/client, fixing every wire-shape bug, and delete the stand-in client (landed 048e1f0)
  model: sonnet · size: M · builds Rust: no · area: clients/vscode · deps: vscode-bundle-and-link-shared-sdk, ts-sdk-add-accept-reject-retry
  files: `clients/vscode/src/extension.ts`, `clients/vscode/src/commands/commands.ts`, `clients/vscode/src/commands/commands.test.ts`, `clients/vscode/src/ticketmaster/client.ts`, `clients/vscode/src/ticketmaster/types.ts`, `clients/vscode/src/tree/ticketTreeProvider.ts`, `clients/vscode/src/tree/treeModel.ts`, `clients/vscode/src/tree/treeModel.test.ts`, `clients/vscode/src/leases/leaseDecorationProvider.ts`, `clients/vscode/src/leases/leaseDecorations.ts`, `clients/vscode/src/leases/leaseDecorations.test.ts`, `clients/vscode/src/codelens/decisionCodeLensProvider.ts`, `clients/vscode/src/codelens/decisionCodeLens.ts`, `clients/vscode/src/codelens/decisionCodeLens.test.ts`
  change: Replace every import of ./ticketmaster/client and ./ticketmaster/types with TicketmasterClient and the domain types from @ticketmaster/client, then delete the two stand-in files. The SDK's correct types fix these by construction: lowercase snake_case TicketState/TicketKind/Milestone state literals in treeModel's BADGES and in ticketTreeProvider's "closed" check; the field names closed_by, ttl_seconds, affected_tickets, affected_paths (not affectedDocs) and superseded_by; decisionCodeLens filtering on affected_paths. Fix the mutation commands. claimTicket calls acquireLease(id, {holder, ttl_seconds: 600, actor: holder}). submitWithEvidence first calls attachEvidence (EvidenceKind is snake_case on the wire, e.g. human_attestation), then transition(id, {submit: {summary, evidence: [artifactIds], actor}}). recordDecision uses the SDK's createDecision shape. Update the test fixtures. Absorbs vscode-fix-ticket-enum-casing, vscode-fix-field-name-casing, vscode-fix-lease-request-body and vscode-fix-submit-transition-shape.
  acceptance: src/ticketmaster/ no longer exists and compile (tsc --noEmit plus the esbuild bundle) passes. Tests assert: lowercase state literals in the tree model; a decision with affected_paths ['src/auth/**'] produces a CodeLens for src/auth/x.ts; the claimTicket lease body has ttl_seconds and actor (no ttlSeconds); submit posts {submit:{summary,evidence,actor}} with no top-level trigger. A grep for PascalCase state/kind literals under src/ returns nothing outside comments.
  test: `pnpm -C clients/ts build && pnpm -C clients/vscode install && pnpm -C clients/vscode run compile && pnpm -C clients/vscode test`

- [x] **macos-extend-transition-methods** — Add submit/accept/reject to TicketmasterKit's APIClient (landed b94121d)
  model: sonnet · size: S · builds Rust: no · area: clients/macos · deps: none
  files: `clients/macos/Sources/TicketmasterKit/APIClient.swift`, `clients/macos/Tests/TicketmasterKitTests/ModelDecodingTests.swift`
  change: Add submitTicket(id:summary:evidence:actor:), acceptTicket(id:note:actor:) and rejectTicket(id:reason:actor:) to the TicketmasterAPI protocol and to APIClient. Each POSTs an externally tagged transition body ({"submit": {...}}, {"accept": {...}}, {"reject": {...}}) whose field names match tm-server routes.rs:353-404 exactly, following the activateTicket pattern (APIClient.swift:95-98).
  acceptance: New unit tests assert that the encoded JSON body for each new method matches the server's shape, and swift test passes.
  test: `swift test --package-path clients/macos`

- [x] **release-cut-v0-1-0** — Push main and cut v0.1.0 with release artifacts (already satisfied: tag v0.1.0 exists)
  superseded: release v0.1.0 was published on 2026-09-23 from the parallel session.
  model: haiku · size: XS · builds Rust: no · area: release/packaging (user request) · deps: release-build-artifact-and-install-guide
  files: 
  change: Run only after B1's gate passes on main. Do not commit the untracked HELLO.md or HELLO_VIOLET.md; wait for the owner's answer. Then: `git push origin main`, `git tag -a v0.1.0 -m 'tm v0.1.0'`, `git push origin v0.1.0`, and `gh run watch` the Release workflow run. Confirm with `gh release view v0.1.0` that both target tarballs and SHA256SUMS are attached. Then follow README install path (a) in a fresh `mktemp -d` directory, never the primary checkout: download, verify the checksum, extract and run `./tm --version`. Make no source edits. If the workflow fails, report the failing step's log instead of patching around it.
  acceptance: `gh release view v0.1.0` lists the aarch64-apple-darwin and x86_64-unknown-linux-gnu tarballs plus SHA256SUMS, and the downloaded macOS binary prints its version.
  test: `gh release view v0.1.0 --json assets --jq '.assets[].name'`

## B3 Genesis termination ([new decision: genesis termination]); symbol/history CLI output fixes

Gate: `mise run verify`

- [x] **genesis-stop-infinite-maturity-loop** — Stop tm genesis from looping forever on a failing maturity gate ([new decision: genesis termination]) (landed 2abd15d)
  model: opus · size: M · builds Rust: yes · area: genesis · deps: none
  files: `crates/tm-cli/src/project.rs`, `crates/tm-genesis/src/stages.rs`, `docs/decisions/D-NNN-genesis-cli-stops-for-work.md`, `CLAUDE.md`
  change: The loop is verified: compile.rs:469 commits tickets as Draft, and nothing in genesis() (project.rs:1297-1315) activates them, so V1 never closes. A failed MaturityGate then goes back to Stabilization, which re-enters MaturityGate unconditionally (stages.rs:183-190), and every pass makes a real judge_maturity provider call. Recommended design is exit-and-resume. Extract the CLI loop into a testable function with an explicit stop policy. Stop after Ignition has committed the Draft graph, or at the latest on the first failed MaturityGate, and also when the V0 or V1 stage cannot advance because its milestone is not closed; never spin. On stopping, print or emit the status: N tickets committed under milestone <id>, run `tm sched run` (or `tm run <T>`) to work them, then re-run genesis to resume. Exit 0. Write [new decision: genesis termination], following the D-002 template, with the three alternatives and why exit-and-resume won. Add a CLAUDE.md line on tm genesis behavior.
  acceptance: A test starts the extracted loop from a GenesisState already at Stabilization, with a MockProvider whose default response is a mature=false judgment. It asserts the loop returns Ok within a small bounded number of advances, makes at most one maturity provider call, and reports next steps. A second test asserts that a V0 stage with an unclosed milestone stops rather than spinning. mise run hygiene resolves [new decision: genesis termination].
  test: `mise run test:crate -- tm-cli && mise run test:crate -- tm-genesis && mise run hygiene`

- [x] **nav-fix-cli-symbol-output-bugs** — Fix tm symbol refs/callers/callees output, the double parse, and `tm history why <path>` defaulting to line 1 (landed 9327843)
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-cli/src/search.rs`
  change: (1) symbol_refs (search.rs:~411) and symbol_callers (~455) put an absolute byte offset in ReferenceInfo.col. Compute a real line-relative column, or drop the field. (2) symbol_callers throws away the caller's name and kind; build SymbolInfo the way symbol_callees does (~480-524). (3) In refs/callers/callees, call code_intel.symbol_index() exactly once and resolve the symbol from that index, instead of also calling CodeIntel::resolve_symbol, which builds a second full tree-sitter parse. (4) In history_why (~583-591), when the locator has no :line, use the file's full line range (1..=line_count). Fall back to line 1 only if the file cannot be read, and say so in the output. Absorbs nav-fix-history-why-whole-file-default.
  acceptance: Tests: callers --json includes each caller's name and kind; no field labeled col holds a byte offset; `history why <path>` with no line surfaces a commit that touched only a later line. The handoff includes a timing note showing callers no longer costs about twice as much as def.
  test: `mise run test:crate -- tm-cli`

## B4 Ordered MockProvider sequence; real `tm events tail`

Gate: `mise run verify`

- [x] **genesis-provider-mock-ordered-sequence** (landed 78e516e) — Add a FIFO ordered-response mode to MockProvider
  model: haiku · size: S · builds Rust: yes · area: provider (serves genesis and replay) · deps: none
  files: `crates/tm-provider/src/mock.rs`
  change: Add script_sequence(Vec<Completion>). It answers successive complete() calls in order regardless of request content; once exhausted, it falls through to the existing hash-match, then default, then Unscripted behavior. Follow the existing Mutex<Script> pattern. Also expose sequence_remaining(), and keep recording each served request in the call log so a replay caller can compare request hashes. The later replay-cassette-types task relies on this.
  acceptance: A unit test scripts three distinct completions, issues three non-matching requests, and gets them back in order. A fourth call falls through to the default or Unscripted behavior.
  test: `mise run test:crate -- tm-provider`

- [x] **tel-events-kind-ticket-filter** — Implement tm events tail for real, with --kind/--ticket/--no-follow (landed c04af19)
  model: sonnet · size: S · builds Rust: yes · area: telemetry · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`
  change: events_tail (ops.rs:2199-2211) is a stub: it computes `from`, prints a note and returns. Implement what its doc comment describes. Read events from `from` to the head with EventLog::read_from, in pages. Then, unless --no-follow is given, subscribe (EventLog::subscribe) and stream until ctrl-c, emitting JSON Lines in --json mode. Add --kind (parse with EventKind::from_str and reject unknown kinds with a clear error), --ticket and --no-follow to EventsTailArgs (args.rs:1015). Filter through a pure `fn event_matches(&Event, &EventFilter) -> bool`. The filter part absorbs the original tel-events-kind-ticket-filter.
  acceptance: Unit tests cover event_matches for kind, ticket and combined filters. A --no-follow test over a scratch store with mixed events returns only the matching events and exits. `--kind not.a.real.kind` returns an error.
  test: `mise run test:crate -- tm-cli`

## B5 Genesis offline fixtures; history blame fallback

Gate: `mise run verify`

- [x] **genesis-shared-offline-fixtures** (landed 8594469) — Extract reusable, schema-valid canned JSON for each Genesis provider stage
  model: haiku · size: S · builds Rust: yes · area: genesis · deps: none
  files: `crates/tm-genesis/src/fixtures.rs`, `crates/tm-genesis/src/lib.rs`
  change: Add an always-compiled (not cfg(test)) module in tm-genesis exposing the minimal canned JSON that each provider-calling stage needs (seed, vision, spec, graph compilation, maturity), taken from what crates/tm-e2e/tests/genesis_e2e.rs writes by hand (from line 82 on). Also add `fn offline_sequence() -> Vec<String>` in stage call order, so a CLI mock can feed MockProvider::script_sequence. Leave genesis_e2e.rs as it is.
  acceptance: A tm-genesis unit test deserializes each fixture into the exact type that stage's parser expects and asserts success.
  test: `mise run test:crate -- tm-genesis`

- [x] **nav-fix-history-commits-table-fallback** — Don't silently drop blamed commits that are missing from the commits table (landed 2f6f769)
  model: sonnet · size: S · builds Rust: yes · area: code-navigation · deps: none
  files: `crates/tm-codeintel/src/history.rs`
  change: In HistoryIndex::why (history.rs:214-256), when a blamed sha has no row in the commits table, build its CommitSummary from a live git2 find_commit instead of skipping it.
  acceptance: New test: a commit made after the last ingest_incremental still appears in why()'s results with its sha, author and message.
  test: `mise run test:crate -- tm-codeintel`

## B6 tm genesis honors TM_TEST_MOCK_PROVIDER; docs state persistence

Gate: `mise run verify`

- [x] **genesis-cli-wire-mock-provider** — Honor TM_TEST_MOCK_PROVIDER in resolve_genesis_provider (landed d2fd9e0)
  model: sonnet · size: S · builds Rust: yes · area: genesis · deps: genesis-provider-mock-ordered-sequence, genesis-shared-offline-fixtures, genesis-stop-infinite-maturity-loop
  files: `crates/tm-cli/src/project.rs`, `crates/tm-cli/src/agent.rs`, `CLAUDE.md`
  change: At the top of resolve_genesis_provider (project.rs:1174, before the local-backend probe), check TEST_MOCK_PROVIDER_ENV (agent.rs:1553) and, when it is set, return a MockProvider scripted through script_sequence with tm_genesis::fixtures::offline_sequence(). Put the check in a function that takes the env value as a parameter, so tests never mutate process-global env. Update agent.rs's TEST_MOCK_PROVIDER_ENV doc comment (around 1550-1560), which claims nothing outside agent.rs reads the variable. Update the CLAUDE.md note on offline testing.
  acceptance: A unit test shows the mock branch returns a provider that serves the fixture sequence. Running tm genesis in a scratch tempdir with TM_TEST_MOCK_PROVIDER=1 and no credentials no longer errors on the missing key, and it stops cleanly per [new decision: genesis termination].
  test: `mise run test:crate -- tm-cli`

- [x] **docs-persist-state-across-invocations** — Persist DocState (loaded state + Reconciling) and mark Review tickets human_required (landed 2da693b)
  model: sonnet · size: M · builds Rust: yes · area: docs (SPEC §9) · deps: none
  files: `crates/tm-cli/src/ops.rs`, `crates/tm-docs/src/registry.rs`, `crates/tm-core/src/store.rs`, `crates/tm-cli/src/tickets.rs`
  change: (1) In load_and_sync_doc_registry (ops.rs:50), give each already-registered doc its persisted state and last_verified from project.store, extending Store's doc read path if needed. Only newly discovered docs get DocRecord::new's Unverified default (registry.rs:219-228). (2) In docs_reconcile (ops.rs:187-238), after opening each reconciliation ticket, persist the Reconciling transition through a new Store method that writes both an event and the materialized docs row. Today only the function-local registry is mutated (ops.rs:229-231). (3) For ReconciliationKind::Review (Maintained or Human docs), build ExecutorRequirements with human_required: true; Regeneration keeps default_executor_requirements(). Absorbs docs-persist-reconciling-transition and docs-differentiate-review-vs-regeneration-tickets.
  acceptance: Tests: a doc whose state is persisted as Stale still reads Stale in a fresh docs_list or docs_check call. After docs_reconcile, a fresh docs list shows Reconciling. With one Generated and one Maintained doc both stale, reconcile produces tickets with human_required false and true respectively. The existing docs_check and docs_reconcile tests pass.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`

## B7 tm genesis offline e2e test; mirror push idempotency

Gate: `mise run verify`

- [x] **genesis-cli-offline-integration-test** (landed 9c1e3a8) — Real-binary subprocess test: tm genesis runs offline end to end
  model: sonnet · size: M · builds Rust: yes · area: genesis · deps: genesis-stop-infinite-maturity-loop, genesis-cli-wire-mock-provider
  files: `crates/tm-cli/tests/genesis_offline.rs`
  change: Follow crates/tm-cli/tests/promotion.rs's pattern: Command::new(env!("CARGO_BIN_EXE_tm")) in a git-initialized tempdir, with TM_HOME set to a tempdir, TM_NOTIFY=0 and TM_TEST_MOCK_PROVIDER=1. Run `tm genesis --prompt ...` with a wall-clock timeout guard. Assert exit 0, that stdout reports the committed tickets and the next steps, and that `tm tickets --json --all` shows the committed Draft tickets.
  acceptance: The test passes deterministically in a few seconds, makes no network calls and cannot hang, because the timeout guard fails the test instead of hanging.
  test: `mise run test:crate -- tm-cli`

- [x] **mirror-persist-content-hash-for-idempotency** — Add content_hash to mirror_links so push idempotency survives restarts (landed 143c276)
  model: sonnet · size: M · builds Rust: yes · area: mirror (SPEC §28) · deps: none
  files: `crates/tm-core/src/schema.rs`, `crates/tm-core/src/store.rs`, `crates/tm-cli/src/ops.rs`
  change: Add a content_hash TEXT column to mirror_links (schema.rs:271-277), with a schema-version bump that follows the file's existing migration convention. Thread the column through MirrorLinkRow's read and write helpers in store.rs. Change mirror_push (ops.rs:1870-1893) to look up the existing row and pass Some(MirrorLink{..., content_hash}) to SyncEngine::push instead of None. Update the doc comment that explains why it was None.
  acceptance: A test runs mirror push twice over an unchanged ticket set, and the second run makes zero calls to the recording Tracker adapter. A migration test opens a pre-bump database and succeeds.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`

## B8 tm genesis resume; MCP navigation toolset

Gate: `mise run verify`

- [x] **genesis-cli-resume-flag** — Let tm genesis resume an in-progress run instead of always starting fresh (landed 49288ba)
  model: sonnet · size: S · builds Rust: yes · area: genesis · deps: genesis-stop-infinite-maturity-loop, genesis-cli-offline-integration-test
  files: `crates/tm-cli/src/project.rs`, `crates/tm-cli/src/args.rs`, `crates/tm-cli/tests/genesis_offline.rs`, `CLAUDE.md`
  change: genesis() always calls GenesisState::new (project.rs:1292). Add --resume to GenesisArgs (args.rs:266), and auto-detect a persisted snapshot when --prompt is omitted. Resume through the existing GenesisDriver::resume (stages.rs:520-550) and continue from the persisted stage. Update the next-steps message from [new decision: genesis termination] and CLAUDE.md to name the real flag.
  acceptance: Extend genesis_offline.rs: run genesis until it stops, then run it again with --resume. Exactly one GraphCompilation artifact exists afterward (no duplicate graph), and the run continues from the persisted stage.
  test: `mise run test:crate -- tm-cli`

- [x] **nav-expand-mcp-navigation-toolset** (landed 6fc9c37) — Expose the remaining tm-codeintel read tools over tm-mcp, with a fresh index
  model: sonnet · size: M · builds Rust: yes · area: code-navigation / MCP · deps: nav-fix-project-codeintel-freshness, nav-fix-codeintel-self-reference
  files: `crates/tm-mcp/src/server.rs`
  change: tm mcp and the underscore tool-name scheme have already landed (the tools are ticket_list, search_exact, symbol_def and so on). Add definitions and handlers for search_regex, search_semantic, symbol_references, symbol_callers, symbol_callees, history_why, history_search and history_deleted, following the existing tool_definitions() and execute_tool (server.rs:425-433) pattern. Leave out rename_preview, since it is write-shaped. McpServer::code_intel() (server.rs:251) also never refreshes: call update_incremental with warn-and-continue, the same policy as nav-fix-project-codeintel-freshness.
  acceptance: tool_definitions_names_match_the_dispatch_table and every_tool_name_is_a_valid_host_tool_name still pass. Each new tool has a test over a small fixture project. symbol_def followed by symbol_references and symbol_callers round-trips a real symbol_id in one server session.
  test: `mise run test:crate -- tm-mcp`

## B9 Server schema + TS snapshot; real dollar cost in usage.recorded

Gate: `mise run verify && pnpm -C clients/ts run gen && git diff --exit-code clients/ts/src/generated.ts clients/ts/schema/snapshot.json && pnpm -C clients/ts test && pnpm -C clients/ts build`

- [x] **ts-sdk-schema-snapshot-drift-note** (landed 3207dcb) — Document accept/reject/retry in GET /schema and regenerate the TS snapshot
  model: haiku · size: XS · builds Rust: yes · area: tm-server / clients/ts · deps: ts-sdk-add-accept-reject-retry
  files: `crates/tm-server/src/routes.rs`, `clients/ts/schema/snapshot.json`, `clients/ts/src/generated.ts`
  change: get_schema's TransitionRequest description (routes.rs:595) still reads 'activate|trigger|submit|verify|audit|close|cancel|reopen|fail'; add accept, reject and retry. First check whether the server-fixes track has already landed this, and if so skip the routes.rs edit. Update schema/snapshot.json to match, then run `pnpm -C clients/ts run gen`, which falls back to the snapshot when no server is running, to regenerate src/generated.ts.
  acceptance: The tm-server tests pass, and a second `pnpm gen` produces no diff in generated.ts or snapshot.json.
  test: `mise run test:crate -- tm-server && pnpm -C clients/ts run gen && git diff --exit-code clients/ts/src/generated.ts`

- [x] **tel-completion-cost-field** — Put the real priced cost into usage.recorded (no new Completion field) (landed c0980b7)
  model: sonnet · size: S · builds Rust: yes · area: telemetry · deps: none
  files: `crates/tm-provider/src/fabric.rs`, `crates/tm-agent/src/agent_loop.rs`
  change: Fabric already computes cost_micros (fabric.rs:358-374) and then drops it, and agent_loop hardcodes dollars_micros: 0 at agent_loop.rs:900 and :948 (both inside execute_with_capacity_wait's call path, not the older :785/:812 anchor). Do not add a field to Completion: it has about 40 struct-literal sites across tm-provider, tm-agent, tm-cli, tm-genesis and tm-e2e. Instead: extract `pub fn cost_micros(price: &Price, usage: &Usage) -> u64`; add `Fabric::execute_priced(role, req) -> Result<(Completion, Option<u64>)>` and have execute() delegate to it; thread it through execute_with_capacity_wait (agent_loop.rs:590, called from :921) and set step_spend.dollars_micros from its result (0 when unpriced). Do not cite [new decision: telemetry and cost attribution] yet; it is created in B10. Absorbs tel-wire-real-cost-usage-recorded.
  acceptance: Fabric tests: a priced candidate yields Some(expected), an unpriced one None. Update run_records_usage_recorded_with_the_completions_actual_spend (agent_loop.rs:~1877) to use a priced mock candidate and assert a nonzero, correct dollars_micros.
  test: `mise run test:crate -- tm-provider && mise run test:crate -- tm-agent`

## B10 Cassette format + RecordingProvider ([new decision: record/replay cassettes]); usage attribution ([new decision: telemetry and cost attribution])

Gate: `mise run verify`

- [x] **replay-cassette-types** — Cassette format, RecordingProvider and ordered cassette replay in tm-provider ([new decision: record/replay cassettes]) (landed 8548586)
  model: sonnet · size: M · builds Rust: yes · area: replay · deps: genesis-provider-mock-ordered-sequence
  files: `crates/tm-provider/src/cassette.rs`, `crates/tm-provider/src/lib.rs`, `crates/tm-provider/src/mock.rs`, `docs/decisions/D-NNN-record-replay-harness.md`
  change: New cassette module, JSONL format. The first line is a header {format_version, harness_epoch: Option<u64>, recorded_at}; one entry per call follows: {seq, role, provider_id, request_hash, request, completion}. request_hash is computed over a normalized request in which the project-root or tempdir path prefix is replaced by a placeholder, so identical starting state in a different tempdir hashes the same. Provide Cassette::read_jsonl and write_jsonl. RecordingProvider<P: Provider> appends an entry on each successful complete(); embed() delegates and is not recorded. Add MockProvider::script_from_cassette, which loads entries into the ordered sequence API from genesis-provider-mock-ordered-sequence. It compares each served request's normalized hash with the recorded one and records mismatches, readable through divergences(), instead of failing. Replay is ordered because exact-hash replay into a fresh project diverges on the first call whenever prompts carry paths, timestamps or ids. Write [new decision: record/replay cassettes] (D-002 template) covering the format, ordered replay with divergence reporting, and path normalization; later tasks amend it. Absorbs replay-recording-provider-wrapper and replay-harness-epoch-metadata.
  acceptance: Round-trip test: write a header plus three entries, read them back and get equal values. RecordingProvider over a scripted MockProvider writes two lines that read back matching. Replay serves entries in order, reports exactly one divergence when one recorded request is mutated, and returns Unscripted once exhausted. Two requests that differ only in the root path hash equal. Hygiene resolves [new decision: record/replay cassettes].
  test: `mise run test:crate -- tm-provider && mise run hygiene`

- [x] **tel-usage-payload-model-field** (landed a201af5) — Attribute usage.recorded to provider/model; create [new decision: telemetry and cost attribution]
  model: sonnet · size: M · builds Rust: yes · area: telemetry · deps: tel-completion-cost-field
  files: `crates/tm-events/src/payload.rs`, `crates/tm-core/src/store.rs`, `crates/tm-core/src/materialize.rs`, `crates/tm-agent/src/agent_loop.rs`, `docs/decisions/D-NNN-local-telemetry.md`
  change: Add provider: Option<String> and model: Option<String> to UsageRecordedPayload (payload.rs:342). They must be Option: the payload_kinds! macro cannot carry per-field serde attributes, serde treats a missing Option field as None, and the log is immutable and hash-chained, so old events must keep decoding. Add Store::record_usage_attributed(..., served_by: Option<(String, String)>) and have the existing record_usage delegate to it with None, so tm-core budget.rs's 11 call sites stay untouched. agent_loop passes the provider and model from completion.model. Create [new decision: telemetry and cost attribution] covering real dollars_micros (from tel-completion-cost-field) and these attribution fields, and state that FabricState/LedgerEntry stays process-local (link ops.rs's PROVIDER_LIVE_STATE_NOTE). Later tasks amend [new decision: telemetry and cost attribution].
  acceptance: Old-shape usage.recorded JSON without provider or model still decodes. A payload with the fields round-trips. An agent_loop test asserts the emitted model matches completion.model. Existing budget tests pass untouched, and hygiene resolves [new decision: telemetry and cost attribution].
  test: `mise run test:crate -- tm-events && mise run test:crate -- tm-core && mise run test:crate -- tm-agent && mise run hygiene`

## B11 tool_call.completed events; workspace snapshot restore

Gate: `mise run verify`

- [x] **tel-tool-call-event-kind** — Add and emit tool_call.completed events (landed a275f8c)
  model: sonnet · size: M · builds Rust: yes · area: telemetry · deps: tel-usage-payload-model-field
  files: `crates/tm-events/src/kind.rs`, `crates/tm-events/src/payload.rs`, `crates/tm-agent/src/agent_loop.rs`, `docs/decisions/D-NNN-local-telemetry.md`
  change: Add EventKind::ToolCallCompleted ("tool_call.completed"): its serde rename, Display and FromStr arms, and the exhaustive kind lists in the tests. Follow the command.* precedent for its category. Add ToolCallCompletedPayload {ticket: Option<TicketId>, session: Option<SessionId>, tool_name: String, duration_ms: u64, outcome: String}, where outcome is completed, denied or error, mirroring ToolCallResolution. In agent_loop's tool-call loop (~878-964), append one event per ToolCallRecord, batched with the step's other appends where the code already batches. materialize.rs's `_ => {}` arm already tolerates new kinds. Amend [new decision: telemetry and cost attribution]. Absorbs tel-emit-tool-call-events.
  acceptance: tm-events exhaustiveness and round-trip tests pass. A new agent_loop test runs a scripted step with two tool calls, one succeeding and one denied, and asserts exactly two tool_call.completed events with the right tool_name and outcome.
  test: `mise run test:crate -- tm-events && mise run test:crate -- tm-agent && mise run hygiene`

- [x] **replay-workspace-snapshot-restore** — Add restore_workspace_snapshot (into an isolated git worktree) (landed 4a0757c)
  model: sonnet · size: S · builds Rust: yes · area: replay · deps: none
  files: `crates/tm-scheduler/src/snapshot.rs`
  change: Add `pub fn restore_workspace_snapshot(repo_root: &Path, snapshot: &WorkspaceSnapshot, target_dir: &Path) -> Option<()>`. It runs `git worktree add <target_dir> <snapshot.git_ref>`, following the D-012 convention, and never mutates repo_root. Keep the same infallible Option convention as capture_workspace_snapshot: log and return None on git failure.
  acceptance: A test captures a snapshot from a dirty scratch repo, restores it into a fresh target dir, and asserts the restored contents equal the dirty contents. Restoring against a non-repo returns None.
  test: `mise run test:crate -- tm-scheduler`

## B12 `tm run --record`; TicketMetrics fold

Gate: `mise run verify`

- [x] **replay-cli-record-flag** — tm run <T> --record <path>: capture a cassette and store it as a Transcript artifact (landed 42b49ab)
  model: sonnet · size: M · builds Rust: yes · area: replay · deps: replay-cassette-types, nav-fix-project-codeintel-freshness
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/sched.rs`, `crates/tm-cli/src/dispatch.rs`, `crates/tm-cli/src/agent.rs`, `docs/decisions/D-NNN-record-replay-harness.md`, `CLAUDE.md`
  change: Add --record <path> to RunArgs (args.rs:663). Thread an optional recording sink through build_dispatcher (dispatch.rs:~266) into build_fabric (agent.rs:1574), and wrap every registered provider, the TM_TEST_MOCK_PROVIDER mock included, in RecordingProvider. The header takes the dispatched AgentTask's harness_epoch. When the run finishes, store the cassette bytes as ArtifactKind::Transcript tied to the ticket through Store::store_artifact; this is the first use of that declared-but-unused kind. Amend [new decision: record/replay cassettes]'s Implemented section and add a CLAUDE.md line. Without the flag, behavior is unchanged. Absorbs replay-cassette-artifact-storage.
  acceptance: An integration test in a tempdir (TM_HOME tempdir, TM_TEST_MOCK_PROVIDER=1, TM_NOTIFY=0): `tm ticket new` then `tm run <T> --record <file>` produces a header plus at least one entry, and the ticket has a Transcript artifact whose bytes equal the file.
  test: `mise run test:crate -- tm-cli && mise run hygiene`

- [x] **tel-ticket-metrics-fold** — Derive TicketMetrics purely from the event log (no new metrics event) (landed d276f04)
  model: sonnet · size: S · builds Rust: yes · area: telemetry / bench · deps: tel-usage-payload-model-field, tel-tool-call-event-kind
  files: `crates/tm-harness/src/metrics.rs`
  change: Add `pub fn ticket_metrics_from_events(ticket: &TicketId, events: &[Event]) -> TicketMetrics`. It folds usage.recorded (tokens, dollars, wall time), tool_call.completed (tool_calls, failures) and command.completed (commands; commands_rerun counts repeated identical argv). Add a session-level fold if SessionMetrics fits. Fields that cannot be derived stay 0, with a doc comment saying so. This replaces the bench audit's proposed ticket.metrics_recorded event: that duplicated data already in the log, and its premise was wrong, because tm-harness depends on tm-core, not the reverse, so the payload would have created a dependency cycle.
  acceptance: A fixture event vector folds to an exact expected TicketMetrics. Events for other tickets are ignored, and an empty input gives the default.
  test: `mise run test:crate -- tm-harness`

## B13 `tm run --replay`; stable symbol ids ([new decision: stable symbol ids])

Gate: `mise run verify`

- [x] **replay-cli-replay-flag** — tm run <T> --replay <path>: offline ordered replay with divergence report (landed 5483da7)
  model: sonnet · size: M · builds Rust: yes · area: replay · deps: replay-cli-record-flag
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/sched.rs`, `crates/tm-cli/src/dispatch.rs`, `crates/tm-cli/src/agent.rs`, `docs/decisions/D-NNN-record-replay-harness.md`, `CLAUDE.md`
  change: Add --replay <path>, which conflicts with --record, and --strict-replay. Build the fabric so each recorded role's provider is MockProvider::script_from_cassette, which uses ordered replay; no network is possible. When the run ends, report the divergence count and the first divergent seq, as JSON under --json. Under --strict-replay, any divergence or exhaustion (Unscripted) is a hard error. Amend [new decision: record/replay cassettes] and CLAUDE.md.
  acceptance: Record a mock run in tempdir A, then replay it against a fresh tempdir B with identical starting state. Both reach the same AgentOutcome variant with the same step count and zero divergences, thanks to path normalization. A cassette with one mutated entry reports the divergence, and under --strict-replay it fails.
  test: `mise run test:crate -- tm-cli && mise run hygiene`

- [x] **nav-design-symbol-index-caching-stable-ids** — Stable content-derived symbol ids, optionally with an incremental cache ([new decision: stable symbol ids]) (landed c1ffa5f)
  model: opus · size: M · builds Rust: yes · area: code-navigation · deps: nav-fix-codeintel-self-reference, nav-agent-tool-index-refresh
  files: `crates/tm-codeintel/src/symbols.rs`, `crates/tm-codeintel/src/api.rs`, `docs/decisions/D-NNN-stable-symbol-ids.md`
  change: Ids come from a per-parse positional counter over path-sorted files (symbols.rs ~373-379), and symbol_index() re-parses the whole workspace on every call (api.rs:444-473). Since agent tools now refresh after writes, ids churn within a single turn. Recommended approach: derive each id from blake3(path, container chain, kind, name, ordinal among same-named siblings), truncated to u64. Deliberately leave out the byte range, which moves whenever lines are edited above the symbol. Optionally cache the parsed index and rebuild only the files in IndexDelta. Pick an approach and write [new decision: stable symbol ids] with the tradeoffs: renames and moves change ids, and cache invalidation.
  acceptance: New test: a symbol id in z.rs is unchanged after update_incremental adds a.rs, which sorts first. Existing symbol and rename tests pass.
  test: `mise run test:crate -- tm-codeintel && mise run hygiene`

## B14 `tm stats`; tool-result replay

Gate: `mise run verify`

- [x] **tel-stats-cli-command** — tm stats: per-ticket/day/model/tool rollups over local telemetry (landed b303b89)
  model: sonnet · size: M · builds Rust: yes · area: telemetry · deps: tel-ticket-metrics-fold, tel-tool-call-event-kind, tel-usage-payload-model-field
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/stats.rs`, `crates/tm-cli/src/lib.rs`, `crates/tm-cli/src/main.rs`, `CLAUDE.md`, `docs/decisions/D-NNN-local-telemetry.md`
  change: Add Command::Stats(StatsArgs) with `--by ticket|day|model|tool` (default ticket), `--ticket T` and --json. Read the project's EventLog. Per ticket, use tm_harness ticket_metrics_from_events. Day buckets come from event timestamps. Model grouping uses usage.recorded provider and model, with None shown as 'unattributed'. Tool grouping reports count, failures and average duration_ms. Keep the aggregation in a pure, unit-tested function and render a Table or JSON. Wire it up in main.rs and lib.rs, then amend [new decision: telemetry and cost attribution] and CLAUDE.md. Absorbs tel-stats-tools-rollup.
  acceptance: Unit tests cover the aggregation. Integration test in a tempdir with the mock provider: after tm run, `tm stats --json` shows nonzero tokens for that ticket, and `--by tool` lists at least one tool if the mock script calls one.
  test: `mise run test:crate -- tm-cli && mise run hygiene`

- [x] **replay-tool-replay-mode-design** (landed 36cb4ea) — Replay recorded tool resolutions instead of re-executing tools
  model: opus · size: M · builds Rust: yes · area: replay · deps: replay-cli-replay-flag, tel-tool-call-event-kind
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-agent/src/executor.rs`, `docs/decisions/D-NNN-record-replay-harness.md`
  change: Add a ReplayToolSource option on AgentLoop. In replay mode, before dispatching a real tool call, match it against the recorded ToolCallRecord, first by tool_use_id and then by tool_name plus a normalized input hash, and return the recorded ToolCallResolution verbatim. When nothing matches, fail with a distinct divergence error; never fall through to real execution. Decide and document how authority checks interact with this, and whether recorded resolutions come from session StepRecords or tool_call.completed events. Amend [new decision: record/replay cassettes] with a tool-replay section; do not create a new D number.
  acceptance: A replay of a recorded run through a test-double Executor, asserted to receive zero calls, reaches the same AgentOutcome as the original. An unrecorded tool call produces the divergence error.
  test: `mise run test:crate -- tm-agent && mise run hygiene`

## B15 replay-diff; bench fixture schema

Gate: `mise run verify`

- [x] **replay-diff-outcomes-command** (landed dfb76f1) — tm harness replay-diff <a> <b>: structural diff of two session transcripts
  model: sonnet · size: S · builds Rust: yes · area: replay · deps: replay-cassette-types
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/replay_diff.rs`, `crates/tm-cli/src/lib.rs`, `CLAUDE.md`
  change: Add a HarnessCommand::ReplayDiff subcommand and a dispatch_harness arm in ops.rs. Put the logic in the new replay_diff.rs. Load both session JSON files through the existing session deserialization (agent.rs:215), walk the paired turns and StepRecords, and report assistant_text mismatches, tool-call name, input or resolution mismatches, spend deltas and step-count mismatches. If both inputs carry a harness epoch (a session pin or a cassette header), warn when they differ. The comparison is a pure function; add a CLAUDE.md line.
  acceptance: Unit tests: identical step vectors give an empty diff, and one differing tool resolution reports exactly that step index. Differing epochs produce the warning.
  test: `mise run test:crate -- tm-cli`

- [x] **bench-schema-repo-fixture-fields** — Extend BenchFixture with optional test_command/setup_commands (landed 1268dd2)
  model: haiku · size: S · builds Rust: yes · area: bench · deps: none
  files: `crates/tm-harness/src/bench.rs`
  change: Add `#[serde(default)] test_command: Option<Vec<String>>` and `#[serde(default)] setup_commands: Vec<Vec<String>>` to BenchFixture (bench.rs:20-25), keeping deny_unknown_fields intact. Treat a task that has a test_command and no script.txt as live-only: the scripted (non-live) runner skips it with a note instead of failing, so adding real-repo fixtures cannot break `tm bench run`. Update the module docs. Do not cite [new decision: live benchmark mode], which lands in B17.
  acceptance: hello-world.toml parses unchanged. A TOML with the new fields round-trips. The scripted runner skips a live-only task and still reports hello-world.
  test: `mise run test:crate -- tm-harness`

## B16 Bench report; workflow starters; SWE-lite fixtures

Gate: `mise run verify && bash bench/tools/check-fixtures.sh`

- [x] **bench-report-render** — tm bench report <json> [--out]: render a BenchmarkReport as markdown (landed 25a39ef)
  model: sonnet · size: S · builds Rust: yes · area: bench · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/bench_report.rs`, `crates/tm-cli/src/lib.rs`, `CLAUDE.md`
  change: Add BenchCommand::Report(BenchReportArgs{path, out}) and a dispatch_bench arm, with the rendering in bench_report.rs. Read a BenchmarkReport JSON and emit markdown: aggregate score, then a per-task table of pass/fail, score, tokens, cost, tool_calls and wall time. The rendering function is pure. Add a CLAUDE.md line.
  acceptance: A unit test renders a hand-built BenchmarkReport to the expected markdown, and `tm bench run --out r.json && tm bench report r.json` works in a tempdir.
  test: `mise run test:crate -- tm-cli`

- [x] **workflow-add-missing-starter-definitions** (landed fa074bb) — Add the migrate-sites and research-and-synthesize SPEC §25 starter workflows
  model: sonnet · size: S · builds Rust: yes · area: workflow (SPEC §25) · deps: none
  files: `crates/tm-workflow/fixtures/migrate-sites.toml`, `crates/tm-workflow/fixtures/research-and-synthesize.toml`, `crates/tm-workflow/tests/expand_snapshot.rs`
  change: Write two WorkflowDef fixtures against the existing schema in def.rs. migrate-sites uses a static for_each fan-out. research-and-synthesize uses a FromOutput for_each feeding a join with a merge rule. Add parse and expand tests in expand_snapshot.rs following the review-change and harness-benchmark pattern, including snapshot files if the test uses them.
  acceptance: Both fixtures parse, and expand() produces subgraphs that pass tm-core's invariant checks.
  test: `mise run test:crate -- tm-workflow`

- [x] **bench-swe-lite-fixture-ingestion** (landed ab92a87) — Vendor 3-5 small hermetic bug-fix-with-failing-test bench tasks
  model: sonnet · size: M · builds Rust: no · area: bench · deps: bench-schema-repo-fixture-fields
  files: `bench/tasks/`, `bench/fixtures/`, `bench/solutions/`, `bench/tools/check-fixtures.sh`, `bench/tools/PROVENANCE.md`
  change: Pick 3 to 5 small, permissively licensed bug-fix tasks, each a few MB at most, runnable with toolchains already present: Python via stdlib `unittest` (not `uv run --with pytest`, which fetches pytest over the network on every fresh run, including every gate and every live bench task — hermetic means no network per SPEC §0), or single-crate Rust with no dependencies. Do not attempt the full SWE-bench corpus. For each task: a minimal repo snapshot in bench/fixtures/<id>/, a bench/tasks/<id>.toml using test_command/setup_commands, and a reference fix at bench/solutions/<id>.patch, which lives outside the fixture dir so it is never copied into an agent's workspace. Add check-fixtures.sh, which for each task copies the fixture to mktemp, asserts test_command fails, applies the solution patch, and asserts it passes. Record provenance and license per task in PROVENANCE.md. Do not name any file live-smoke; that name is reserved for B17.
  acceptance: check-fixtures.sh passes for every new task (fails before the patch, passes after), and tm bench list parses them.
  test: `bash bench/tools/check-fixtures.sh`

## B17 Live benchmark mode ([new decision: live benchmark mode]); dollar-aware affordability

Gate: `mise run verify`

- [x] **bench-live-seeded-provider** — tm bench run --live: drive real ticket runs per task and score with real tests ([new decision: live benchmark mode]) (landed 4a5a2a4)
  model: opus · size: M · builds Rust: yes · area: bench · deps: bench-schema-repo-fixture-fields, tel-ticket-metrics-fold, replay-cli-record-flag, nav-agent-tool-index-refresh, nav-fix-project-codeintel-freshness
  files: `crates/tm-cli/src/bench_live.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/lib.rs`, `docs/decisions/D-NNN-live-benchmark-mode.md`, `bench/tasks/live-smoke.toml`, `bench/fixtures/live-smoke/`, `CLAUDE.md`
  change: Add --live to BenchRunArgs (args.rs:888) and a LiveSeededProvider implementing tm_harness::SeededProvider in bench_live.rs. For each task: copy bench/<fixture> into a fresh mktemp dir (per-task isolation; git worktrees are not used), then git init and commit so codeintel history works; run setup_commands; create and activate one ticket with Authority::worker() and run it through sched.rs's real run_ticket path (do not reimplement the AgentLoop wiring), always with a per-task cassette recorded next to the report; after the turn, run test_command and emit tests_pass:<suite> only on exit 0; fill TaskResult tokens, cost and tool_calls from ticket_metrics_from_events over the scratch project's log. is_finished returns true after the one run. Use the mock fabric under TM_TEST_MOCK_PROVIDER and a real one through build_fabric only when configured. Add a purpose-built live-smoke task whose test_command is ["true"]. Write [new decision: live benchmark mode] covering what --live does and its cost and determinism tradeoffs, and add a CLAUDE.md line. Absorbs bench-live-flag-plumbing.
  acceptance: With TM_TEST_MOCK_PROVIDER=1 and a scratch project, `tm bench run --live --filter live-smoke` produces a TaskResult whose passed value comes from actually running test_command, with nonzero tokens and a cassette file. The report includes cost and tool_calls. Without --live, output is byte-identical to today's.
  test: `mise run test:crate -- tm-cli && mise run hygiene`

- [x] **budget-affordability-menu-in-context-pack** — Dollar-aware pre-call affordability, tier-down, and an affordable-tiers menu (SPEC §31) (landed dfa9621)
  model: opus · size: M · builds Rust: yes · area: budget (SPEC §31) · deps: tel-completion-cost-field
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-context/src/sections.rs`, `crates/tm-provider/src/fabric.rs`
  change: The audit's claim that 'no refuse-to-start logic exists' is wrong. A token-only pre-call check already exists (agent_loop.rs:755-770, can_afford and first_unaffordable_dimension), but its estimate hardcodes dollars_micros: 0. Extend the estimate to dollars using candidate prices: prompt tokens times the input price plus MAX_TOKENS_PER_STEP times the output price, via tel-completion-cost-field's cost_micros. Add Fabric::affordable_candidates(role, remaining budget, estimated tokens). When the primary candidate is unaffordable but a cheaper one fits, tier down for that step instead of handing off. Render a compact 'Budget' section listing the affordable tiers with roughly how many steps each allows, as a context-pack section or a system-prompt addendum, whichever fits the compile path. Document the estimate formula in doc comments.
  acceptance: With a MockProvider-backed Fabric holding two priced candidates and a nearly exhausted dollar budget, the step routes to the cheaper candidate, the menu names only affordable tiers, and a step no candidate can afford hands off instead of being attempted.
  test: `mise run test:crate -- tm-provider && mise run test:crate -- tm-context && mise run test:crate -- tm-agent`

## B18 Cross-tool benchmark ([new decision: cross-tool benchmark]); promotion baseline persistence

Gate: `mise run verify`

- [x] **bench-cross-tool-comparison-harness** (landed 85cf57c) — Head-to-head tm vs opencode/Codex/Claude Code benchmark runner ([new decision: cross-tool benchmark])
  model: opus · size: M · builds Rust: yes · area: bench · deps: bench-live-seeded-provider, bench-report-render, bench-swe-lite-fixture-ingestion
  files: `crates/xtask/src/bench_cross.rs`, `crates/xtask/src/main.rs`, `docs/decisions/D-NNN-cross-tool-benchmark.md`, `bench/README.md`, `CLAUDE.md`, `mise.toml`
  change: Add an xtask subcommand, plus a mise bench:cross task, that runs the bench/tasks suite identically through `tm bench run --live` and through each configured external CLI, as sketched in docs/backlog.md:498-517. Credentials default to DevPass; real-Claude auth needs an explicit opt-in flag. Each external tool gets a per-task mktemp copy, runs the same test_command scoring, and has its tokens, wall time and tool calls captured where the tool reports them. Store results as real ticket and event-log state in a scratch project, as the backlog asks, and render them with bench report's shape. Real external runs are opt-in and never part of verify; unit tests use a fake tool adapter. Write [new decision: cross-tool benchmark] and add CLAUDE.md and mise lines. Scope depends on the owner's answer about which tools and credentials to use.
  acceptance: Unit tests with a fake adapter produce a comparable report for tm and one other tool on live-smoke. A documented manual command runs a real comparison when credentials exist.
  test: `cargo test -p xtask -j 2 && mise run hygiene`

- [x] **bench-promotion-baseline-persistence** (landed 32af768) — Persist a benchmark per promoted harness epoch as the automatic baseline
  model: sonnet · size: M · builds Rust: yes · area: bench / harness · deps: mirror-persist-content-hash-for-idempotency
  files: `crates/tm-core/src/schema.rs`, `crates/tm-core/src/store.rs`, `crates/tm-core/src/materialize.rs`, `crates/tm-cli/src/ops.rs`
  change: When promoting an epoch (the promote flow at ops.rs ~1257-1360), persist the resolved BenchmarkReport alongside the harness_epochs row: add a benchmark_json column materialized from the promotion event's payload, with a schema-version bump that follows mirror-persist-content-hash's migration. Store::harness_epochs() returns the stored report. When --baseline is not given, promote defaults to the previous epoch's stored report.
  acceptance: A tm-core test promotes with a report and harness_epochs() returns it. A tm-cli test promotes a second epoch without --baseline and the gate compares against the first epoch's report.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-cli`

## B19 Genesis lifecycle events; doc staleness trigger

Gate: `mise run verify`

- [x] **genesis-emit-lifecycle-events** — Append genesis.* events on each stage transition (landed 62018f1)
  model: sonnet · size: M · builds Rust: yes · area: genesis · deps: genesis-stop-infinite-maturity-loop
  files: `crates/tm-core/src/store.rs`, `crates/tm-genesis/src/stages.rs`
  change: The genesis.started, genesis.stage_entered and genesis.stage_completed kinds already exist (tm-events kind.rs:273-280). GenesisDriver cannot append them only because Store exposes no API for it (stages.rs:12-19). Add a narrow Store method, e.g. record_genesis_stage(stage, artifact ref, actor), and call it from GenesisDriver::advance. Update stages.rs's module doc.
  acceptance: After advance() runs, the project's event log, read the way tm events reads it, has one stage event per completed stage.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-genesis`

- [x] **docs-wire-real-staleness-trigger** (landed a24541c) — Feed tm_docs::Assessor a real git ChangeSet so tm docs check can fail
  model: sonnet · size: M · builds Rust: yes · area: docs (SPEC §9) · deps: docs-persist-state-across-invocations
  files: `crates/tm-cli/src/ops.rs`, `crates/tm-docs/src/assess.rs`, `crates/tm-docs/src/provenance.rs`
  change: In docs_check and docs_reconcile, build a ChangeSet from a git2 diff between each doc's last-verified commit (from doc_provenance, recorded at verify or attest time) and HEAD plus the working tree. Run it through Assessor::assess before Assessor::check, and persist the resulting state through the storage added in docs-persist-state-across-invocations. Update ops.rs's admission comment (146-149).
  acceptance: Editing a file matched by a doc's derived_from glob makes tm docs check exit 1 and name that doc Stale. Editing an unrelated file exits 0.
  test: `mise run test:crate -- tm-docs && mise run test:crate -- tm-cli`

## B20 Genesis explicit V0/V1; tm docs attest

Gate: `mise run verify`

- [x] **genesis-explicit-v0-v1-milestone-marking** — Select V0/V1 milestones by explicit tag, not position (landed 962b3a7)
  model: sonnet · size: M · builds Rust: yes · area: genesis · deps: genesis-emit-lifecycle-events, genesis-shared-offline-fixtures
  files: `crates/tm-genesis/src/compile.rs`, `crates/tm-genesis/src/spec.rs`, `crates/tm-genesis/src/stages.rs`, `crates/tm-genesis/src/fixtures.rs`
  change: Thread a release marker (v0 or v1) from Specification's ReleaseDefinitions through the graph-compilation prompt and its JSON schema onto the committed milestones. Make the marker optional in the schema so older fixtures keep working, and update fixtures.rs to include it. Change Stage::Ignition and Stage::MaturityGate (stages.rs:430-437, 459-467) to select by marker, falling back to the current positional heuristic only when no marker exists.
  acceptance: A test with milestones ordered so the positional heuristic would pick wrong confirms the tagged milestone is selected. The existing genesis and e2e tests pass.
  test: `mise run test:crate -- tm-genesis && mise run test:crate -- tm-e2e`

- [x] **docs-wire-attestation-cli-path** (landed d49d1c2) — tm docs attest <doc> --note: close a Review ticket with a human Attestation
  model: sonnet · size: S · builds Rust: yes · area: docs (SPEC §9) · deps: docs-persist-state-across-invocations
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-docs/src/reconcile.rs`, `CLAUDE.md`
  change: Add a DocsCommand::Attest variant and its dispatch_docs arm. Build a tm_docs::reconcile::Attestation, record it as Evidence{kind: HumanAttestation} against the doc's open Review ticket, close that ticket (human actor only), set the doc's persisted state to Fresh and record the new last-verified commit. Add a CLAUDE.md line.
  acceptance: On a doc with an open Review ticket, attest closes the ticket, writes a HumanAttestation evidence row, and a following docs list shows Fresh.
  test: `mise run test:crate -- tm-cli`

## U — Opus audit 2026-09-25 (docs/audits/2026-09-25-opus-audit.md)

## U1 — Found in the 2026-09-25 end-to-end audit (/tmp/tm-audit/AUDIT.md)

- [x] **u1-agent-search-exact-limit** (landed 82a40ad) — Cap agent/MCP `search.exact`/`search.regex` results (default 50) and add `limit` + `path_glob` params
  model: sonnet · severity: critical · builds Rust: yes · area: agent-tools · deps: none
  files: `crates/tm-agent/src/tools.rs`, `crates/tm-codeintel/src/exact.rs`, `crates/tm-codeintel/src/api.rs`, `crates/tm-mcp/src/server.rs`
  change: `ToolName::SearchExact`/`SearchRegex` in tools.rs (grep `ToolName::SearchExact =>`) call `self.ci.search_exact(needle)` which uses `DEFAULT_HIT_CAP = 1000` (exact.rs:30), with the full `line_text` of every hit going to the model. Add optional `limit` (default 50, max 200) and `path_glob` (a glob filter applied during the walk) to both tool schemas and to `CodeIntel::search_exact`/`search_regex` (new `*_with` variants taking an options struct, so existing callers stay the same). Truncate each `line_text` to 240 chars. When results are cut, return `"truncated": true` plus `"total_seen"` and a hint string: "narrow the query or pass path_glob". Apply the same defaults to the MCP `search_exact`/`search_regex` tools.
  acceptance: In a repo with more than 1000 occurrences of `provider`, one agent `search.exact {"query":"provider"}` returns at most 50 hits and `truncated:true`; `{"query":"provider","path_glob":"crates/tm-cli/**","limit":10}` returns at most 10 hits, all under crates/tm-cli. Unit tests cover the limit, the glob and the line clipping.
  test: `mise run test:crate -- tm-agent && mise run test:crate -- tm-codeintel && mise run test:crate -- tm-mcp`
  evidence: A dogfood ticket on a clone of this repo spent 1,025,330 tokens over 18 tool calls (`tm stats`). Right after its first `search.exact` call a single request was 67,870 tokens, and every later request was 36k–94k tokens. It never edited a file. Measured directly: `tm search --exact provider --limit 100000 --json` on the clone returns 1000 hits and 177,808 bytes (about 45k tokens), which is what one agent `search.exact` call hands the model. The MCP tool and CLI default are already capped at about 20 hits; only the agent tool is uncapped.

- [x] **u1-context-pack-fit-sections** (landed 8969884) — Fit context-pack sections to their share (top-k until full) instead of dropping them whole
  model: sonnet · severity: critical · builds Rust: yes · area: context · deps: none
  files: `crates/tm-context/src/pack.rs`, `crates/tm-context/src/sections.rs`
  change: `assemble` (pack.rs, grep `"exceeds remaining token budget"`) either admits a section whole or drops it. Make retrieval-like sections (SearchHits, Wiki, Conventions, SymbolOutlines, GitHistory) truncatable: have `RawSection` carry ordered items, and admit items in order until the section's share or the remaining budget is used up, recording a `DroppedSection` only for the items left out, with a reason like "trimmed 41 of 60 hits to fit". In sections.rs, cap the search-hits builder at 20 hits of at most 40 lines each before assembly, and give Conventions a head-truncation (the first N lines of AGENTS.md/CLAUDE.md plus "…truncated"). Derive expected values in tests from `SectionKind` shares, not hardcoded quotients (CLAUDE.md convention).
  acceptance: `tm ticket context <T>` on a clone of this repo admits a non-empty "Search hits" and "Project conventions" section. The total stays within the pack budget, and "Left out to fit the budget" lists only trimmed items. New unit tests show a 100-hit section admitted partially, not dropped.
  test: `mise run test:crate -- tm-context`
  evidence: `tm ticket context T-1` on /tmp/tm-audit/self admitted 478 tokens across 7 sections, and dropped "Search hits — would have needed ~93427 tokens", "Project conventions — ~12197 tokens" and "Wiki pages — ~1771 tokens". The worker got no retrieval context at all.

- [x] **u1-provider-table-follows-env** (landed 01a371e) — Stop freezing env-detected providers into providers.toml at `tm init`; re-detect at load, and add `tm provider reset`
  model: sonnet · severity: critical · builds Rust: yes · area: providers · deps: none
  files: `crates/tm-provider/src/role_config.rs`, `crates/tm-cli/src/ops.rs`, `crates/tm-cli/src/args.rs`, `docs/providers.md`
  change: `RoleTable::default_table_with(devpass_model)` (role_config.rs, grep `fn default_table_with`) is written to `.tm/providers.toml` by `tm init` using whatever env vars exist at init time. A project initialised without DEVPASS_* never routes to devpass, even after the key is set later. Fix: (1) have `tm init` record that providers.toml was generated. Check first whether `RoleTable::parse`'s `collect_roles` would treat a top-level `generated = true` as a role; if it would, use a `[meta]` table it skips or a sibling `.tm/providers.generated` marker file; (2) in the effective-table loader, when `generated = true`, merge env-detected candidates (devpass, and any other `Registry::autodetect` provider that is ready) ahead of not-configured ones at load time; (3) add `tm provider reset` to regenerate the file from the current environment (keeping a `.bak`). A hand-edited file (no `generated` key) is never touched. Update docs/providers.md.
  acceptance: In a fresh dir: `tm init` without DEVPASS_*, then with `.env` loaded, `tm -p "Reply hello"` replies via devpass, and `tm run T-1` routes coder.fast to devpass. `tm provider reset` rewrites the table and prints what changed. Unit tests cover the merge and the hand-edited opt-out. Projects created before this change have no marker, so only `tm provider reset` fixes them; `tm doctor` should suggest it when the table routes to unconfigured providers while a configured one is ready.
  test: `mise run test:crate -- tm-provider && mise run test:crate -- tm-cli`
  evidence: /tmp/tm-audit/split: `tm init` (no .env), then `set -a; . .env; tm -p ...` gave `no usable model is configured for coder.fast`, and `tm run T-1` gave `no candidate can serve role coder.fast: provider not registered: anthropic`. The same binary in /tmp/tm-audit/probe (init with .env loaded) works.

- [x] **u1-genesis-reasoning-model-tokens** (landed 2c78e3c) — Genesis stages must survive reasoning models: raise max_tokens and retry once on an empty/MaxTokens reply
  model: sonnet · severity: critical · builds Rust: yes · area: genesis · deps: none
  files: `crates/tm-genesis/src/seed.rs`, `crates/tm-genesis/src/vision.rs`, `crates/tm-genesis/src/spec.rs`, `crates/tm-genesis/src/compile.rs`, `crates/tm-genesis/src/maturity.rs`
  change: Every stage hard-codes a small `max_tokens` (seed.rs:199 and vision.rs:136 use 2048; maturity.rs:253 uses 1024). With devpass's reasoning model (`muse-spark-1.3-contributor`), `analyze_prompt` gets a completion with no text block and fails with `parse: completion has no text content`. That fits (but does not prove) the truncation compat.rs:921 documents, where the gateway returns `finish_reason:"incomplete"`/null content when reasoning uses up max_tokens. First confirm with `RUST_LOG=tm_provider=trace` or a `--record` cassette; if it is not MaxTokens, handle the actual stop reason the same way. Add one helper in tm-genesis (e.g. `complete_text(provider, req)`) that: uses max_tokens of at least 16384; if the completion has no text and `StopReason::MaxTokens`, retries once with double the tokens; and on final failure returns a plain error naming the model and saying the reply was empty or truncated. Route all five stages through it. Add mock-provider tests for the empty-then-success path.
  acceptance: With `.env` loaded (devpass only), `tm genesis --plain --prompt "A todo CLI in Python with pytest tests"` in an empty git repo gets past "reading the prompt" and commits a ticket graph. The unit test for the retry passes.
  test: `mise run test:crate -- tm-genesis`
  evidence: /tmp/tm-audit/genesis1.log and genesis2.log both show `genesis: reading the prompt` then `error: parse: completion has no text content` (exit 1, about 30s), reproducibly, on the only live provider.

- [x] **u1-genesis-activates-its-graph** — `tm genesis` should activate the tickets it commits (or offer `--run`) so `tm sched run` actually works them (landed 6f5cce7)
  model: sonnet · severity: critical · builds Rust: yes · area: genesis · deps: u1-genesis-reasoning-model-tokens, u1-hygiene-spec-refs-in-user-strings
  files: `crates/tm-cli/src/project.rs`, `crates/tm-genesis/src/compile.rs`, `docs/decisions/D-NNN-genesis-activates-its-graph.md (next free number)`, `docs/decisions/D-027-genesis-cli-stops-for-work.md`
  change: `run_genesis_stages` stops with "N tickets committed under milestone M-1. Run `tm sched run` (or `tm run <T>`) to work them, then re-run `tm genesis --resume`". The tickets are Draft, and `tm sched plan`/`tick` ignore drafts, so that advice is a dead end. Change it: after GraphCompilation, activate the committed V0 tickets (Draft to Ready/Blocked through `Store::activate`, as `ticket.activate` does). Add a `--run` flag that then drives the in-process scheduler (`sched::spawn_background_runner` or the `tm sched run` loop) until the V0 milestone closes or a ticket escalates, and resumes the genesis stages after that. Without `--run`, print the exact next command, which must now work. This reverses an accepted D-027 choice, so write a new decision doc (next free number, [new decision: genesis activates its graph] at the time of writing; check `ls docs/decisions`) that supersedes that part, and add only a "Superseded in part by D-0NN" line to D-027's status.
  acceptance: Under `TM_TEST_MOCK_PROVIDER=1`, `tm genesis --prompt "x"` leaves its tickets `ready` (not `draft`), and `tm sched plan` then lists lease actions. `tm genesis --run --prompt "x"` under the mock reaches past V0. The e2e test in tm-cli covers both.
  test: `mise run test:crate -- tm-cli && mise run test:crate -- tm-genesis`
  evidence: /tmp/tm-audit/genmock: genesis committed T-1/T-2 as `draft`. `tm sched plan` then printed "No scheduler actions planned" and `tm sched tick` printed "No work to do right now".

- [~] **u1-fabric-error-copy-not-configured** (needs the owner: this batch's editor died without reporting; its diff left an unresolved `use crate::providers::Registry;` import (should be `crate::Registry`) and did not compile, so it was reverted) — Say "not configured (ANTHROPIC_API_KEY is not set)" instead of "provider not registered: anthropic"
  model: haiku · severity: high · builds Rust: yes · area: providers · deps: u1-provider-table-follows-env
  files: `crates/tm-provider/src/fabric.rs`
  change: fabric.rs:406 and :485 (grep `provider not registered`) produce `no candidate can serve role coder.fast: provider not registered: anthropic`. Replace them with plain wording that names the missing credential env var when known (from `Registry::known_providers`), lists which candidates were tried, and ends with the fix: "Set ANTHROPIC_API_KEY, or run `tm provider reset` to route to a provider that is configured (devpass is ready)." Update any test asserting the old string.
  acceptance: `tm run T-1` in a project whose roles only name anthropic, with no ANTHROPIC_API_KEY, prints the new message, naming the env var and a ready alternative when one exists.
  test: `mise run test:crate -- tm-provider`
  evidence: The dogfood run's first attempt died with `provider was unavailable: provider: no candidate can serve role coder.fast: provider not registered: anthropic`. Anthropic is a known provider; it just had no key.

- [x] **u1-worker-submit-nudge** (landed 6f05bc1) — When a ticketed run ends with plain text, send one reminder turn before failing the attempt
  model: sonnet · severity: high · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-agent/src/executor.rs`
  change: In `AgentLoop::drive` (grep `"model ended turn without submitting"` in executor.rs and the matching path in agent_loop.rs), when a ticketed task (no `conversation`) gets a text-only assistant turn, append one user message ("You ended your turn without calling ticket.submit. If the work is done and verified, call ticket.submit with evidence now; if not, continue working.") and continue. Do this at most once per attempt (twice if configurable), and only then fail with the existing message. Record the nudge in the step transcript.
  acceptance: A MockProvider script (text-only turn, then a `ticket.submit` call) ends `AgentOutcome::Submitted`. Two text-only turns in a row still fail with "model ended turn without submitting".
  test: `mise run test:crate -- tm-agent`
  evidence: Dogfood T-1 (/tmp/tm-audit/dogfood2.log) ran 18 tool calls in 253s, then failed with `something went wrong: model ended turn without submitting`, with no edits. Claude Code/Codex never lose a run's work to this: their loop ends on a text turn and the harness decides what happens next.

- [x] **u1-run-progress-shows-args** (landed 21829b3) — Show what each step did in `tm run`'s live output (the command, path or query), not "Ran a command"
  model: haiku · severity: high · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/agent.rs`
  change: `tm run T --plain` prints lines like `* Ran a command`, `* Read a file`, `* Searched the code`, `* Read a file -> error: couldn't read or write a file`. The step printer is `plain_tool_action` (agent.rs:2280-2314, grep `"Ran a command"`) and include the salient argument, clipped to about 80 chars: the shell command, the file path (plus the line range for read_range), the search query, and for errors the path and error kind. It already receives the tool name; pass the call's `input` too, and keep the plain verb as the prefix.
  acceptance: `tm run` on a mock-scripted ticket prints `* Ran \`cargo test -p tm-cli\``, `* Read crates/tm-cli/src/project.rs:2200-2280`, and `* Searched for "providers"`. A snapshot test covers the formatter.
  test: `mise run test:crate -- tm-cli`
  evidence: /tmp/tm-audit/dogfood2.log: 18 steps, none of which says what was run or read, including a failed read with no path.

- [x] **u1-wire-potion-embedder** — Use `open_at_auto` (Potion when cached) on every real command path, and store the embedder's identity in the index so a change triggers a re-embed (landed b2ffbb2)
  model: sonnet · severity: high · builds Rust: yes · area: code-intel · deps: u1-agent-search-exact-limit, u1-genesis-resume-checks-snapshot-first
  files: `crates/tm-codeintel/src/api.rs`, `crates/tm-codeintel/src/store.rs`, `crates/tm-cli/src/project.rs`, `crates/tm-cli/src/dispatch.rs`, `crates/tm-mcp/src/server.rs`, `crates/tm-genesis/src/attach.rs`
  change: No production code calls `CodeIntel::open_auto`/`open_at_auto`. `Project::code_intel` (project.rs:91), `doctor` (project.rs:2221), `open_and_refresh_code_intel` (dispatch.rs:447), `McpServer::code_intel` (tm-mcp server.rs:386) and `attach_repository` (attach.rs:430) all use hash-only `open_at`. The embed.rs:222 doc comment says "real command paths use" it, which is false. (1) Persist `embedder_id` (e.g. `hash-v1`, `potion-code-16M-v2`) in the index DB's meta table. On open with a different embedder, clear the vector table and let `update_incremental` re-embed. (2) Switch those five call sites to `open_at_auto(.., None)`. Tests keep using `open`/`open_at` (hash) so they stay network-free (hygiene). Tests that go through `Project::code_intel` would otherwise pick Potion on this machine and hash on CI, so force hash in the test harness (`TM_EMBEDDER=hash` or the `config_override` argument) to keep them deterministic. (3) Correct the embed.rs comment and D-025's "no real command path calls open_auto yet" note.
  acceptance: With the Potion model cached in `~/.cache/huggingface/hub/models--minishlab--potion-code-16M-v2` (it is on this machine), `tm search --semantic "where does the scheduler decide a ticket is ready"` on a clone of this repo puts a `crates/tm-scheduler` hit in the top 5, where today it returns flat 0.51 scores from docs. `TM_EMBEDDER=hash` restores the old behaviour. A unit test covers the embedder-change re-embed.
  test: `mise run test:crate -- tm-codeintel && mise run test:crate -- tm-cli`
  evidence: `tm search --semantic "where does the scheduler decide a ticket is ready"` on /tmp/tm-audit/self returned D-008, D-023, TASKS.md and thesis.md, all scoring 0.51–0.52, and no readiness code (`crates/tm-scheduler/src/select.rs`). The model cache directory exists but is never used.

- [x] **u1-decider-wire-shadow-triage** — Wire the configured `Role::Decider` into `Store` at project open, with a `DecisionProvider`→`TriageDecider` adapter (landed e3a27a9)
  model: sonnet · severity: high · builds Rust: yes · area: providers (D-020) · deps: u1-init-builds-index
  files: `crates/tm-provider/src/decide.rs`, `crates/tm-cli/src/project.rs`, `docs/decisions/D-020-system-one-decision-providers.md`
  change: `Store::with_decider` (store.rs:616) and `Registry::build_decider` (registry.rs:421) are only ever called from tests. No production code builds a decider, and nothing implements `TriageDecider` for a `DecisionProvider`, so the "landed" d20-shadow-triage task never fires outside unit tests. (1) In tm-provider (or tm-cli, whichever avoids a tm-core→tm-provider dependency; tm-core must not depend on tm-provider), add `TriageAdapter { inner: Arc<dyn DecisionProvider>, runtime handle }` implementing `tm_core::store::TriageDecider`, which builds the triage `DecideRequest` (kind + routing questions) and maps the top answer. (2) In project open (grep `Store::open_at` in project.rs), when the effective role table's `decider` candidate is not `mock`, or when `TM_DECIDER_SHADOW=1`, build it through `Registry::build_decider` and call `with_decider`. By default the mock writes no `classify.decided` events. (3) Update D-020's status line to say what is actually wired.
  acceptance: With `providers.toml` `[decider]` set to a `systemone-http` candidate pointing at a local stub HTTP server (test), `tm ticket new "x"` appends `ticket.created` and `classify.decided` with `disposition:"shadow"`. With the default config it appends no `classify.decided`. A decider HTTP error still creates the ticket.
  test: `mise run test:crate -- tm-provider && mise run test:crate -- tm-cli`
  evidence: `rg -ln "with_decider|build_decider|TriageDecider|DecisionProvider" crates` finds production callers nowhere, only the definitions, store.rs tests and registry.rs tests. `tm provider list` shows `decider mock mock-decider not-configured`.

- [x] **u1-automatic-verification-step** (landed 64c3d16) — After a submit, run the ticket's declared verification commands and record pass/fail evidence, keeping the ticket in Submitted
  model: sonnet · severity: high · builds Rust: yes · area: scheduler · deps: none
  files: `crates/tm-scheduler/src/dispatch.rs`, `crates/tm-e2e/tests/ticket_lifecycle_e2e.rs`
  change: SPEC.md:721-724 ("Verification separation", non-negotiable) and :308 (`Submitted -> Verifying` automatically) have no implementation. Nothing creates a Verification ticket or runs checks, and `Store::verify` is only reachable from the HTTP route (routes.rs:846). First slice, with no state-machine change: in `report_outcome` (dispatch.rs, grep `store.submit(ticket`), after a successful submit, when the ticket's `VerificationPolicy` names commands (or the project's `harness.toml` has a `verify_command`), run them in the ticket's workspace with the existing command executor as `system`. Attach the output as a verification evidence record with `passed: bool`, and leave the ticket in `Submitted` so `Store::accept`/`reject` (which begin with `Trigger::VerificationStarted` from Submitted) keep working. If the check fails, reject it through the existing `Store::reject` path as `system`, with the failing output as the reason, so it goes back through retry. Don't call `Store::verify` here: it takes a verifier `TicketId` and is subject to `AuditorMustDiffer`, which is part of task u1-verification-state-and-review.
  acceptance: Mock-provider e2e tests: with verification command `false`, a submitted ticket gets rejected by `system` with the command output and returns to the retry path; with `true`, it stays in Submitted with a passing verification evidence record, and `tm ticket accept` still closes it.
  test: `mise run test:crate -- tm-scheduler && mise run test:crate -- tm-e2e`
  evidence: `rg "store\.verify\(|TicketKind::Verification"` outside tests finds only routes.rs:846 and label/mirror code. Every "Ready for review" ticket today depends entirely on the model's own claim that it ran the tests.

- [x] **u1-dogfood-e2e-smoke** (landed e9967fe) — Add a mise task that runs one real ticket against a scratch clone of this repo and reports submit/tokens/wall time
  model: sonnet · severity: high · builds Rust: no · area: harness · deps: u1-agent-search-exact-limit, u1-context-pack-fit-sections, u1-worker-submit-nudge
  files: `scripts/dogfood-smoke.sh`, `mise.toml`, `CLAUDE.md`
  change: Add `scripts/dogfood-smoke.sh` (POSIX sh, as in disk-guard.sh): clone the primary checkout into `mktemp -d` via `git clone file://…`, `tm init`, `mise trust`, create a fixed small ticket (the doctor providers-warn fix), and run `tm run T-1 --plain --record <tmp>/cassette` under a wall-clock bound. Print the final state, `tm stats` tokens/tool calls, and `git diff --stat`, and exit non-zero unless the ticket reached Submitted. Use DevPass from `.env` only through `set -a; . .env` inside the script, never echoing it. Add `mise run dogfood` and a CLAUDE.md line. Not part of verify.
  acceptance: `mise run dogfood` prints a one-line verdict like `SUBMITTED in 212s, 380k tokens, 14 tool calls, 2 files changed` or a clear failure, and cleans up its tempdir.
  test: `sh -n scripts/dogfood-smoke.sh && mise tasks | grep dogfood`
  evidence: Nothing in the repo measures "can tm work on itself". The audit's manual dogfood took about 6 calls to set up and exposed 4 critical bugs.

- [x] **u1-edit-anchor-by-text** (landed befd198) — Let `edit.apply_patch` anchor on exact old text (search/replace) and reject byte ranges that don't split on line boundaries the model saw
  model: sonnet · severity: high · builds Rust: yes · area: agent-tools · deps: u1-agent-search-exact-limit
  files: `crates/tm-agent/src/patch.rs`, `crates/tm-agent/src/tools.rs`
  change: The model edits with `{"edits":[{"byte_start","byte_end","replacement"}]}` (patch.rs `Edit`, around line 25-63). In dogfood T-2, a correct 3-line change to `provider_doctor_detail` also duplicated a phrase in the next doc comment (`a smell, not a regardless -- a 1x1 workflow is a smell, not a`), which is the classic off-by-N byte-offset failure. First check whether the edit-hash-fix track has landed a fix (`git log --oneline -- crates/tm-agent/src/patch.rs`); if it has, only add the regression test below. Otherwise add an `old_text` form (`{"path","old_text","new_text"}`, which must match exactly once, like Claude Code's Edit), make it the schema's preferred form, and keep byte ranges as a fallback that must also carry the `old_text` they replace, verified before applying.
  acceptance: A unit test replays the T-2 shape (a byte range off by several bytes with `old_text` supplied) and gets a clear "old_text does not match at that range" error instead of corruption. A unique `old_text` replacement succeeds; an ambiguous one fails with the match count.
  test: `mise run test:crate -- tm-agent`
  evidence: /tmp/tm-audit/self `git diff` after T-2: the second hunk corrupts the doc comment at project.rs:~2134.

- [x] **u1-decider-provider-status** — Show the decider role correctly in `tm provider list/status/test` (not "not-configured" for the mock) and add `tm provider test decider` (landed 4ed98ed)
  model: haiku · severity: medium · builds Rust: yes · area: providers (D-020) · deps: u1-decider-wire-shadow-triage
  files: `crates/tm-cli/src/ops.rs`
  change: `tm provider list` prints `decider  mock  mock-decider  1  not-configured`. Report the mock as `offline (mock)`, and a systemone candidate as `ready`/`missing AI_GATEWAY_API_KEY`. Extend `tm provider test` so `tm provider test decider` sends one triage `DecideRequest` through the configured decider and prints the answers with confidences.
  acceptance: `tm provider list` shows `offline (mock)` for the default decider, and `tm provider test decider` against the mock prints a triage answer and exits 0.
  test: `mise run test:crate -- tm-cli`
  evidence: /tmp/tm-audit/probe `tm provider list` output, last row.

- [x] **u1-verification-state-and-review** — Move auto-verified tickets through `Verifying` and adapt accept/reject, the tickets-screen peek and the server transitions to match (landed 404e4e6)
  model: sonnet · severity: medium · builds Rust: yes · area: core · deps: u1-automatic-verification-step, u1-hub-shortcuts-panel-tabs
  files: `crates/tm-core/src/store.rs`, `crates/tm-server/src/routes.rs`, `crates/tm-tui/src/screens/tickets.rs`, `SPEC.md`
  change: Once u1-automatic-verification-step records verification evidence, drive `Submitted -> Verifying -> Auditing` through `Store::verify` using a system verifier id that satisfies `AuditorMustDiffer`. Make `Store::accept`/`reject` accept a ticket in `Verifying`/`Auditing` as well as `Submitted` (grep `Trigger::VerificationStarted` in accept/reject). Show the verification result in the peek ("checks passed: cargo test -p x"). Correct SPEC §4/§11 to describe what is implemented, and keep the V-*/A-* ticket split as documented future work.
  acceptance: Core unit tests: accept and reject work from Submitted, Verifying and Auditing; invariants hold. A TUI snapshot shows the check result in the peek.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-server && mise run test:crate -- tm-tui`
  evidence: store.rs:1205-1313 `accept`/`reject` both start with `step(t.state, Trigger::VerificationStarted)`, so they are only valid from Submitted.

- [x] **u1-genesis-maturity-without-verification-tickets** (landed 83b6d99) — Make the maturity gate's verification-pass-rate predicate use the verification evidence that actually exists
  model: sonnet · severity: medium · builds Rust: yes · area: genesis · deps: u1-automatic-verification-step
  files: `crates/tm-genesis/src/maturity.rs`
  change: `evaluate_predicate` (maturity.rs:86-159) filters `t.kind == TicketKind::Verification`, and no code path creates such tickets, so the pass rate is computed over nothing. Count the verification evidence/`ticket.verified`/`ticket.verification_failed` events on Work tickets in the window instead (keep counting Verification tickets too, for later). Define explicitly what an empty window means (fail, with the reason "no verified work yet") and test it.
  acceptance: Unit tests: a window with 3 verified-passed and 1 failed Work ticket gives a 0.75 pass rate; an empty window fails with a readable reason.
  test: `mise run test:crate -- tm-genesis`
  evidence: maturity.rs:95 filters on `TicketKind::Verification`, and `rg TicketKind::Verification crates/tm-genesis/src crates/tm-scheduler/src` shows no creator.

- [x] **u1-hub-board-display-labels** (landed 51f43b8) — Use the tickets screen's display labels on the Kanban board, and show the tab strip there
  model: sonnet · severity: medium · builds Rust: yes · area: tui · deps: none
  files: `crates/tm-tui/src/screens/kanban.rs`, `crates/tm-cli/src/tui.rs`
  change: The board (Tab or Ctrl+B from the tickets screen) shows raw state names as columns: `draft (0) blocked (0) ready (0) leased (0) active (0)` (only 5 were visible at 120 cols; confirm whether the other states are cut off). There is no header or tab strip, and it breaks D-024's one-set-of-display-labels rule. Use the same group labels as the tickets screen (Needs input / Working / Ready for review / Queued / Completed) as columns, render the shared header and tab strip (Tickets · Board · Milestones · Timeline · Graph) with Board highlighted, and make columns fit the width.
  acceptance: In a 120x36 PTY, the board shows 5 labelled columns, all visible, under the tab strip. A render snapshot test at 120 cols covers it.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: The TUI trial (terminal-mcp, /tmp/tm-audit/chatgen): Tab from the tickets screen rendered only `Left/Right: columns ... draft (0) blocked (0) ready (0) leased (0) active (0)`.

- [x] **u1-chat-header-unusable-model** (landed d274642) — Don't advertise a model the chat can't use: flag the header and welcome box when the default model has no credential
  model: sonnet · severity: medium · builds Rust: yes · area: tui · deps: u1-provider-table-follows-env, u1-hub-board-display-labels
  files: `crates/tm-cli/src/tui.rs`, `crates/tm-tui/src/screens/chat.rs`
  change: Opening `tm` in a project whose default model is `anthropic/claude-sonnet-5`, with no ANTHROPIC_API_KEY, shows `model: anthropic/claude-sonnet-5` in the welcome box and status line with no warning. The first message then fails with `no usable model is configured for coder.fast`. At startup, check the effective chat model's availability (the same check `tm provider status` uses). When unavailable, show `model: anthropic/claude-sonnet-5 (not set up — /connect)` in the welcome box and a warning-coloured dot in the status line.
  acceptance: A render test with an unavailable model shows the "not set up" hint, and with an available one it shows the plain model.
  test: `mise run test:crate -- tm-tui && mise run test:crate -- tm-cli`
  evidence: The TUI trial in /tmp/tm-audit/chatgen without .env: the welcome box said `model: anthropic/claude-sonnet-5`, then `hi` gave `✗ The turn could not run: provider: no usable model is configured for coder.fast`.

- [x] **u1-ticket-kind-task-alias-api** (landed afd35c7) — Accept `task` as an alias of `work` everywhere (HTTP API, MCP) since the CLI help advertises it
  model: haiku · severity: medium · builds Rust: yes · area: server · deps: u1-provider-table-follows-env
  files: `crates/tm-core/src/ticket.rs`, `crates/tm-cli/src/args.rs`
  change: `tm ticket new --help` says `--kind <KIND> Ticket kind (task, investigation, verification, audit, recovery, ...) [default: task]`, but `POST /tickets {"kind":"task",...}` returns 400 `unknown variant \`task\`, expected one of \`work\`, ...`, and lists show `work`. Add `#[serde(alias = "task")]` on `TicketKind::Work` and make the args.rs help say `work (alias: task)` with default `work`.
  acceptance: `curl -X POST /tickets -d '{"kind":"task","objective":"x","actor":"human:a"}'` returns 201, and the help text says `work`. A serde round-trip test covers the alias.
  test: `mise run test:crate -- tm-core && mise run test:crate -- tm-server`
  evidence: `tm serve` trial: `{"error":"bad_request","message":"This ticket's request body is invalid: unknown variant \`task\`..."}`.

- [x] **u1-hygiene-spec-refs-in-user-strings** — Extend hygiene to flag `SPEC.md §`/`D-NNN` references inside user-facing string literals, and fix the current ones (landed 8476ca7)
  model: sonnet · severity: medium · builds Rust: yes · area: harness · deps: u1-doctor-providers-warn, u1-ticket-kind-task-alias-api
  files: `crates/xtask/src/hygiene.rs`, `crates/tm-cli/src/drive.rs`, `crates/tm-cli/src/workflow.rs`, `crates/tm-cli/src/project.rs`, `crates/tm-computer/src/macos.rs`, `crates/tm-cli/src/args.rs`
  change: Users see these strings: drive.rs:142 ("See SPEC.md §19.1a for examples"), workflow.rs:267 and project.rs:2153 ("prompt wearing a costume (SPEC.md §25.3)"), macos.rs:633 (`tm doctor` prints "... (SPEC.md §20.3)"), and `tm acp --help` ("for editors like Zed (`SPEC.md` §28.1)"). Hygiene only checks args.rs doc comments for D-NNN/crates/tm_*:: jargon. Extend it to flag `SPEC.md §` inside string literals in non-test code and inside args.rs `///` help text (allowlist mechanism as for D-NNN). Rewrite each hit in plain words (e.g. an inline browser.toml example instead of the SPEC pointer). Update the test at drive.rs:588 accordingly.
  acceptance: `mise run hygiene` fails on a planted `"see SPEC.md §1"` literal and passes on the fixed tree. `tm doctor` and `tm acp --help` contain no "SPEC.md".
  test: `mise run hygiene && mise run test:crate -- xtask && mise run test:crate -- tm-cli`
  evidence: The `tm doctor` output on /tmp/tm-audit/probe ends "...both target the active login session (SPEC.md §20.3)". `tm acp --help` first line.

- [x] **u1-doctor-providers-warn** (landed cbad8eb) — `tm doctor`'s providers row says `ok` while its detail says "no model provider is ready"; make it `warn`
  model: haiku · severity: medium · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/project.rs`
  change: In `doctor` (project.rs, grep `"providers"` near the doctor checks), set status `warn` when no provider is ready. Also reword the first-index detail `repaired incremental drift: 634 added ...` to `indexed 634 files (3146 chunks, 502 commits)` when the index was empty before, and in a repo with no commits, report `index-health` as `warn: no commits yet` instead of `FAIL git2: reference 'refs/heads/main' not found`.
  acceptance: Unit tests for all three wordings/statuses. `tm doctor` in a fresh `git init` dir with no commits exits 0, with a warn row.
  test: `mise run test:crate -- tm-cli`
  evidence: /tmp/tm-audit/self `tm doctor`: `providers ok ... no model provider is ready`, and `index-health ok repaired incremental drift` on the very first index. /tmp/tm-audit/probe (fresh `git init`): `index-health FAIL storage: git2: reference 'refs/heads/main' not found`, and doctor exits non-zero.

- [x] **u1-bare-text-starts-chat** — `tm "fix the bug"` should open the chat with that prompt (as `claude "…"` does), not error with "--prompt required" (landed 14fc4d2)
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: u1-hygiene-spec-refs-in-user-strings
  files: `crates/tm-cli/src/main.rs`, `crates/tm-cli/src/args.rs`
  change: `tm "some words"` today fails with `error: the following required arguments were not provided: --prompt`. When `[TEXT]` is given without `-p` in an interactive terminal, start the TUI chat with TEXT submitted as the first message. Without a TTY, behave like `-p`. Update `--help`'s TEXT description.
  acceptance: An args-parse unit test: `tm hello` parses to "interactive with initial prompt". `echo | tm hello` (no TTY) runs a one-shot turn.
  test: `mise run test:crate -- tm-cli`
  evidence: A mis-quoted subcommand (`tm "acp --help"`) returned `error: the following required arguments were not provided: --prompt  Usage: tm --prompt <TEXT>`, which is confusing wording for the most natural invocation.

- [x] **u1-bench-cross-stdin-closed** — Cross-tool bench: run external CLIs with stdin closed; report tm's tokens and each tool's model (landed a902066)
  model: sonnet · severity: medium · builds Rust: yes · area: bench · deps: none
  files: `crates/xtask/src/bench_cross.rs`
  change: `opencode run` hung for the full 300s timeout when stdin was an open pipe, and passed in 14s with `< /dev/null`. `claude -p` printed "no stdin data received in 3s". In each `ToolAdapter::run`, spawn with `Stdio::null()` for stdin. Also record per-tool `model` and `tokens`/`cost` where the tool reports them (claude `--output-format json` usage/total_cost_usd, tm `--json -p` tokens), so the report isn't model-confounded without saying so.
  acceptance: The unit test's fake adapter command, which blocks on stdin, completes. The rendered report has Model and Tokens columns.
  test: `mise run test:crate -- xtask`
  evidence: /tmp/tm-audit/h2h/results.txt: `opencode rc=124 wall=302.2s FAIL`, then the rerun with `< /dev/null`: `rc=0 wall=14s OK`.

- [x] **u1-init-builds-index** — Build the code index during `tm init` (with a progress line) so the first search/doctor/run isn't a silent 60s stall (landed 505d253)
  model: sonnet · severity: medium · builds Rust: yes · area: cli-ux · deps: u1-wire-potion-embedder
  files: `crates/tm-cli/src/project.rs`
  change: `tm init` on this repo takes 0s and builds no index. The first `tm doctor` took 57s ("repaired incremental drift: 634 added ... 3146 chunks"), and a first `tm run` pays the same cost inside the attempt. After creating the project, run `update_incremental` with a one-line progress/summary ("Indexed 634 files in 41s"). Add `--no-index` to skip.
  acceptance: `tm init` in a clone prints the indexed-files summary, and a following `tm doctor` shows index-health with 0 added.
  test: `mise run test:crate -- tm-cli`
  evidence: /tmp/tm-audit/self: `init took 0s`, `doctor 57s`.

- [x] **u1-context-cost-report** — Show per-step context size in `tm stats --by tool`/`tm ticket show` so context blowups are visible (landed 2e2c49a)
  model: sonnet · severity: medium · builds Rust: yes · area: telemetry · deps: u1-worker-submit-nudge
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-events/src/payload.rs`, `crates/tm-cli/src/stats.rs`
  change: `tool_call.completed` records the tool name and duration but not the result size, so finding which tool call blew the context up needed a manual correlation with `usage.recorded`. Add `result_bytes` (and `truncated: bool`) to the `tool_call.completed` payload (an optional field, so old logs still replay), and add a `Result KB` column and a `Max request tokens` column to `tm stats --by tool`/`--by ticket`.
  acceptance: After a mock run with one big tool result, `tm stats --by tool` shows its bytes. Old event logs still materialise (a replay test).
  test: `mise run test:crate -- tm-events && mise run test:crate -- tm-cli`
  evidence: The dogfood forensics needed `sqlite3 .tm/project.db` to see that 18 calls cost 1.03M tokens. `tm stats` only showed `T-1 S-2 18 1025330 not priced 203`.

- [x] **u1-record-flush-incremental** (landed 218abf3) — `tm run --record` must write the cassette incrementally so a killed or timed-out run still leaves one
  model: sonnet · severity: medium · builds Rust: yes · area: replay · deps: none
  files: `crates/tm-provider/src/cassette.rs`, `crates/tm-cli/src/agent.rs`, `crates/tm-cli/src/sched.rs`, `crates/tm-cli/src/dispatch.rs`
  change: `tm run T-2 --record /tmp/x.cassette` killed by SIGTERM at a 580s bound left no file at all. Two layers need the fix, not one:
    1. `tm-provider/src/cassette.rs`: add a `CassetteWriter` that fsyncs the header immediately and each entry as it's appended, and make `Cassette::read_jsonl` tolerate a truncated last line (treat it as ended at the last complete entry, not a parse error) — a partial implementation of exactly this already exists on branch `salvage/stopped-agent-agent-a799b001ca1b99336-20260925190935` (an externally-stopped subagent's unfinished, unverified work; review before reusing, don't merge blindly).
    2. The real `tm run --record` path does NOT go through `tm_provider::RecordingProvider` at all — it uses its own separate `RecordingProviderWrapper` (`crates/tm-cli/src/agent.rs` ~line 1777) backed by an in-memory-only `CassetteSink` (`type CassetteSink = Arc<Mutex<Vec<CassetteEntry>>>`, `agent.rs` ~line 1755). `crates/tm-cli/src/sched.rs` ~line 520 creates that sink empty, and ~line 677-700 is the ONLY place it's ever written to disk (`Cassette::write_jsonl`, once, presumably on successful completion) — so a kill before that point loses everything, matching the bug exactly. The salvaged branch's fix does nothing for this real path since it never touches `RecordingProviderWrapper`/`RecordingSpec`/`sched.rs`. Open a `CassetteWriter` alongside the sink (in `dispatch.rs`/wherever `RecordingSpec` is actually constructed for a real `--record` invocation, ~`dispatch.rs:303`) and have `RecordingProviderWrapper::complete` (`agent.rs` ~line 1785) append to it too, mirroring whatever pattern `cassette.rs`'s own fix uses.
  acceptance: A test writes 3 entries, drops the recorder without finishing it, and reads back 3 entries; `tm run --replay` on a truncated cassette replays up to the cut and then reports the divergence. Separately, an integration test (or a manual isolated-tempdir check) confirms a real `tm run --record <path>` killed with SIGTERM mid-run leaves a valid partial cassette file — this is the part the salvaged branch's fix does not cover.
  test: `mise run test:crate -- tm-provider && mise run test:crate -- tm-cli`
  evidence: /tmp/tm-audit/dogfood3.log ends `TIMEOUT after 580s / EXIT 124`, and `ls /tmp/tm-audit/dog2.cassette` gives No such file.

- [x] **u1-run-sigterm-releases-lease** (landed 2ecbb9c) — On SIGINT/SIGTERM, `tm run` should release its lease and record an interrupted attempt instead of leaving the ticket `active`
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: u1-run-progress-shows-args
  files: `crates/tm-cli/src/agent.rs`, `crates/tm-cli/src/sched.rs`
  change: After `tm run T-2` was killed with SIGTERM, `tm ticket list` still shows `T-2 work active`. A prior, externally-stopped subagent got this ~95% done on branch `salvage/stopped-agent-agent-a3eaaae5ebb3f0ec8-20260925190937` — review it before writing this from scratch: it adds `InterruptWatcher` (SIGINT+SIGTERM via `tokio::signal`) and `handle_run_interrupt` to `sched.rs`'s `run_ticket` poll loop, which on the first signal calls `Store::record_failure(FailureClass::ExecutorCrash, "interrupted by user")` (releases the lease and drives the normal retry-vs-escalate path — reuses existing machinery rather than reinventing lease release) and exits 130, and on a second signal exits immediately without waiting on the cleanup task. It also adds `TEST_MOCK_PROVIDER_BLOCK_ENV`/`ScriptedMockProvider.block` (`agent.rs`) so a mock completion can be made to never resolve, specifically to support a deterministic integration test. That test itself — `crates/tm-cli/tests/run_interrupt.rs`, referenced by the salvaged code's own doc comments — was never actually written before the subagent was stopped; this is the one real gap left.
  acceptance: The integration test described above exists and passes: spawn `tm run` against the blocking mock (`TM_TEST_MOCK_PROVIDER_BLOCK=1`), send SIGTERM once genuinely in flight, then see the ticket `ready` with a failure record containing `interrupted by user`. A second-signal case (immediate exit, no wait on cleanup) should also get a test if the salvaged branch doesn't already cover it.
  test: `mise run test:crate -- tm-cli`
  evidence: The dogfood T-2 kill left `T-2 work active 0 - In crates/tm-cli/src/project.rs, ...` in `tm ticket list`.

- [x] **u1-hub-shortcuts-panel-tabs** — List Tab/Shift+Tab (switch view) in the tickets screen's `?` panel, and fix the misleading "enter to collapse" footer hint (landed 3d0e49c)
  model: haiku · severity: low · builds Rust: yes · area: tui · deps: u1-hub-board-display-labels
  files: `crates/tm-tui/src/screens/tickets.rs`
  change: The tickets screen's `?` panel lists `ctrl+b open the Kanban board`, but not Tab/Shift+Tab, which `tab_cycle_key` (crates/tm-cli/src/tui.rs:618) binds to cycle Tickets/Board/Milestones/Timeline/Graph. The footer says "enter to collapse" while the dispatch input has focus and there are no tickets. Add a `tab / shift+tab  switch view` row, and make the footer hint depend on focus ("enter to dispatch" when the input is non-empty or the list is empty).
  acceptance: The `?` panel shows the tab row, and the footer reads "enter to dispatch" on an empty project. A snapshot test covers both.
  test: `mise run test:crate -- tm-tui`
  evidence: The TUI trial: the `?` panel on the tickets screen had no Tab row, and the footer said `enter to collapse · esc to go back` on an empty list.

- [x] **u1-tui-overlay-swallows-quit** — Ctrl+C twice must quit from any overlay (shortcuts panel, peek), as it does from the base screen (landed ad6a043)
  model: haiku · severity: low · builds Rust: yes · area: tui · deps: u1-chat-header-unusable-model
  files: `crates/tm-cli/src/tui.rs`
  change: With the tickets screen's `?` panel open, Ctrl+C, Ctrl+C left the TUI running with the panel still open. Route Ctrl+C to the app-level quit handler (first press closes the overlay and arms quit, second press quits) before overlay key handling.
  acceptance: Key-sequence tests: (`?`, Ctrl+C, Ctrl+C) on the tickets screen, and (Ctrl+C, Ctrl+C) on the base chat screen after leaving the board with Esc, both end the app loop.
  test: `mise run test:crate -- tm-cli`
  evidence: The TUI trial: after `?` then Ctrl+C twice, `getContent` still showed the Shortcuts panel. Later, after Esc from the board back to the chat, Ctrl+C twice also left the chat open, and only Ctrl+D quit.

- [x] **u1-history-why-no-commits-copy** — `tm history why <file>` says "repository may be corrupted" for an untracked file or a repo with no history (landed 807c4ee)
  model: haiku · severity: low · builds Rust: yes · area: cli-ux · deps: u1-wire-potion-embedder
  files: `crates/tm-codeintel/src/history.rs`, `crates/tm-cli/src/search.rs`
  change: Map the "file has no commits / unborn HEAD / untracked" cases to "todo.py has no git history yet (it isn't committed)". Keep "may be corrupted" only for real git2 corruption errors.
  acceptance: A test in a tempdir repo with an untracked file gets the new message.
  test: `mise run test:crate -- tm-codeintel && mise run test:crate -- tm-cli`
  evidence: /tmp/tm-audit/chatgen: `tm history why todo.py` gave `error: storage: Unable to read file history — repository may be corrupted.`

- [x] **u1-genesis-resume-checks-snapshot-first** — `tm genesis --resume` with no snapshot should say so before demanding a provider (landed bfc25d2)
  model: haiku · severity: low · builds Rust: yes · area: genesis · deps: u1-genesis-activates-its-graph
  files: `crates/tm-cli/src/project.rs`
  change: `tm genesis --resume` in a project that never ran genesis errors with `provider: ANTHROPIC_API_KEY is not set ... no local model provider is reachable`. Resolve the snapshot first and fail with "No stopped genesis run to resume here. Start one with `tm genesis --prompt \"…\"`."
  acceptance: A unit test: resume with no snapshot and no provider gives the new message.
  test: `mise run test:crate -- tm-cli`
  evidence: /tmp/tm-audit/chatgen: `tm genesis --resume` gave the provider error.

- [x] **u1-mcp-protocol-version** — `tm mcp` should negotiate the client's protocol version (2025-06-18) rather than always answering 2024-11-05 (landed 7d998cb)
  model: haiku · severity: low · builds Rust: yes · area: mcp · deps: u1-wire-potion-embedder
  files: `crates/tm-mcp/src/server.rs`
  change: `PROTOCOL_VERSION` is hard-coded to "2024-11-05" (tm-mcp server.rs:53; client.rs:23 and capability.rs:178 are the client side, leave them). `initialize` with `protocolVersion:"2025-06-18"` got `"protocolVersion":"2024-11-05"` back. Echo the client's version when it is one tm supports (2024-11-05, 2025-03-26, 2025-06-18); otherwise answer the latest supported. Keep newline-delimited framing.
  acceptance: A unit test covers all three versions and an unknown one.
  test: `mise run test:crate -- tm-cli`
  evidence: The `tm mcp` stdio trial, first response line.

- [x] **u1-workflow-list-copy** — `tm workflow list` header `NAME NODES PARAMS 1X1?` and empty state; `tm templates list` empty state (landed 2e8eaf3)
  model: haiku · severity: low · builds Rust: yes · area: cli-ux · deps: u1-provider-table-follows-env, u1-hygiene-spec-refs-in-user-strings
  files: `crates/tm-cli/src/workflow.rs`, `crates/tm-cli/src/ops.rs`
  change: With no workflows, print "No workflows yet. Starters: `tm workflow new --from <starter>`" (list the real starters) instead of a bare header, and rename the `1X1?` column to `Single-ticket`. `tm templates list` prints `No templates declared in /…/templates.toml`. Instead, list the built-in template catalog (B21) and say where to add project templates.
  acceptance: Snapshot tests for both empty states.
  test: `mise run test:crate -- tm-cli`
  evidence: /tmp/tm-audit/chatgen outputs of `tm workflow list` and `tm templates list`.

- [x] **u1-cli-help-global-flags-once** — Stop repeating the 5 global flags in every subcommand's `--help` (landed 0d1207b)
  model: sonnet · severity: low · builds Rust: yes · area: cli-ux · deps: u1-bare-text-starts-chat
  files: `crates/tm-cli/src/args.rs`
  change: Every subcommand help (`tm ticket new --help`, `tm run --help`, ...) lists `--json --quiet --no-color --plain --project` with their full paragraphs, pushing the command's own options off screen. Keep them `global = true`, but put them under a separate `help_heading = "Global options"` and shorten their text (e.g. `--project <PATH>  Use this project root`), with the long form only in `tm --help`.
  acceptance: `tm run --help` shows the command's own options first and the global ones under "Global options", each one line.
  test: `mise run test:crate -- tm-cli`
  evidence: `tm run --help` / `tm ticket new --help` output in the audit: the global flags' 2-3 line paragraphs are mixed in with `--worktree`/`--record`.

- [x] **u1-fix-false-codeintel-docs** — Correct the doc comments and docs that claim Potion is used on real paths (landed 1d2c477)
  model: haiku · severity: low · builds Rust: yes · area: docs · deps: u1-wire-potion-embedder
  files: `crates/tm-codeintel/src/embed.rs`, `crates/tm-codeintel/src/lib.rs`, `docs/decisions/D-025-potion-semantic-embedder.md`
  change: After u1-wire-potion-embedder lands, reconcile embed.rs:222 ("which real command paths use"), lib.rs:12, and D-025's "What this costs" with the new call sites and the embedder-id re-embed behaviour.
  acceptance: The wording matches the code, and `mise run hygiene` passes.
  test: `mise run hygiene`
  evidence: embed.rs:222 claims real command paths use `open_auto`, but no caller exists.

- [x] **u1-bench-cross-permission-flags** — Cross-tool bench: run each competitor headless with its no-prompt mode, only inside the throwaway task copy (landed 9965cc0)
  model: sonnet · severity: high · builds Rust: yes · area: bench · deps: u1-bench-cross-stdin-closed
  files: `crates/xtask/src/bench_cross.rs`
  change: The claude/codex/opencode adapters pass no permission flags, so headless runs stall or get refused and tm wins by default. Add `--permission-mode bypassPermissions` to `claude -p`, `--sandbox workspace-write` to `codex exec`, and OpenCode's non-interactive auto-approve flag (check `opencode run --help`). Always set the working dir to the per-task scratch copy, never the repo.
  acceptance: The adapter command lines include these flags (unit test on the built argv); the report states each tool's permission posture.
  test: `mise run test:crate -- xtask`
  evidence: docs/audits/2026-09-25-bench-plan.md "Required fixes", item 0.

- [x] **u1-bench-cross-model-timeout-cost** — Cross-tool bench: pin one model per cohort across tools, a per-task timeout and a cost cap (landed 9965cc0)
  model: sonnet · severity: high · builds Rust: yes · area: bench · deps: u1-bench-cross-permission-flags
  files: `crates/xtask/src/bench_cross.rs`
  change: Add `--model <provider/model>` passthrough (claude `--model`, codex `-m`, opencode `-m`, tm via a scratch `providers.toml` role candidate), `--task-timeout <secs>` (kill the process group; mark TIMEOUT) and `--max-cost-usd` (stop scheduling new tasks once reported spend reaches it). Record the tool versions (`--version`) in the report.
  acceptance: Unit tests for the argv per tool, a fake adapter that sleeps past the timeout is marked TIMEOUT, and the report header lists the versions, model and caps.
  test: `mise run test:crate -- xtask`
  evidence: docs/audits/2026-09-25-bench-plan.md "Required fixes", items 1-3.

- [~] **u1-bench-polyglot-subset** — Vendor a 20-exercise Aider Polyglot subset as bench tasks (needs another pass: the worker made no changes)
  model: sonnet · severity: medium · builds Rust: no · area: bench · deps: none
  files: `bench/tasks/polyglot-*.toml`, `bench/fixtures/polyglot-*/`
  change: Per docs/audits/2026-09-25-bench-plan.md Track A: 20 exercises across Python/JS/Go/Rust, each fixture with the stub and the tests and a task TOML in the existing bench/tasks format (copy py-binary-search-bound.toml's shape). Record the upstream commit and licence in a bench/fixtures/POLYGLOT-SOURCE.md.
  acceptance: `tm bench` lists the 20 new tasks; each fixture's tests fail before a fix (check 3 by hand).
  test: `mise run test:crate -- tm-harness`
  evidence: docs/audits/2026-09-25-bench-plan.md Track A recommendation.


## B21 Genesis template catalog; tm acp

Gate: `mise run verify`

- [x] **genesis-wire-template-catalog** — Pass a real template catalog into GenesisDriver's graph compilation (landed 655e822)
  model: sonnet · size: M · builds Rust: yes · area: genesis · deps: genesis-explicit-v0-v1-milestone-marking, genesis-cli-resume-flag
  files: `crates/tm-genesis/src/stages.rs`, `crates/tm-cli/src/project.rs`, `crates/tm-templates/src/lib.rs`
  change: stages.rs:401-416 passes `&[]` as the catalog. Recommended: tm-templates exposes a bundled catalog (manifests from templates/starter embedded at compile time) with a TM_TEMPLATES_DIR runtime override. GenesisDriver takes the catalog as a constructor argument, and the CLI supplies it. Remove the 'no catalog wired' comment.
  acceptance: A test advances GraphCompilation with a non-empty catalog and a Specification whose prose matches one template's tags, and GraphSummary.selected_template is populated. tm-templates' starter tests still pass.
  test: `mise run test:crate -- tm-templates && mise run test:crate -- tm-genesis && mise run test:crate -- tm-cli`

- [x] **acp-wire-tm-acp-serve-command** — tm acp: serve the project as an ACP agent over stdio (landed 55414ec)
  model: sonnet · size: M · builds Rust: yes · area: clients / ACP (SPEC §28.1) · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/acp.rs`, `crates/tm-cli/src/main.rs`, `crates/tm-cli/src/lib.rs`, `CLAUDE.md`
  change: Add Command::Acp(AcpArgs), modeled on McpArgs, and a new acp.rs modeled on mcp.rs. Open the project with project::open_for_command, build tm_acp::AcpServer::new(Arc::new(ProjectAgentBackend::new(project.store.clone(), ids))), attach stdin and stdout, keep diagnostics on stderr, and await until stdin closes. Add a main.rs arm next to Command::Mcp and a CLAUDE.md line. tm-cli already depends on tm-acp (dispatch.rs uses AcpExecutor).
  acceptance: A subprocess test in a tempdir project (TM_TEST_MOCK_PROVIDER=1) sends initialize on stdin and receives a valid InitializeResponse, mirroring tm-acp's server_roundtrip.rs.
  test: `mise run test:crate -- tm-cli`

## B22 Swift symbol navigation

Gate: `mise run verify`

- [x] **nav-add-swift-language-support** — tree-sitter Swift support so clients/macos gets symbol navigation (landed 4824777)
  model: sonnet · size: M · builds Rust: yes · area: code-navigation · deps: nav-design-symbol-index-caching-stable-ids
  files: `crates/tm-codeintel/src/walk.rs`, `crates/tm-codeintel/src/symbols.rs`, `crates/tm-codeintel/Cargo.toml`, `Cargo.toml`, `Cargo.lock`
  change: Add Language::Swift and map .swift to it in from_extension (walk.rs:58-69). Add a tree-sitter-swift dependency pinned compatibly with the workspace's tree-sitter version, and wire grammar_for. Write symbol and reference queries covering func, class, struct, enum, protocol and extension declarations and call sites, applying the same self-reference exclusion. Ids come from the [new decision: stable symbol ids] scheme.
  acceptance: On a scratch Swift file, outline lists the function, def resolves it, and refs finds exactly the call site.
  test: `mise run test:crate -- tm-codeintel`

## D-020 (accepted 2026-09-23)

The owner approved D-020 on 2026-09-23 (asked directly for Jev/Laya support), superseding the
earlier "6 owner-gated batches, not scheduled until the owner brings D-020 back" note (journal of
`wf_87a85411-848`). The shadow-mode slice is now scheduled as tasks `d20-decider-trait-and-mock`
through `d20-flip-status-and-amend-spec` in section T above; see `docs/backlog.md`'s "Accepted,
2026-09-23 (D-020 system-one decision providers)" entry for the Jev/Laya research this scheduling
is based on.

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

## V — Found by trials and audits

- [x] **t20260925-1314-BurntSushi-ripgrep-3376-recover-without-repeating-agent-investigation** (landed 76ca45d, see D-036)
  model: sonnet · severity: high · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/executor.rs`, `crates/tm-agent/src/agent_loop.rs`
  change: When a ticketed model turn ends without submission, include concise prior findings in the retry context and detect repeated tool/file reads; after a repeated no-progress attempt, ask for a targeted reproduction or produce a specific diagnosis instead of restarting the same investigation.
  acceptance: A simulated no-submit ticket whose next attempt repeats the same file reads is steered to a different action or stops with a user-actionable explanation, while normal retries continue to work.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260925-1314/BurntSushi-ripgrep-3376/tm.log` lines 5-37 and 58-82 show repeated reads of `walk.rs`/`dir.rs` followed by the identical no-submit failure; `tm events tail --from 0` shows two sessions and retry scheduling (events 164-176, 287-294). Also: pass-2/3 trials show the same pattern repeatedly: pallets-click-3822 (both passes, up to 1.39M input tokens on a retry that repeated the same reads) and psf-requests-7432 (41 tool calls, 3.29M input tokens, repeated reads of models.py/test_requests.py/utils.py) — see docs/trials/20260925-1650 and 20260925-1847. Also: recurred again in pass 4/5 as pallets-click-3822 ending without submission after repeated Path-source reads (docs/trials/20260925-2015: 1,416,202 input tokens; docs/trials/20260925-2206: 916,284 input tokens for a second ticket) — same no-submit-then-restart-investigation shape.
- [x] **t20260925-1314-gohugoio-hugo-15360-actionable-tool-errors** (landed 12c4ec6) — Note: passes 20260925-2357 and 20260926-0129 (run against pre-fix binaries) show the same generic-phrase cluster plus a distinct `unknown variant \`verification\`` schema mismatch on artifact-store payloads; the schema-mismatch angle is tracked separately below as `t20260926-agent-submit-schema-verification-variant` since it's a real payload/schema bug, not just message wording — worth a quick post-landing check that the wording fix actually covers `sindresorhus-ky-878`/`spf13-cobra-2257`'s repeat of "couldn't find that"/"operation state was inconsistent" in a fresh trial pass.
  model: sonnet · severity: low · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/agent.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: Extend `plain_tool_error` and its progress rendering to preserve a concise safe diagnostic and identify an actionable recovery step, especially for invariant and parse failures, instead of reducing them to generic phrases.
  acceptance: When a tool call fails with an invariant or parse error, `tm run` output names the failed operation and suggests a concrete next action, while detailed diagnostics remain available in logs; add focused tests for both cases.
  test: `cargo test -p tm-cli plain_tool_error`
  evidence: `/tmp/tm-trials/20260925-1314/gohugoio-hugo-15360/tm.log` lines 96-98: “Submitted the ticket -> error: hit an unexpected internal problem”, “Saved a result -> error: got a response it couldn't understand”, then “Saved a result” and “Submitted the ticket”. Also: spf13-cobra-2257 (docs/trials/20260925-1650) shows the identical pattern: `agent.rs` lines ~2415-2452's `plain_tool_error` collapses a parse failure and an internal invariant failure to the same generic phrasing, disconnected from the recovery that follows. Also: the same generic-phrase collapse ("couldn't find that" / "hit an unexpected internal problem" / "got a response it couldn't understand") recurred in sindresorhus-ky-878 across passes 3, 4 and 5 (docs/trials/20260925-1847, 20260925-2015, 20260925-2206) and in gohugoio-hugo-15360 pass 5 (20260925-2206) — always resolving on a later retry with no distinction rendered between a recovered transient failure and a terminal one; a separate finding in the same cluster asked that a successful "Ran tests" progress line show the actual command and exit status (20260925-2015/sindresorhus-ky-878) rather than just the generic label.
- [x] **t20260925-1314-pallets-click-3822-summarize-repeated-file-reads** (landed bd384a9)
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/agent.rs`
  change: Update tool progress rendering so consecutive repeated reads of the same path are compacted or explicitly counted, while preserving distinct path/range reads as separate actions; include a final repeat count so users can tell whether the agent is stuck rereading context.
  acceptance: A run transcript with multiple identical `fs.read` calls for one path renders a clear repeat indicator instead of an indistinguishable list, while different paths and `fs.read_range` ranges remain individually visible.
  test: `cargo test -p tm-cli format_tool_call`
  evidence: `/tmp/tm-trials/20260925-1314/pallets-click-3822/tm.log` lines 27–48 show repeated `Read src/click/types.py` progress entries and overlapping full-file/range reads; implementation and adjacent tests are in `crates/tm-cli/src/agent.rs` (`plain_tool_arg`, `plain_tool_action`, `format_tool_call_shows_the_call_s_salient_argument`). Also: confirmed again in pass 2 (docs/trials/20260925-1650/pallets-click-3822.json) and pass 3 (docs/trials/20260925-1847), including a retry that repeats the same file reads a second time. Also: psf-requests-7432 pass 4 (docs/trials/20260925-2015) repeated reads of `tests/test_requests.py` and `prepare_body`/`_body_position` searches across 66 tool calls and 6,150,789 input tokens before hitting the step limit.
- [x] **t20260925-1314-psf-requests-7432-terminal-failure-summary** (landed fa0ccf2)
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: When `tm run` completes with a failed attempt such as “model ended turn without submitting,” render a concise terminal summary that says no patch/evidence was submitted, includes the failure reason, and points to the exact resume/retry command; preserve the current error exit status.
  acceptance: A ticket run that ends without submission prints a final summary distinguishing this from a test failure and gives an actionable recovery command; a successful run's output remains unchanged.
  test: `cargo test -p tm-cli run_outcome_ready_state_plain_message`
  evidence: `/private/tmp/tm-trials/20260925-1314/psf-requests-7432/tm.log:170` — “model ended turn without submitting. Run `tm run T-1` again to retry.” after 1,063.43 seconds, followed by `ticket.retry_scheduled` at line 172. Also: confirmed again in psf-requests-7432 pass 3 (docs/trials/20260925-1847: 41 tool calls, 3,291,872 input tokens, ended without submission) and pallets-click-3822 pass 2 (docs/trials/20260925-1650: 177s, 1,139,896 input tokens). Also: BurntSushi-ripgrep-3376 pass 3 (docs/trials/20260925-1847: 291.91s, `ticket.retry_scheduled` at event 124) and pallets-click-3822 pass 4/5 (docs/trials/20260925-2015 line 47/101, 20260925-2357 line 24) all hit the identical unqualified "model ended turn without submitting" summary with no indication of whether any file changed or whether the scheduled retry is expected to help. Also: recurred again in pass 6/7 — BurntSushi-ripgrep-3376 (docs/trials/20260925-2357: 25 tool calls/344s), psf-requests-7432 (20260925-2357: 35 tool calls/493s and again 20260926-0129: 515.9s/2,529,469 input tokens), and pallets-click-3822 (20260926-0129) — all still hit the same unqualified "something went wrong: model ended turn without submitting" with a blind retry suggestion; note the separately-landed no-submit-memory fix (D-036, commit 76ca45d) now carries steering context across retries but deliberately does not change this CLI-facing summary wording, so this task is still fully open and not superseded by that landing. Also: recurred in passes 8/9 (docs/trials/20260926-0240, 20260926-0459) across BurntSushi-ripgrep-3376, pallets-click-3822 (twice) and psf-requests-7432, one reaching 10,865,871 input tokens on a single ticket before the run hit an external 600s timeout mid-retry with no final response at all — the retry-loop-without-bound angle is the same shape tracked in the token-budget/exploration-cycle tasks below, but the CLI summary wording itself (this task's scope) is confirmed still generic across all of them.
- [x] **t20260925-1314-sindresorhus-ky-878-provider-retry-guidance** (landed 5a946a9) — upgraded to a preflight check, not just better retry text (2026-09-25, recurred a 2nd time in pass 2)
  model: sonnet · severity: high · builds Rust: yes · area: provider-routing · deps: none
  files: `crates/tm-cli/src/sched.rs`, `crates/tm-provider/src/fabric.rs`
  change: Better than just improving the retry message (the original framing): validate the effective provider candidates for a ticket's required role against registered providers/credentials *before* dispatching an agent turn at all, so a misconfigured provider fails fast with a direct, actionable diagnosis instead of burning a lease cycle first (`no worker attached; lease lapsed`, seen in both trial passes) and only then explaining itself. When no candidate can serve, name the missing provider and the real repair command; do not recommend rerunning `tm run <ticket>` unchanged. Keep `run_outcome`'s existing transient-vs-configuration distinction for failures that still happen mid-run despite the preflight check.
  acceptance: With a project whose `coder.fast` candidate is `devpass` but no provider can be built, `tm run T-1` exits before dispatching an agent and names the missing provider and a viable repair path; with a valid candidate, the run proceeds normally. A test for the `provider not registered: <name>` failure (mid-run case) still verifies a concrete recovery step, not a blind retry.
  test: `cargo test -p tm-cli run_outcome`
  evidence: `/tmp/tm-trials/20260925-1314/sindresorhus-ky-878/tm.log:7` — `provider was unavailable: provider: no candidate can serve role coder.fast: provider not registered: devpass. Run tm run T-1 again to retry.`; source: `crates/tm-cli/src/sched.rs:818-830` currently recommends the same retry for every `Ready`/`Blocked` failure. Also: confirmed again in pass 2 (docs/trials/20260925-1650/sindresorhus-ky-878.json): `no worker attached; lease lapsed` then the same unregistered-devpass error with the same unhelpful retry suggestion. Also: recurred a 3rd time in spf13-cobra-2257 pass 6 (docs/trials/20260925-2357: `provider was unavailable: ... provider not registered: devpass. Run tm run T-1 again to retry.`), confirming this is a persistent, not one-off, gap in `run_outcome`'s Ready/Blocked message construction (`crates/tm-cli/src/sched.rs:818-830`).
- [x] **t20260925-1314-spf13-cobra-2257-step-limit-submit-recovery** (landed e84b8fb)
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When the worker approaches `max_steps`, surface an explicit finalization/recovery path and preserve completion evidence if `ticket.submit` fails at the limit; replace generic retry guidance with the actual failure reason and safe next action.
  acceptance: A test simulates a worker that completes and verifies its task but reaches the final allowed step with a failed submit; the run either submits successfully through the recovery path or returns a user-facing failure that identifies the step-limit/submit cause, preserves the diff and test evidence, and does not claim the ticket was submitted.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260925-1314/spf13-cobra-2257/tm.log` line 19 (`something went wrong: step limit (64) reached without submitting. Run tm run T-2 again to retry`); events seq 355-358 show `goal_claimed_complete`, `ticket.submit` error, then `ticket.failed` for the step limit. Also: the same shape recurs as a plain step-limit failure (no submit attempt at all) in psf-requests-7432 pass 1 (docs/trials/20260925-1314): 64 provider usage events, then `step limit (64) reached without submitting`. Also: recurred in spf13-cobra-2257 pass 4/5 (docs/trials/20260925-2015: 1,446 wall seconds, 6,759,875 input tokens, `ticket.failed` at seq 360/546 followed by automatic `ticket.retry_scheduled`) and in psf-requests-7432 pass 4 (docs/trials/20260925-2015: step limit reached at 66 tool calls after repeated reads, 6,150,789 input tokens) — a separate finding in the same cluster asked that the loop track its own proximity to the limit and steer the model toward verification/submission before the hard cutoff rather than after it. Also: recurred in spf13-cobra-2257 passes 8/9 (docs/trials/20260926-0240: 66 tool calls/4,064,274 input tokens; 20260926-0459: same generic wording, `ticket.retry_scheduled` immediately after).

## W — Real-task trial finding: the dominant token-cost bug (2026-09-25)

- [~] (claimed: subagent) **u1-shell-run-command-field-bypasses-pruning** — Pruning's shell.run dedup key ignores the `command` field, so build/test loops resend ~24KB blobs forever instead of collapsing repeats
  model: sonnet · severity: critical · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/pruning.rs`, `crates/tm-agent/src/tools.rs`
  change: `pruning::addressable_for` (pruning.rs, the `shell.run`/`build.run`/`test.run` arm) derives its dedup key only from `input.get("argv")`. But `tools.rs::command_argv` (~line 385) accepts either `argv` or a `command` shell-line string, and the tool's own description tells the model it can pass either. A call issued as `{"command": "go test ./..."}` — the natural form for build/test — makes `addressable_for` return `None` (falls through the `_ => None` arm), so `working_set()` never supersedes it: the same ~24KB-capped shell.run result (tools.rs's `inline_output` caps: 4KB+12KB stdout, 2KB+6KB stderr) gets replayed in full at every later step for the rest of the run, no matter how many times the identical command reruns. Fix `addressable_for`'s shell.run arm to derive the same canonical key from either `command` or `argv` — reuse or mirror `tools.rs::command_argv`'s own resolution logic so the two never drift apart, rather than reimplementing a second parse of the same two fields.
  acceptance: A `pruning.rs` unit test: two `shell.run` steps with identical `{"command": "..."}` input (no `argv` field) — the first is `Superseded` after the second, matching existing argv-form coverage. A second test with `argv` present alongside a different `command` confirms `argv` still wins (no behavior change for tool calls already using the argv form).
  test: `mise run test:crate -- tm-agent`
  evidence: Trial `t20260925-1314-spf13-cobra-2257` (`docs/trials/20260925-1314/spf13-cobra-2257.json`) burned 4,625,656 tokens over 64 steps (avg ~72K/step) against OpenCode's 26,285 tokens for the whole run on the same issue — `tm stats --by tool` on that trial's preserved `.tm/project.db` shows 36 `shell.run` calls. Confirmed via direct code read (`crates/tm-agent/src/pruning.rs` lines ~114-188, `crates/tm-agent/src/tools.rs` lines ~385-395, ~1893) that the shell.run dedup key path never inspects `command`, only `argv`.

## X — Groomed from trial-inbox passes 2 and 3 (2026-09-25); 7 of 9 raw findings were duplicates of existing V-section tasks and were merged in as extra evidence instead of new entries

- [x] **t20260925-1650-ticket-title-from-long-body** — Keep long, pasted issue bodies out of ticket display titles (landed 994c684)
  model: sonnet · severity: medium · builds Rust: yes · area: tickets · deps: none
  files: `crates/tm-cli/src/tickets.rs`
  change: `tm ticket new` passes the whole objective straight through as the display title (`crates/tm-cli/src/tickets.rs` ~line 605-610's `args.objective.clone()`), so pasting a multi-paragraph GitHub issue body makes markdown, reproduction code, and environment details the ticket's title everywhere it's listed. Keep the complete objective in storage (the worker still needs the full text), but derive a concise display title from its first meaningful line for `ticket.created` events, `tm ticket list`, and creation feedback; show the full text only in `tm ticket show`/detail views.
  acceptance: Creating a ticket from a multi-paragraph issue body keeps the full text in storage and ticket detail, while list output and creation/event summaries show a short, readable title rather than the full body.
  test: `cargo test -p tm-cli ticket_new`
  evidence: `/tmp/tm-trials/20260925-1650/gohugoio-hugo-15360/tm.log` lines 3, 16 and `/tmp/tm-trials/20260925-1650/psf-requests-7432/tm.log` line 89/98 — both trials' `ticket.created` events store the entire multi-paragraph issue body as `title`.

## Y — Found running Track A myself: bench-cross has never actually executed (2026-09-25)

- [x] **v2026-bench-cross-tm-binary-not-on-path** (landed 49d0e09) — `mise run bench:cross`/`cargo xtask bench-cross` fails every single task immediately: `TmAdapter` assumes `tm` is on `$PATH`
  model: sonnet · severity: critical · builds Rust: yes · area: bench · deps: none
  files: `crates/xtask/src/bench_cross.rs`
  change: `bench_cross.rs` ~line 1145-1148 constructs `TmAdapter { binary: "tm".to_string(), ... }` — a bare command name resolved via `$PATH`. But this workspace's own compiled `tm` only ever exists at `target/<profile>/tm` (or wherever `cargo build -p tm-cli` puts it); it is never installed onto `$PATH` as part of any dev workflow, `mise run build`, or this bench-cross task itself. Every real run — including this session's attempt with fresh binaries built minutes earlier — fails all 6 tasks instantly with `failed to spawn command: No such file or directory`, and this apparently was never caught because the 3 `u1-bench-cross-*` fix tasks landed this session were verified only by unit tests against fake adapters (`mise run verify` never spawns real coding tools, per their own test suite comments), never by an actual `mise run bench:cross` invocation. Resolve the `tm` binary's real path instead: prefer `env!("CARGO_BIN_EXE_tm")`-style resolution when built via `cargo run -p xtask`, or compute it relative to the current executable's own path (`std::env::current_exe()`'s sibling `tm`/`tm.exe`), with a `--tm-binary <path>` override flag for a caller that built it elsewhere (e.g. a fresh clone in a benchmark script).
  acceptance: `mise run bench:cross -- --task hello-world` succeeds in spawning the real `tm` binary (no "No such file or directory") in a completely fresh shell with no `tm` on `$PATH`, from this exact worktree.
  test: `mise run test:crate -- xtask` plus a real manual run: `mise run bench:cross -- --task hello-world`
  evidence: This session, `mise run bench:cross -- --help` (help wasn't recognized either — a possible second, minor issue) ran the real benchmark against all 6 discovered tasks and printed `bench-cross: tm/<task> adapter failed: failed to spawn command / Caused by: No such file or directory (os error 2)` for every one, with `which tm` confirming no `tm` on `$PATH` in this environment.

- [x] **v2026-bench-cross-followups** — Three small bench-cross gaps found while fixing the tm-binary-not-on-path bug (landed 131553f)
  model: haiku · severity: low · builds Rust: yes · area: bench · deps: v2026-bench-cross-tm-binary-not-on-path
  files: `mise.toml`, `crates/xtask/src/bench_cross.rs`
  change: (1) `bench:cross`'s zero-config path only works if `tm` was already built separately — the mise task itself only builds `xtask`. Add `depends = ["build"]` to the `bench:cross` mise task so a fresh checkout's first `mise run bench:cross` just works. (2) The cross-tool report scores a `(tool, task)` pair as pass/fail purely from the task's own predicate, even when the adapter itself reported the tool "did not finish cleanly" (crashed, errored, gave up) — so a task whose predicate happens to already be satisfied (a pre-existing fixture file, an already-passing test) can score a false "pass" unrelated to anything the tool did. Cross-check the adapter's own outcome before crediting a pass. (3) `bench_cross::tests::claude_adapter_completes_and_captures_output_when_the_tool_blocks_on_stdin` has a 5s timeout that can flake on this machine's known first-exec amfid-wedge (a freshly-written script's first exec can hang briefly before running instantly on retry) — give it more headroom or a warm-up exec.
  acceptance: `mise run bench:cross` from a completely fresh clone (no prior `mise run build`) succeeds in spawning `tm`. A task whose predicate is trivially pre-satisfied no longer scores a pass if the adapter reported a real failure. The flaky test passes reliably across repeated runs.
  test: `mise run test:crate -- xtask`
  evidence: Found while fixing `v2026-bench-cross-tm-binary-not-on-path` (see its subagent's final report) — the `hello-world` bench task's predicate (`file_exists` on a file already present in the fixture) scored `pass 0.90` for tm despite the run only having been checked for "did it spawn", not "did it solve anything".

## Z — Groomed from trial-inbox passes 4, 5 and 6 (2026-09-26); 19 of 22 raw findings were duplicates of existing V/X-section tasks and were merged in as extra evidence instead of new entries

- [x] **t20260926-agent-loop-detect-unproductive-exploration-cycles** — Detect repeated read/search/command cycles with no forward progress and nudge instead of burning the step budget (landed dd4e266)
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Distinct from the existing no-submit-retry cluster above (which fires only after a turn ends without submitting): during a single attempt, detect near-identical repeated file reads/searches/toolchain-discovery commands with no intervening edit or verification progress, and inject one concise goal-focused nudge (listing already-inspected paths and findings) before the step budget is exhausted. Also cover the specific case where the repeated discovery is chasing a missing toolchain/environment dependency rather than exploring source — that should hand off with the concrete missing-dependency diagnosis, not keep retrying the same search.
  acceptance: A scripted agent loop that repeatedly inspects the same files/searches without editing receives a progress-aware nudge before its configured step limit; a scripted loop searching for an unavailable toolchain reports the specific missing dependency instead of exhausting the step limit; legitimate multi-step investigation that keeps making distinct forward progress is unaffected.
  test: `cargo test -p tm-agent drive_fails_after_two_consecutive_text_only_turns`
  evidence: Recurred across at least 4 separate trial runs: spf13-cobra-2257 pass 3 (docs/trials/20260925-1847, tm.log:31-60,76-81 — repeated `ParseFlags`/`append` searches before the 64-step limit), spf13-cobra-2257 pass 4 (docs/trials/20260925-2015, same pattern, retry repeats identical searches), gohugoio-hugo-15360 pass 5 (docs/trials/20260925-2206, tm.log lines 4-32/35-40/48-50 — 54 tool calls and 5,841,099 input tokens re-searching BOM/TrimPrefix/Unmarshal variants after already locating the target decoder), and spf13-cobra-2257 pass 5 (docs/trials/20260925-2206, tm.events.jsonl — repeated `fs.read` calls with `result_bytes:65875, truncated:true` while probing for an unavailable Go toolchain). Also: BurntSushi-ripgrep-3376 pass 8 (docs/trials/20260926-0240: repeated rereads of `crates/ignore/src/dir.rs`/`walk.rs`, 10,865,871 input tokens across a step-limit failure plus a retry that hit an external 600s timeout) and pass 9 (20260926-0459: repeated reads of the same two files again, 1,708,426 input tokens over just 23 tool calls) and psf-requests-7432 pass 9 (20260926-0459: repeated reads/searches across `models.py`/`utils.py`, 4,121,406 input tokens/606s) and spf13-cobra-2257 pass 8 (20260926-0240: 66 tool calls/4,064,274 input tokens rereading `completions.go`/`command.go`) — this cluster is now confirmed across 4+ distinct trial passes as the largest recurring token-cost driver in the whole benchmark.

- [x] **t20260926-agent-loop-cumulative-token-budget-and-compaction** — Track cumulative per-attempt token usage, compact repeated context, and warn/stop before a hard budget (landed f11a794)
  model: sonnet · severity: high · builds Rust: yes · area: agent-runtime · deps: u1-shell-run-command-field-bypasses-pruning
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Broader than the shell.run-specific pruning-key bug fixed separately (see the "## W" section above, `u1-shell-run-command-field-bypasses-pruning`): track cumulative input-token usage across an attempt, compact/summarize prior tool results before resending them on later model steps (preserving raw output for audit), and surface a configurable warning before crossing a token/cost threshold, stopping outright before a hard limit rather than only discovering the blowup after the run completes. This is a real, still-open gap even once the shell.run dedup bug lands, since it also covers `fs.read`/`fs.search` repetition and non-shell context growth.
  acceptance: A representative focused code-fix run retains task-relevant evidence and completion behavior while emitting a visible context-compaction event and materially fewer aggregate input tokens than an uncompacted run; a scripted multi-turn test that crosses a configured warning threshold emits a user-visible warning before the next completion call, and a run crossing the hard limit stops without an additional provider call.
  test: `cargo test -p tm-agent --lib`
  evidence: psf-requests-7432 pass 5 (docs/trials/20260925-2206): `tm stats --json` for T-1 records `tokens_in: 2786544`, 43 tool calls, 241 wall seconds. pallets-click-3822 pass 6 (docs/trials/20260925-2357): `wall_seconds: 278`, `tokens_in: 744357`, `tokens_out: 0` after only 18 tool calls with no visible warning at any point during the run.

- [x] **t20260926-events-bare-command-quick-view** — Give the bare `tm events` command a useful non-following recent-event view (landed e8760cc)
  model: haiku · severity: low · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs`
  change: Running bare `tm events` in a project with real events currently prints usage/subcommand help instead of anything from the durable event log — a user has to already know to run `tm events tail`. Add a non-following recent-event view (or a dedicated quick-view subcommand) that prints a bounded list of the most recent events and exits; print an actionable hint (not just an empty list) when the log has no events yet. Keep `tm events tail` for live streaming, unchanged.
  acceptance: Running `tm events` in a project with events prints a bounded recent event list and exits with a normal status; an empty project gets an actionable hint pointing at ticket/run commands that would populate the log; `tm events tail` retains its current follow behavior.
  test: `cargo test -p tm-cli events_tail`
  evidence: pallets-click-3822 pass 6 (docs/trials/20260925-2357/pallets-click-3822/tm.log lines 26-33) — bare `tm events` printed usage/subcommands instead of the durable event log during the trial's own investigation. Also: recurred in gohugoio-hugo-15360 (20260925-2357), pallets-click-3822 and sindresorhus-ky-878 (20260926-0129) — a persistent, cross-pass gap, not a one-off. Also: recurred yet again in pallets-click-3822, sindresorhus-ky-878 and spf13-cobra-2257 (20260926-0240) and pallets-click-3822 (20260926-0459) — now confirmed in every single trial pass that has touched `tm events` bare, across 6 separate occurrences.

## AA — Groomed from trial-inbox passes 6 (2357) and 7 (0129) (2026-09-26); 12 of 14 raw findings duplicated open V/X/Z-section tasks, folded in as evidence

- [x] **t20260926-agent-submit-schema-verification-variant** — Fix the `ticket.submit`/artifact-store schema mismatch producing `unknown variant \`verification\`` (landed a90cca1)
  model: sonnet · severity: high · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/tools.rs`, `crates/tm-cli/src/agent.rs`, `crates/tm-core/src/store.rs`
  change: Distinct from the generic-phrasing/wording cluster tracked elsewhere (`t20260925-1314-gohugoio-hugo-15360-actionable-tool-errors`, landed 12c4ec6): the model is naturally producing a `verification` result-kind variant when storing a test/verification artifact, and the store's schema rejects it outright with `unknown variant \`verification\``, forcing a retry. Either accept the variant the model naturally produces (add it to the accepted enum) or, if it's genuinely invalid, have the artifact-store tool's own schema/description list the exact accepted variants so the model doesn't have to discover the mismatch by trial and error. Also align the `ticket.submit` evidence-empty invariant message ("Submitting a ticket needs at least one piece of evidence" / "operation state was inconsistent") to name a concrete artifact id it should attach, per the same evidence.
  acceptance: A worker submitting a `verification`-kind result stores it successfully on the first attempt (no `unknown variant` error), or the tool schema visibly lists the accepted kinds so the model self-corrects without a failed round-trip; a `ticket.submit` call with no evidence gets a message naming a specific artifact id to attach rather than generic "inconsistent state" text.
  test: `cargo test -p tm-agent && cargo test -p tm-cli plain_tool_error`
  evidence: sindresorhus-ky-878 pass 7 (docs/trials/20260926-0129, tm.log lines 34-38, 110-137): submit attempts returned "couldn't find that", inconsistent-state boilerplate, and `unknown variant \`verification\`` before eventually succeeding on retry. spf13-cobra-2257 pass 7 (docs/trials/20260926-0129, tm.log lines 114-116): "Submitting a ticket needs at least one piece of evidence", "operation state was inconsistent", then `unknown variant \`verification\``, ending in a step-limit failure without ever submitting. gohugoio-hugo-15360 pass 7 (docs/trials/20260926-0129, tm.log lines 125-127): submission failed for missing evidence and storing a `test-output` result failed with "unknown variant" too. Also: recurred again in sindresorhus-ky-878 passes 8/9 (docs/trials/20260926-0240 line 24, 20260926-0459 line 47) with the full accepted-kinds list visible in the error text (`command_output`, `patch`, `file`, `report`, `index`, `benchmark`, ...) — confirming `verification` specifically is the one variant the model naturally reaches for that isn't accepted; both times the ticket eventually submitted only after a wasted retry once the model happened to pick a different kind. Also relates to the empty-evidence invariant (`Store::submit` at `crates/tm-core/src/store.rs:1010-1013`, seen again in sindresorhus-ky-878 pass 8): the fix for this task should also make that invariant's message name a concrete artifact id to attach, per the already-open evidence-guidance angle in this same cluster.

- [x] **t20260926-agent-tool-paths-not-rooted-to-project** (landed a57e058) — Root agent tool file operations to the active project, not a caller's parent directory
  model: sonnet · severity: high · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-cli/src/agent.rs`, `crates/tm-cli/src/project.rs`
  change: A ticketed agent's relative file reads/edits/listings are not reliably rooted to the resolved project directory: starting `tm run` from a directory containing sibling repositories let the agent's initial relative-path read miss ("couldn't read or write a file"), fall back to listing the parent directory (surfacing an unrelated sibling clone by name), and only then correct itself. This is a real correctness/safety gap, not just a UX one — a differently-shaped retry could plausibly read or edit a sibling repo instead of the intended project. Resolve and pin the project root before dispatching any agent tool call, and make relative paths resolve against that root unconditionally rather than the process's actual cwd/parent; on a failed relative path, report the resolved project root and a corrected-path hint instead of only "couldn't read or write a file."
  acceptance: Starting `tm init`/`tm run` from a directory containing sibling repositories cannot cause the agent to inspect, list, or edit a sibling clone's files; a failed relative-path tool call names the resolved project root and a likely corrected path.
  test: `cargo test -p tm-cli`
  evidence: sindresorhus-ky-878 pass 7 (docs/trials/20260926-0129, tm.log lines 8-18): the agent's first `source/utils/body.ts` read failed with "couldn't read or write a file", it then listed the parent directory (surfacing both the `tm` and `opencode` sibling clones from the trial harness by name), and only then read/edited `tm/source/utils/body.ts` correctly.

## BB — Groomed from trial-inbox passes 8 (0240) and 9 (0459) (2026-09-26); 9 of 11 raw findings duplicated open V/X/Z/AA-section tasks, folded in as evidence

- [x] **t20260926-ticket-new-shows-active-lease-state** — Show lease/session ownership when a ticket is already being worked before `tm run` (landed 2c0ddb6)
  model: sonnet · severity: high · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/tickets.rs`
  change: When a background scheduler activates and leases a ticket before the user invokes `tm run` on it (a real race in any project running `tm serve`/the TUI/`sched run` in-process), `tm ticket new`'s own output only ever prints "Created ticket T-2" — the user has no way to know a worker already grabbed it until `tm run` fails with `error: conflict: T-2 is already being worked by agent:builtin/T-2 until <timestamp>`. Show the ticket's current state, lease holder, and session id as part of `tm ticket new`'s creation output when it's non-draft; when `tm run <ticket>` hits an active-lease conflict, either follow/attach to the existing session automatically or print the precise command to inspect/resume it, rather than ending on an unexplained conflict.
  acceptance: In a project with an active in-process scheduler, creating a ticket that gets immediately claimed shows its lease holder and session id in the creation output; `tm run` against an already-leased ticket gives a clear attach/resume path instead of a bare conflict error.
  test: `cargo test -p tm-cli`
  evidence: gohugoio-hugo-15360 pass 8 (docs/trials/20260926-0240/gohugoio-hugo-15360/tm.log): `error: conflict: T-2 is already being worked by agent:builtin/T-2 until 2026-09-26T11:42:28.870292Z`; the event log shows `ticket.leased T-2` and `session.started S-1` had already happened, but `tm ticket new`'s own output only printed `Created ticket T-2` with no indication a worker had already picked it up.

- [x] **t20260926-usage-token-split-not-fabricated** — Don't fabricate an input/output token split when a provider only reports an aggregate (landed b8d0641)
  model: sonnet · severity: medium · builds Rust: yes · area: telemetry · deps: none
  files: `crates/tm-harness/src/metrics.rs`
  change: `usage.recorded` events currently fold a provider's aggregate-only token count entirely into `tokens_in`, reporting `tokens_out: 0` even when real output tokens were produced — `tm stats --json` then presents a fabricated, misleadingly precise split rather than an honest aggregate/unavailable-split. Preserve the provider's actual input/output counts when it reports them separately; when it only reports an aggregate, expose that as an aggregate figure (or an explicit unavailable-split marker) instead of assigning the whole amount to `tokens_in`.
  acceptance: A run whose provider reports separate input/output counts yields those exact counts in `tm stats --json`; a run with only aggregate usage reports an aggregate/unavailable-split rather than a fabricated `tokens_out: 0`.
  test: `cargo test -p tm-harness ticket_metrics_from_events`
  evidence: sindresorhus-ky-878 pass 9 (docs/trials/20260926-0459/sindresorhus-ky-878/tm.log lines 158-175): `tokens_in: 895489`, `tokens_out: 0` for a run that plainly produced real assistant output; `crates/tm-harness/src/metrics.rs` documents that `usage.recorded` carries no split and folds the whole amount into `tokens_in`.

- [x] **t20260926-followups-from-rootpath-fix** — Three small gaps found while landing the project-root path-safety fix (landed ed60bfb)
  model: haiku · severity: low · builds Rust: yes · area: bench/agent · deps: t20260926-agent-tool-paths-not-rooted-to-project
  files: `scripts/luna-trials.sh`, `crates/tm-agent/src/tools.rs`
  change: (1) `scripts/luna-trials.sh` documents running `tm init` inside `$dir/$slug/tm`, but the real trial that exposed the path-safety bug shows it actually ran at `$dir/$slug/` (the parent holding both the `tm` and `opencode` sibling clones) — now fails loudly instead of silently thanks to the landed fix, but the harness script itself should be corrected so trials exercise the intended scenario. (2) `shell.run`/`test.run`/`build.run`'s `resolve_cwd` now rejects an absolute `cwd` outright, even one that legitimately points inside the project root (e.g. a model echoing the root path back verbatim as `cwd`) — accept an absolute `cwd` when `strip_prefix(root)` succeeds instead of rejecting all absolute paths. (3) `tm-cli/tests/acp_serve.rs`'s `tm_acp_answers_initialize_over_stdio` showed a 7-of-15 failure rate (30s timeout, empty stderr) in one session running on this shared 8GB machine under concurrent load, structurally unrelated to any diff in flight at the time; establish a baseline failure rate against a clean `main` with nothing else running concurrently before assuming it's fixed or newly flaky.
  acceptance: `luna-trials.sh`'s tm arm runs `tm init` inside the actual per-tool clone directory, not its parent. A `shell.run` call with an absolute `cwd` equal to (or inside) the resolved project root succeeds instead of being rejected. A documented baseline failure rate exists for `tm_acp_answers_initialize_over_stdio` (e.g. N/20 runs on an idle machine) to compare future runs against.
  test: `mise run test:crate -- tm-agent`; manual: rerun `cargo test -p tm-cli --test acp_serve -- --test-threads=1` 20x on an otherwise idle machine
  evidence: project-root-path-fixer subagent's final report (this session): confirmed via `tm.log` that the trial's `tm init` ran one directory level higher than the harness script's own comment claims; confirmed the absolute-cwd rejection is a real regression risk via code read of the new `resolve_cwd` guard; observed 7/15 `acp_serve` timeouts in-session with no diff able to reach that code path.
- [x] **t20260926-0637-pallets-click-3822-no-submit-recovery-guidance** — Give actionable recovery guidance when a run repeatedly ends without submitting (landed d352cf9)
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: In the no-submit failure branch, identify a model turn that ended without submitting as a distinct recoverable cause and provide an action that inspects the saved attempt or resumes it with focused continuation instead of blindly rerunning the unchanged ticket; avoid the generic “something went wrong” phrasing for this known failure.
  acceptance: A CLI test for repeated `model ended turn without submitting` failures asserts the output names the cause and offers artifact-inspection or focused-resume guidance, while test failures retain their existing retry instructions.
  test: `cargo test -p tm-cli run_outcome_ready_state_plain_message`
  evidence: `/private/tmp/tm-trials/20260926-0637/pallets-click-3822/tm.log:51` and `:167` — both attempts ended “no patch or evidence was submitted … Failure: something went wrong: model ended turn without submitting. Run `tm run T-1` again to retry.”
- [~] **t20260926-0637-psf-requests-7432-no-submit-recovery-guidance** — Give actionable recovery guidance when a run ends without a patch (needs another pass: the worker made no changes)
  model: sonnet · severity: medium · builds Rust: yes · area: cli
  deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: In the no-submit failure branch, distinguish a model turn that ended without submitting from test failures and provide a concrete recovery action that does not blindly rerun the same unchanged ticket; include where to inspect the run's saved artifacts or how to retry with a focused continuation.
  acceptance: A CLI test simulates a `model ended turn without submitting` failure and asserts the rendered message names the no-submit cause and gives a useful artifact-inspection or focused-resume command, while ordinary test failures retain their existing retry guidance.
  test: `cargo test -p tm-cli run_outcome_ready_state_plain_message`
  evidence: `/tmp/tm-trials/20260926-0637/psf-requests-7432/tm.log:45` — “no patch or evidence was submitted … model ended turn without submitting. Run `tm run T-1` again to retry.”
- [x] **t20260926-0637-sindresorhus-ky-878-repo-path-argument-guidance** — Make repository-path errors actionable when tools receive absolute paths (landed d882bec)
  model: sonnet · severity: medium · builds Rust: yes · area: agent-tools · deps: none
  files: `crates/tm-agent/src/tools.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When a tool receives an absolute path inside the configured project root, either safely normalize it to the corresponding repository-relative path or return an error that explains the accepted relative-path format and gives the correct example; keep rejecting paths outside the project root.
  acceptance: A tool request using an absolute path inside the project succeeds or returns a concise correction pointing to its relative form, while an outside-root path remains rejected.
  test: `cargo test -p tm-agent resolve_repo_path`
  evidence: `/tmp/tm-trials/20260926-0637/sindresorhus-ky-878/tm.log:12` — “Built the project -> error: couldn't parse the result (path `/private/tmp/tm-trials/20260926-0637/sindresorhus-ky-878/tm` must be repository-relative)”
- [x] **t20260926-0637-result-schema-recovery** — Explain unsupported result kinds and recover without repeated invalid tool calls (landed e84fdba)
  model: sonnet · severity: medium · builds Rust: yes · area: agent-tools · deps: none
  files: `crates/tm-cli/src/agent.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: Include the supported result-kind names in parse-error feedback and provide an explicit mapping/recovery hint when a provider emits an unsupported result kind such as `verification`; ensure retries switch to a supported kind rather than resubmitting the same invalid shape.
  acceptance: A scripted unsupported `verification` result produces one clear error naming valid result kinds and a succeeding retry uses a supported kind, with no repeated schema failure.
  test: `cargo test -p tm-cli plain_tool_error_keeps_parse_diagnostic_and_recovery_step`
  evidence: `/tmp/tm-trials/20260926-0637/sindresorhus-ky-878/tm.log:18` — “Saved a result -> error: couldn't parse the result (unknown variant `verification`, expected one of `command_output`, `patch`, `file`, `report`, `index`, `benchmark`, `tran…); retry the operation, and check its input if it fails again”
- [x] **t20260926-0637-evidence-submission-guidance** — Surface evidence requirements before ticket submission fails (landed cfa02f3)
  model: sonnet · severity: low · builds Rust: yes · area: ticket-lifecycle · deps: none
  files: `crates/tm-cli/src/agent.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When an agent attempts ticket submission without evidence, explain that it must store a report or other evidence artifact first and then cite that artifact when submitting; direct the next action rather than suggesting a blind retry.
  acceptance: An evidence-free submission produces a user-facing instruction to create and attach evidence, and the agent run completes submission with that evidence without an unhelpful retry loop.
  test: `cargo test -p tm-cli plain_tool_error_keeps_invariant_diagnostic_and_recovery_step`
  evidence: `/tmp/tm-trials/20260926-0637/sindresorhus-ky-878/tm.log:17` — “Submitted the ticket -> error: operation state was inconsistent (Submitting a ticket needs at least one piece of evidence); retry once, then report this failure if it persists”
- [x] **t20260926-0821-BurntSushi-ripgrep-3376-recover-empty-agent-turn** — Recover automatically when a model ends a turn without submitting work (landed eb1b126)
  model: sonnet · severity: high · builds Rust: yes · area: run/recovery · deps: none
  files: `crates/tm-cli/src/sched.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When a transient agent-turn failure leaves a ready ticket with no patch or evidence, continue using the ticket's retry policy within the current `tm run` invocation when budget permits; otherwise report the scheduled retry time, current attempt count, and exact next command instead of ending after a generic model-turn failure.
  acceptance: A simulated model-ended-turn-without-submission failure either recovers and completes within the configured retry budget or prints the retry schedule and an unambiguous next step; it never implies work was completed or tests passed.
  test: `cargo test -p tm-cli run_outcome_ready_state_plain_message`
  evidence: `/tmp/tm-trials/20260926-0821/BurntSushi-ripgrep-3376/tm.log:54` — `agent turn failed: Ticket T-1: no patch or evidence was submitted ... model ended turn without submitting. Run tm run T-1 again to retry.`; `/tmp/tm-trials/20260926-0821/BurntSushi-ripgrep-3376/tm.log:193` — retry was only scheduled after this run failed.
- [x] **t20260926-0821-pallets-click-3822-events-snapshot-default** — Make `tm events` useful for a one-shot event inspection (landed 5f3e2fb)
  model: sonnet · severity: low · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: When `tm events` is invoked without a subcommand, render the current event log snapshot and exit instead of printing command usage; retain `tm events tail` for live-follow behavior and expose an explicit follow option only when requested.
  acceptance: A CLI test runs bare `tm events` with recorded events and asserts it prints the snapshot and exits successfully without hanging; existing `tm events tail` follow and `--no-follow` behavior remains covered.
  test: `cargo test -p tm-cli events_tail`
  evidence: `/tmp/tm-trials/20260926-0821/pallets-click-3822/tm.log:12-34` — the protocol's `tm events` invocation printed only “The durable event log: tail, inspect, replay, and verify the hash chain” and usage, so I had to discover and invoke `events tail --from 1 --no-follow` to capture the actual events.
- [x] **t20260926-0821-psf-requests-7432-resume-no-submit-investigation** — Recover a no-submit run using the investigation already performed (landed d6be9df)
  model: sonnet · severity: high · builds Rust: yes · area: agent
  deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When a run has substantial relevant tool activity but ends without `ticket.submit`, carry a concise actionable investigation summary into the recovery attempt and tell the user what was retained and what remains unverified instead of only returning “model ended turn without submitting.”
  acceptance: A regression test simulates a relevant investigation followed by a no-submit ending and proves the retry receives the findings without repeating the same discovery calls; the final CLI error names the missing deliverable and the next recovery action.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-0821/psf-requests-7432/tm.log:4-51` — T-3 used 35 tool calls across repeated source/history inspection, then failed “no patch or evidence was submitted … model ended turn without submitting”; `tm stats --json` in the same log records 392 wall seconds and 2,751,047 input tokens.
- [x] **t20260926-0821-psf-requests-7432-bound-repeated-source-exploration** — Bound repeated source and history exploration during ticket runs (landed e087aa7)
  model: sonnet · severity: medium · builds Rust: yes · area: agent
  deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: Track repeated file reads and historical diff probes in an investigation, then have the agent summarize existing evidence and move to a concrete edit or an explicit blocker instead of reopening the same files and comparisons.
  acceptance: An agent-loop test fixture with duplicate read/history calls demonstrates that the run reuses its first findings, bounds redundant exploration, and still permits a targeted reread when new evidence requires it.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-0821/psf-requests-7432/tm.log:6-49` — T-3 repeatedly read `src/requests/models.py`, `src/requests/utils.py`, and `tests/test_requests.py` and compared several historical revisions before failing without a patch; `tm stats --json` records 35 tool calls, 392 seconds, and 2,751,047 input tokens.
- [x] **t20260926-0821-sindresorhus-ky-878-result-persistence-recovery** — Recover cleanly from agent result/evidence persistence errors (landed efebfa3)
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-cli/src/agent.rs`
  change: When ticket submission reports missing evidence or a result save fails to parse, give the agent a concrete recovery action and ensure a later success message only appears after durable evidence/result persistence succeeds; preserve a concise indication of whether the ticket was actually submitted.
  acceptance: A run that first encounters the evidence invariant and an invalid result shape retries with valid evidence/result data, and its transcript contains neither a misleading success claim nor an unqualified final submitted message while persistence is incomplete.
  test: `cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-0821/sindresorhus-ky-878/tm.log` lines 44–47 — “Submitting a ticket needs at least one piece of evidence”; “couldn't parse the result”; then “Saved a result” and “Ticket T-2 submitted its work.”
- [x] **t20260926-0821-spf13-cobra-2257-events-snapshot-default** — Make `tm events` useful for one-shot event inspection (landed 00b8a7c)
  model: sonnet · severity: low · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/args.rs`, `crates/tm-cli/src/ops.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: When `tm events` is invoked without a subcommand, render the current event log snapshot and exit instead of printing command usage; retain `tm events tail` for live-follow behavior and `--no-follow` for snapshots from a chosen sequence.
  acceptance: A CLI test runs bare `tm events` with recorded events and asserts it prints the snapshot and exits successfully without hanging; existing `tm events tail` follow and `--no-follow` behavior remains covered.
  test: `cargo test -p tm-cli events_tail`
  evidence: `/tmp/tm-trials/20260926-0821/spf13-cobra-2257/tm.log:71-86` — protocol's bare `tm events` invocation printed “The durable event log: tail, inspect, replay, and verify the hash chain” followed by usage, so I had to discover `events tail --from 1 --no-follow` to capture the event log.
- [x] **t20260926-0821-spf13-cobra-2257-no-submit-recovery** — Make no-submit retry guidance explain what is retained (landed 604db60)
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/sched.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: For a failed run with no patch or evidence, supplement the generic `tm run <ticket> again` recommendation with whether the working-tree edits were retained and what retrying will do, so a user can choose a recovery action without guessing.
  acceptance: CLI tests cover a no-submit failure with retained edits and one without edits; each message states the accurate workspace state and a concrete next action, while preserving the distinction from test failures.
  test: `cargo test -p tm-cli run_outcome`
  evidence: `/tmp/tm-trials/20260926-0821/spf13-cobra-2257/tm.log:69` — after 1160.07 seconds the run ended “no patch or evidence was submitted ... Run `tm run T-1` again to retry,” without indicating whether the workspace contained recoverable edits.
- [x] **t20260926-0821-spf13-cobra-2257-reuse-read-context** — Avoid repeating unchanged file reads during ticket runs (landed 956f48b)
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/prompt.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: Add worker guidance to reuse file contents already present in the conversation and repeat a read only when the file changed, a needed range was omitted, or a tool requires a fresh hash; before rereading, identify the specific missing information.
  acceptance: Prompt tests assert the worker instructions explicitly direct reuse of unchanged file context and give concrete reasons that justify rereading; a replay test with repeated read calls shows the worker proceeds to the next relevant action instead of issuing redundant identical reads.
  test: `cargo test -p tm-agent prompt`
  evidence: `/tmp/tm-trials/20260926-0821/spf13-cobra-2257/tm.log:7-21` — before changing any files, the run repeatedly emitted “Read command.go” and “Read completions.go” across 17 tool actions, after it had already found and inspected the completion append path.
- [x] **t20260926-1044-BurntSushi-ripgrep-3376-recover-empty-agent-turn** — Make empty agent turns recoverable and actionable (landed 56acca3)
  model: sonnet · severity: high · builds Rust: yes · area: run/recovery · deps: none
  files: `crates/tm-cli/src/sched.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When a model ends a turn without submitting a patch or evidence, use the remaining retry budget within the same `tm run` invocation when possible. If the run cannot recover, report the attempt count, current ticket state, why the turn failed, whether/when an automatic retry is scheduled, and a precise next command; distinguish provider-turn failure from test failure.
  acceptance: A simulated model-ended-turn-without-submission either recovers and completes under the configured retry budget or prints the scheduled retry and exact next action; output never leaves a user unsure whether the failed ticket will resume automatically.
  test: `cargo test -p tm-cli run_outcome_ready_state_plain_message`
  evidence: `/tmp/tm-trials/20260926-1044/BurntSushi-ripgrep-3376/tm.log:77` — `agent turn failed: Ticket T-1: no patch or evidence was submitted (this was not a test failure). Failure: something went wrong: model ended turn without submitting. Run tm run T-1 again to retry.`; `/tmp/tm-trials/20260926-1044/BurntSushi-ripgrep-3376/tm.log:245` — `168  ticket.failed T-1`; `/tmp/tm-trials/20260926-1044/BurntSushi-ripgrep-3376/tm.log:251` — `174  ticket.retry_scheduled T-1`
- [x] **t20260926-1044-pallets-click-3822-actionable-empty-turn-recovery** — Give actionable recovery guidance when an agent turn submits no patch or evidence (landed ddf4195)
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: When a model turn ends without a patch or evidence, report the concrete failed-turn state and useful completed work, distinguish transient/provider errors from a model that simply stopped without submitting, and avoid recommending an unconditional rerun when the run made no progress.
  acceptance: A scripted empty-turn run prints an actionable message identifying that no repository change or evidence was submitted and gives a recovery step appropriate to the failure; it does not tell the user only that something went wrong or recommend repeating an unchanged no-progress run.
  test: `cargo test -p tm-cli run_outcome_ready_state_plain_message`
  evidence: `/tmp/tm-trials/20260926-1044/pallets-click-3822/tm.log` — `wall_seconds=324`; `tokens_in`: 732981; then `Failure: something went wrong: model ended turn without submitting. Run \`tm run T-1\` again to retry.`
- [x] **t20260926-1044-psf-requests-7432-stop-no-progress-runs** — Stop repeated no-progress agent runs with actionable recovery context (landed 0b49208)
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Detect repeated turns/tool work that make no repository or submitted-artifact progress, and stop before exhausting the full step budget; include a concise summary of the last useful actions and the missing submission in the failure detail.
  acceptance: A scripted agent run that repeats investigations without changing files or submitting evidence terminates at the no-progress threshold, retains its useful work, and reports what it explored and how to continue; a productive multi-step run is not stopped early.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1044/psf-requests-7432/tm.log` — run took 619 seconds / 83,380 tokens, repeated source/history and Python probes, then `error: agent turn failed: Ticket T-1: no patch or evidence was submitted (this was not a test failure). Failure: something went wrong: model ended turn without submitting.`
- [x] **t20260926-1044-parse-error-corrective-guidance** — Give corrective guidance for invalid artifact inputs instead of recommending a blind retry (landed f25760a)
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-cli/src/agent.rs`
  change: In `plain_tool_error`, distinguish correctable parse/enum errors and inconsistent tool arguments from transient failures; preserve the expected values and tell the model to change the offending input rather than retry the same operation. Keep the current specific evidence-submission guidance.
  acceptance: When `artifact.store` receives an unsupported kind, the rendered error identifies the invalid kind, shows accepted kinds, and directs the agent to correct and retry the input; it does not recommend replaying the unchanged call. Existing empty-submission guidance remains specific.
  test: `cargo test -p tm-cli plain_tool_error`
  evidence: `/tmp/tm-trials/20260926-1044/sindresorhus-ky-878/tm.log` — lines 22–23 and 115: submission retry reported “operation state was inconsistent” and artifact storage reported `unknown variant 'verification'` without actionable correction guidance.

- [x] **t20260926-1044-shell-result-path-recovery** — Make shell tool path parsing recover from absolute result paths (landed a9b9311)
  model: sonnet · severity: low · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/tools.rs`
  change: When parsing command output that contains an absolute path under the current project root, canonicalize it to a repository-relative path before validation; continue rejecting paths outside the root and retain a clear recovery hint.
  acceptance: A shell command producing an in-repository absolute path is captured without a tool error, while a path outside the project is still rejected with the expected boundary explanation.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1044/sindresorhus-ky-878/tm.log` — line 20: `npx ava test/stream.ts test/body-size.ts` result parsing failed with “path ... must be repository-relative”; the agent had to repeat the command.
- [x] **t20260926-1044-spf13-cobra-2257-actionable-turn-failure** — Make no-submission failures actionable and avoid blind retries (landed 6151f00)
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: When an agent turn ends without submitting a patch or evidence, preserve the useful no-submission distinction while including the concrete turn failure and a clear next-step command; avoid scheduling a retry that merely repeats the same failure without new context.
  acceptance: A simulated model-ended-without-submit run prints a human-readable, specific cause and an unambiguous recovery action; retry behavior is bounded and visible, with no duplicate full-context attempt when no recovery condition changed.
  test: `cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-1044/spf13-cobra-2257/tm.log:69` — `no patch or evidence was submitted (this was not a test failure). Failure: something went wrong: model ended turn without submitting. Run tm run T-3 again to retry.` The run metrics in lines 700–716 show 840 seconds and 3,661,274 input tokens for the initial run; a subsequent retry also failed without a patch.
- [x] **t20260926-1223-BurntSushi-ripgrep-3376-cargo-detection-false-positive** — Diagnose and explain unavailable Cargo before aborting a Rust task (landed 6b78f94)
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When `missing_toolchain_dependency` classifies a Cargo failure as unavailable, verify that the executable is actually missing from the agent command environment and include the failed command/output and detected PATH in the task failure; do not label an unrelated build failure as missing Cargo.
  acceptance: With Cargo installed and reachable in the run environment, a failing `cargo build` is reported with its actual cause and does not produce “Required toolchain dependency is unavailable: cargo”; with Cargo absent, the actionable missing-dependency message remains.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1223/BurntSushi-ripgrep-3376/tm.log:16` — “Required toolchain dependency is unavailable: cargo”; `/tmp/tm-trials/20260926-1223/BurntSushi-ripgrep-3376/tm.log:53` shows that the run did invoke `cargo build` before it failed. In the same trial shell, `cargo --version` returned `cargo 1.95.0`.
- [x] **t20260926-1223-pallets-click-3822-context-progress-guard** — Bound repeated source reads and surface useful progress on model turn failure (landed 1ae203c)
  model: sonnet · severity: high · builds Rust: yes · area: agent/context · deps: none
  files: `crates/tm-cli/src/sched.rs`, `crates/tm-cli/src/dispatch.rs`
  change: Detect repeated large-file reads or investigation without new findings during a ticket run, compact/deduplicate previously supplied source context, and expose a small scoped progress/plan plus a no-progress or context-use guard. On a text-only model stop, summarize findings actually retained and state whether a patch or verification evidence exists, along with a concrete resume command.
  acceptance: On a reproduction where the model repeatedly reads `src/click/types.py` then stops without submitting, the run avoids re-sending identical file content, reports actionable Path-focused progress, and ends with a concise saved-session/resume message that clearly states no patch and no tests were produced; ordinary successful runs continue to submit evidence.
  test: `cargo test -p tm-cli run_outcome`
  evidence: `/tmp/tm-trials/20260926-1223/pallets-click-3822/tm.log:19-42` (repeated reads of `src/click/types.py`, followed by “no patch or evidence was submitted” / “something went wrong: model ended turn without submitting”); `/tmp/tm-trials/20260926-1223/pallets-click-3822/tm.log:131-149` (575981 total tokens, 222 recorded seconds, 21 tool calls)
- [x] **t20260926-1223-psf-requests-7432-bound-redundant-context-loops** — Stop repetitive context exploration before a ticket burns excessive time and tokens (landed ecb013f)
  model: sonnet · severity: high · builds Rust: yes · area: agent
  deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Add a run-level guard for repeated unproductive tool/context cycles (including repeated reads of the same source region) so the loop stops or switches to a focused recovery prompt before consuming excessive time/tokens without a submission; include actionable diagnostics in the failure output.
  acceptance: A scripted agent run that repeatedly reads the same file and submits no patch is bounded by the guard, ends with a clear reason and usage/time totals, and offers a directly copyable resume command containing the saved session ID; productive runs remain unaffected.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1223/psf-requests-7432/tm.log` lines 15–66 show repeated reads/exploration and lines 67–70 record no patch after 621.131 seconds; `tm stats --json` recorded 1,445,727 tokens and 41 tool calls.
- [x] **t20260926-1223-spf13-cobra-2257-stop-repeated-investigation** — Detect unproductive repeated source reads before exhausting the run step budget (landed 65de56c)
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Track repeated reads and searches that return substantially the same paths/ranges without an intervening edit or verification, then issue a concise goal-directed nudge that names already-inspected files and asks for the smallest implementation plus focused verification. Do not continue the identical exploration pattern to the hard step limit.
  acceptance: A deterministic agent-loop test scripts repeated overlapping reads with no progress and asserts the loop nudges before the configured step ceiling; a subsequent focused edit and submission succeeds, while useful distinct reads remain allowed.
  test: `cargo test -p tm-agent agent_loop`
  evidence: `/private/tmp/tm-trials/20260926-1223/spf13-cobra-2257/tm.log:55-145` — repeated reads/searches of completions.go and command.go; `tm stats --json` at lines 170-189 reports 3,601,360 total tokens and 64 tool calls before the step-limit failure.
- [x] **t20260926-1223-spf13-cobra-2257-step-limit-summary** — Make step-limit failures actionable when useful work was retained (landed 5766d70)
  model: sonnet · severity: medium · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: When an attempt reaches the step limit without ticket submission, render a concise summary that clearly separates retained work from submitted work, states whether verification ran, includes the underlying stop reason, and gives one safe bounded continuation command rather than an unqualified retry suggestion.
  acceptance: A CLI test simulating a step-limit failure with a retained edit and test result asserts the summary identifies the unsubmitted state, accurately reports verification, preserves the stop cause, and prints the continuation command; a failure with no retained change reports that explicitly.
  test: `cargo test -p tm-cli sched`
  evidence: `/private/tmp/tm-trials/20260926-1223/spf13-cobra-2257/tm.log:145-147` — `step limit (64) reached without submitting` followed by a retry suggestion; preceding lines show an edit and attempted test runs but no submitted ticket.
- [x] **t20260926-1342-BurntSushi-ripgrep-3376-toolchain-availability-diagnostics** — Verify missing toolchains in the agent command environment before failing a task (landed cd5bfb4)
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When `missing_toolchain_dependency` sees “not found” in a shell/build/test result, check executable resolution in the same command environment and preserve the failed command and output in the failure detail. Distinguish a missing executable from a PATH/environment mismatch instead of telling users to install an already-present toolchain.
  acceptance: A run where host Cargo is installed but agent commands cannot resolve it reports the command-environment/PATH discrepancy and recovery steps; an ordinary cargo build failure is not classified as missing Cargo; a genuinely absent Cargo still gets the dependency-specific message.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1342/BurntSushi-ripgrep-3376/tm.log:53` — “Required toolchain dependency is unavailable: cargo. Install or configure it, then retry the task.” The previous ticket failed identically for python3 at line 20; Cargo was available in the trial shell (`cargo test -p ignore` completed successfully after the agent run).
- [x] **t20260926-1342-gohugoio-hugo-15360-surface-attempt-retries** — Surface attempt failures and retries during `tm run` (landed 1530b98)
  model: sonnet · severity: medium · builds Rust: yes · area: cli
  deps: none
  files: `crates/tm-cli/src/sched.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When a foreground `tm run <ticket>` attempt ends without submitting and the scheduler retries or recovers, print a concise user-facing notice with the failure reason, attempt count, ticket state, and next action instead of requiring a later `tm events`/ticket metadata inspection to discover why the run stalled.
  acceptance: A simulated first attempt that ends with `model ended turn without submitting` prints that reason and whether a retry is scheduled; the final run summary reports the resulting state and resume command. The event log remains authoritative and unchanged.
  test: `cargo test -p tm-cli sched::tests`
  evidence: `/tmp/tm-trials/20260926-1342/gohugoio-hugo-15360/tm.log` — `attempt 1 model ended turn without submitting`; user-facing output proceeded through extensive command listings and ended only when bounded execution was interrupted; retry/failure details were recovered from `tm events`.
- [x] **t20260926-1342-pallets-click-3822-repeated-source-read-progress-guard** — Bound repeated source reads and show actionable progress when a run stalls (landed e0e76e1)
  model: sonnet · severity: high · builds Rust: yes · area: agent/context · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-cli/src/sched.rs`
  change: Detect repeated overlapping reads and searches of the same source during a ticket run, avoid re-sending already available content when there is no new finding, and surface a concise retained-findings summary with a scoped next step. When a model ends without submitting, provide a direct saved-session resume command alongside the honest no-patch/no-evidence status.
  acceptance: Replaying the Click #3822 investigation pattern stops the repeated `src/click/types.py` reads/searches from consuming another full context window, tells the user what was established and what remains, and includes a usable resume command after no-submit; normal runs still submit patches and evidence.
  test: `cargo test -p tm-agent repeated_exploration_nudge && cargo test -p tm-cli repeated_no_submit_failure_gets_focused_recovery_guidance`
  evidence: `/tmp/tm-trials/20260926-1342/pallets-click-3822/tm.log:8-18` (repeated reads of `src/click/types.py` and overlapping ranges); `/tmp/tm-trials/20260926-1342/pallets-click-3822/tm.log:97-123` (continued rereads, then “model ended turn without submitting” and the escalation); `/tmp/tm-trials/20260926-1342/pallets-click-3822/tm.log:260-280` (stats report 1,691,000 tokens and 54 tool calls)
- [x] **t20260926-1342-psf-requests-7432-bound-redundant-context-loops** — Stop repetitive context exploration before a ticket burns excessive time and tokens (landed aca6ee9)
  model: sonnet · severity: high · builds Rust: yes · area: agent
  deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Add a run-level guard for repeated unproductive tool/context cycles (including repeated reads of the same source region) so the loop stops or switches to a focused recovery prompt before consuming excessive time/tokens without a submission; include actionable diagnostics in the failure output.
  acceptance: A scripted agent run that repeatedly reads the same file and submits no patch is bounded by the guard, ends with a clear reason and usage/time totals, and offers a directly copyable resume command containing the saved session ID; productive runs remain unaffected.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1342/psf-requests-7432/tm.log` lines 7–29 and 54–106 show repetitive code reads/exploration before ending without a patch at line 107; lines 112–123 record 1107 seconds, 3,114,837 tokens, and 80 tool calls.
- [x] **t20260926-1342-sindresorhus-ky-878-recovered-submit-feedback** — Clarify recovered ticket submission errors in run output (landed 87330bd)
  model: sonnet · severity: low · builds Rust: yes · area: ux · deps: none
  files: `crates/tm-cli/src/dispatch.rs`, `crates/tm-agent/src/tools.rs`
  change: When an agent's first `ticket.submit` fails for missing evidence but the same run later stores evidence and successfully submits, report the first attempt as a recovered submission error in the user-facing run summary rather than leaving a raw tool error next to a later success without connecting them.
  acceptance: A scripted run that first submits with an empty evidence list and then stores/cites an artifact ends with a summary explicitly stating the initial submit was recovered and the final ticket state is submitted; a final unsuccessful submit remains clearly reported as failed.
  test: `cargo test -p tm-cli dispatch`
  evidence: `/tmp/tm-trials/20260926-1342/sindresorhus-ky-878/tm.log:28-31` — “Submitted the ticket -> error: submission needs evidence…” followed by “Saved a result” and “Submitted the ticket”.
- [~] **t20260926-1342-cobra-reuse-read-context** — Reuse unchanged source reads during ticket runs (needs another pass: the worker made no changes)
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/prompt.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: Tell workers to rely on unchanged file contents already in the conversation, and reread only when a specific range was omitted, the file changed, or a tool requires a fresh hash; require identifying that missing detail before repeating a read.
  acceptance: Prompt tests cover the guidance, and a replay of this Cobra task's repeated `completions.go`/`command.go` reads shows the agent reuses prior findings and reaches the edit without redundant reads.
  test: `cargo test -p tm-agent prompt`
  evidence: `/tmp/tm-trials/20260926-1342/spf13-cobra-2257/tm.log:6-27` — the run reread `completions.go` repeatedly, including consecutive reads of the same source, and revisited overlapping `command.go` ranges before editing.
- [x] **t20260926-1342-cobra-stats-token-reconciliation** — Reconcile per-ticket token totals with recorded usage events (landed ed62c14)
  model: sonnet · severity: high · builds Rust: yes · area: telemetry · deps: none
  files: `crates/tm-cli/src/stats.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: Ensure `tm stats --json --ticket <id>` reports the sum of that ticket's `usage.recorded` token counts; add a diagnostic or test-backed correction path if the rollup can diverge from its source events.
  acceptance: An end-to-end ticket run asserts the JSON `tokens_total` equals the independently summed matching `usage.recorded` events, including multiple model turns.
  test: `cargo test -p tm-cli stats_by_ticket`
  evidence: `/tmp/tm-trials/20260926-1342/spf13-cobra-2257/tm.log` — `tm stats --json --ticket T-1` reported `tokens_total: 2378178`, while the captured `tm events --json` usage records for T-1 showed `tokens: 76192` and `tokens: 76853` (153045 total).
- [x] **t20260926-1521-BurntSushi-ripgrep-3376-toolchain-environment-diagnostics** — Make missing-toolchain failures distinguish absent executables from agent command-environment problems (landed 40a3036)
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When `missing_toolchain_dependency` identifies “not found” in shell/build/test output, retain the failed command and its output and check executable resolution in the same command environment. Report an absent executable separately from a PATH/environment mismatch, with a recovery step appropriate to the actual condition.
  acceptance: A run whose host has Cargo but whose agent shell cannot resolve Cargo reports the command/PATH discrepancy and actionable recovery; an ordinary cargo build failure is not classified as missing Cargo; genuinely absent Cargo still receives the dependency-specific message.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1521/BurntSushi-ripgrep-3376/tm.log:79-80` — the agent ran `cargo build --bin rg ...` and then terminated with “Required toolchain dependency is unavailable: cargo. Install or configure it, then retry the task.”
- [x] **t20260926-1521-pallets-click-3822-repeated-exploration-stall-feedback** — Surface a focused progress nudge before repeated source exploration consumes another run (landed 64e0bdc)
  model: sonnet · severity: high · builds Rust: yes · area: agent/context · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/executor.rs`
  change: Strengthen repeated-exploration detection to trigger an actionable, user-visible nudge as soon as a turn repeatedly rereads the same source file or overlapping ranges without a new finding. Include a short retained-findings summary and a concrete next step, and carry that context into a resumed attempt instead of allowing another long reread loop.
  acceptance: Replaying the Click #3822 investigation pattern surfaces a nudge after repeated `src/click/types.py` reads, avoids another full cycle of overlapping reads with no new finding, and preserves a concise investigation summary for recovery; productive read-then-edit runs are not interrupted.
  test: `cargo test -p tm-agent repeated_exploration_nudge && cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1521/pallets-click-3822/tm.log:10-38` (repeated searches and wide/overlapping `types.py` reads); `/tmp/tm-trials/20260926-1521/pallets-click-3822/tm.log:40-120` (the same exploration pattern repeats across dispatches); `/tmp/tm-trials/20260926-1521/pallets-click-3822/tm.log:122-123` (final no-patch/no-evidence failure)
- [x] **t20260926-1521-psf-requests-7432-reduce-repeated-source-context** — Reduce repeated source reads and report context cost during runs (landed be76672)
  model: sonnet · severity: medium · builds Rust: yes · area: agent/context · deps: none
  files: `crates/tm-agent/src/pruning.rs`
  change: Audit the run-time working-set pruning behavior for repeated reads/searches, ensure a same-path reread does not keep redundant full result content in subsequent provider context, and expose cumulative token usage to the run summary so operators can spot disproportionate context consumption.
  acceptance: A scripted run that reads the same source file repeatedly retains all actions in its event log but sends only the newest full result plus superseded stubs to later model turns; its final run summary includes cumulative tokens. Add a regression covering a repeated large file read.
  test: `cargo test -p tm-agent pruning`
  evidence: `/tmp/tm-trials/20260926-1521/psf-requests-7432/tm.log:14-20` and `:33-39` show repeated models.py reads; `tm stats --json` for T-1 reported `tokens_total: 2030788` and 53 tool calls.
- [x] **t20260926-1521-psf-requests-7432-summarize-recovered-submission** — Explain recovered ticket-submission failures in final run output (landed 042a852)
  model: sonnet · severity: low · builds Rust: yes · area: cli/ux · deps: none
  files: `crates/tm-cli/src/agent.rs`
  change: When the agent's first ticket.submit fails for missing evidence and a later artifact.store plus ticket.submit succeeds, render the sequence as a recovered submission and include that recovery in the final run summary, rather than leaving the user to infer success from separate generic progress lines.
  acceptance: A transcript with an evidence-free failed submit followed by a stored report and successful submit says the first attempt was recovered, names the final submitted state once, and does not imply that the run failed.
  test: `cargo test -p tm-cli format_tool_call`
  evidence: `/tmp/tm-trials/20260926-1521/psf-requests-7432/tm.log:71-74` records the evidence-free submit error, `Saved a result`, and success; formatter paths are in `crates/tm-cli/src/agent.rs:2346-2364` and `:2496-2502`.
- [x] **t20260926-1521-sindresorhus-ky-878-avoid-redundant-source-rereads** — Stop repeated test-file exploration after the relevant case is found (landed 41b8136)
  model: sonnet · severity: high · builds Rust: yes · area: agent/context · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`
  change: Track recent successful reads and searches within a ticket run, and when the agent requests overlapping content again without an intervening edit or a clear new hypothesis, reuse the prior result or prompt it to state what new question the reread answers. Preserve deliberate rereads after edits and when verification requires fresh content.
  acceptance: Replaying this investigation finds `source/utils/body.ts` and `test/stream.ts` once, then does not issue repeated identical `onDownloadProgress` searches or five consecutive full reads of `test/stream.ts`; a reread after editing still returns the current file contents.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1521/sindresorhus-ky-878/tm.log:7-18` (duplicate searches and browser reads); `/tmp/tm-trials/20260926-1521/sindresorhus-ky-878/tm.log:27-40` (repeated stream-file reads after the target was located)
- [x] **t20260926-1521-sindresorhus-ky-878-suggest-extension-near-match** — Recover from guessed source extensions without a directory-listing detour (landed 02044f7)
  model: sonnet · severity: medium · builds Rust: yes · area: agent/tools · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/tools.rs`
  change: Extend the not-found path hinting used by `fs_io_error` to consider a unique same-stem candidate with a different source extension (for example `.js` to `.ts`) and include it as a safe correction; retain the existing project-root guidance when candidates are ambiguous.
  acceptance: A read request for `test/helpers/create-http-test-server.js` in a project containing only `test/helpers/create-http-test-server.ts` returns the precise `.ts` suggestion and the agent can continue without listing the project root; ambiguous matches do not auto-select a path.
  test: `cargo test -p tm-agent fs_io_error`
  evidence: `/tmp/tm-trials/20260926-1521/sindresorhus-ky-878/tm.log:16-17` (the guessed `.js` path failed and required a separate directory listing before finding the `.ts` helper)
- [x] **t20260926-1521-spf13-cobra-2257-child-toolchain-preflight** — Diagnose Go availability in the actual agent execution environment before starting edits (landed 2e0f952)
  model: sonnet · severity: medium · builds Rust: yes · area: agent-runtime · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When a shell/build/test call reports a missing toolchain executable, distinguish an actually absent binary from a PATH/environment mismatch and include the failing command plus the child process PATH/toolchain lookup in the surfaced recovery message; do not issue an unqualified “Install or configure it” message when the binary is available to the caller but not the agent child.
  acceptance: A run where Go is on the invoking process PATH but unavailable in the child execution environment reports that distinction and the executable lookup path, while a genuinely absent Go binary still names Go and provides the install/configure guidance.
  test: `cargo test -p tm-agent missing_toolchain`
  evidence: `/tmp/tm-trials/20260926-1521/spf13-cobra-2257/tm.log:15` — “Required toolchain dependency is unavailable: go. Install or configure it, then retry the task.”
- [x] **t20260926-1521-spf13-cobra-2257-bound-retry-context** — Preserve focused investigation context across failed toolchain retries (landed 3ec4e0d)
  model: sonnet · severity: medium · builds Rust: yes · area: agent-runtime · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: On retry after a toolchain-related failure, carry forward the relevant files, identified suspect call site, and attempted verification into the next model turn so the agent resumes instead of repeating broad searches and file reads; keep retries within the configured run budget.
  acceptance: An integration fixture that first emits a missing-toolchain failure and then succeeds resumes at its saved diagnosis, produces fewer repeated read/search calls than a fresh-context retry, and exits within the configured retry/token budget.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1521/spf13-cobra-2257/tm.log:15,23-43` — toolchain failure followed by repeated reads/searches of `completions.go` and `command.go`; `/tmp/tm-trials/20260926-1521/spf13-cobra-2257/tm.log:69` records 347.15 seconds.
- [x] **t20260926-1702-BurntSushi-ripgrep-3376-repeat-exploration-checkpoint** — Stop repeated source reads before exhausting the agent step budget (landed 67874ca)
  model: sonnet · severity: high · builds Rust: yes · area: agent
  deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: Strengthen the repeated-exploration guard so repeated or overlapping reads of the same source path/range trigger a useful reminder to reuse already-returned evidence and move to a reproducer, code change, or focused test; when an attempt reaches its step limit, attach a compact checkpoint naming inspected paths, current hypothesis, and next action to the retry guidance.
  acceptance: In a deterministic scripted run that repeats reads of the same two files until near the configured step limit, the agent is nudged before the final three steps, does not repeat the same ranges, and on a forced limit error the CLI reports the retained checkpoint and one actionable resume step; existing normal investigation flows remain unchanged.
  test: `cargo test -p tm-agent repeated_exploration && cargo test -p tm-cli step_limit`
  evidence: `/tmp/tm-trials/20260926-1702/BurntSushi-ripgrep-3376/tm.log` lines 18-35 show repeated/overlapping reads of `crates/ignore/src/dir.rs` and `walk.rs`; line 150 shows `step limit (64) reached without submitting` and generic retry advice. Source: `crates/tm-agent/src/agent_loop.rs` lines 1444-1454 contains the current repeated-exploration nudge and only adds a step-limit reminder within the last three steps; lines 1822-1826 formats the generic step-limit guidance.
- [x] **t20260926-1702-pallets-click-3822-cumulative-exploration-budget** — Interrupt cumulative source thrashing when exact-call streak detection misses it (landed 817c0fa)
  model: sonnet · severity: high · builds Rust: yes · area: agent/context · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`
  change: Extend repeated-exploration detection to account for a cumulative sequence of reads and searches that repeatedly returns to the same source file through different tools, ranges, or intervening low-value inspection calls. Before further exploration, show a concise retained-findings summary and a concrete next action; reset the budget on a meaningful edit, test, or new evidence.
  acceptance: Replaying the Click #3822 pattern detects repeated investigation of `src/click/types.py` well before 52 tool calls and 1.6M recorded tokens, prompts the agent to edit or state a blocker, and leaves productive focused rereads unaffected.
  test: `cargo test -p tm-agent repeated_exploration_nudge && cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1702/pallets-click-3822/tm.log:12-91` (three dispatches repeatedly inspect `src/click/types.py` without a change); `/tmp/tm-trials/20260926-1702/pallets-click-3822/tm.log:94` (run ends without patch or evidence); `/tmp/tm-trials/20260926-1702/pallets-click-3822/tm.log:232-240` (stats record 1,634,307 tokens and 52 tool calls)
- [x] **t20260926-1702-pallets-click-3822-honest-escalated-edit-message** — Avoid claiming retained edits when an escalated no-submit run left none (landed babf45f)
  model: sonnet · severity: medium · builds Rust: yes · area: cli/ux · deps: none
  files: `crates/tm-cli/src/sched.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-cli/src/sched.rs`
  change: Make the `Escalated if did_not_submit` message branch on `workspace_edits_retained`, as the Ready/Blocked branch already does. When false, explain that no working-tree edits were retained and point to the saved attempt and the appropriate retry command; only tell the user to inspect/resume retained edits when edits actually exist.
  acceptance: An escalated no-submit outcome with `workspace_edits_retained = false` never claims edits remain, while an outcome with retained edits still gives the inspect-and-retry guidance; add regression coverage for both.
  test: `cargo test -p tm-cli run_outcome_escalated_state_plain_message && cargo test -p tm-cli run_outcome_`
  evidence: `/tmp/tm-trials/20260926-1702/pallets-click-3822/tm.log:94` (failure without a patch); `/tmp/tm-trials/20260926-1702/pallets-click-3822/tm.log:114` (message says working-tree edits were retained and directs `git status`/`git diff`); `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-cli/src/sched.rs:1171-1175` (escalated branch omits the `workspace_edits_retained` check that exists in the Ready/Blocked branch)
- [x] **t20260926-1702-psf-requests-7432-align-submit-evidence-prompt** — Align worker instructions with ticket submission evidence requirements (landed 58d4b2e)
  model: sonnet · severity: medium · builds Rust: yes · area: agent/prompt · deps: none
  files: `crates/tm-agent/src/prompt.rs`
  change: Update the ticketed-worker finishing instructions so they do not tell agents an empty evidence list is acceptable when ticket.submit requires evidence. Explain how to store a concise verification/result artifact and cite its returned ID in ticket.submit; keep the guidance accurate for blocked work as well.
  acceptance: Prompt tests assert the worker instructions require an evidence artifact for successful work and do not offer an empty-list path; a scripted ticketed run stores a result artifact and submits successfully without an avoidable evidence-rejection retry.
  test: `cargo test -p tm-agent prompt::tests`
  evidence: `/private/tmp/tm-trials/20260926-1702/psf-requests-7432/tm.log:60-63` shows the empty-evidence ticket.submit rejection followed by artifact storage and a successful submission; corresponding worker wording is in `crates/tm-agent/src/prompt.rs:213-216`.
- [x] **t20260926-1702-sindresorhus-ky-878-provider-recovery-guidance** — Show a concrete recovery path for an unregistered provider candidate (landed ca15fa8)
  model: sonnet · severity: medium · builds Rust: yes · area: provider UX · deps: none
  files: `crates/tm-provider/src/fabric.rs`
  change: Extend the `preflight_role` unregistered-provider error to identify the missing provider candidate and provide the exact supported next step to either register/configure that provider or replace the candidate with a currently registered one; keep the warning that retrying unchanged configuration cannot work.
  acceptance: A preflight failure for `coder.fast` configured with `devpass` tells the user how to inspect available providers and what configuration action to take, while continuing to fail before an agent turn; tests assert the message includes both the missing provider and actionable repair guidance.
  test: `cargo test -p tm-provider preflight_role`
  evidence: `/private/tmp/tm-trials/20260926-1702/sindresorhus-ky-878/tm.log:4` reports `provider not registered: devpass` and points to `tm provider list`, but does not explain how to register that provider or replace the candidate.
- [x] **t20260926-1702-spf13-cobra-2257-ticket-usage-summary** — Make ticket usage totals easy to retrieve (landed 5c1e444)
  model: sonnet · severity: low · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/tickets.rs`, `crates/tm-cli/src/stats.rs`
  change: Add actual recorded usage totals (at least total tokens and wall time) to `tm ticket show` text and JSON, aggregating the ticket's `usage.recorded` events without confusing ticket budget limits with consumed usage.
  acceptance: After a run, `tm ticket show <id> --json` and human-readable output display the same actual total token count as `tm stats --ticket <id> --json`; before any usage is recorded, show a clear zero/no-usage value and do not print the unlimited budget sentinel as spent tokens.
  test: `cargo test -p tm-cli tickets`
  evidence: `/tmp/tm-trials/20260926-1702/spf13-cobra-2257/tm.log` lines 173-203 show `tm ticket show --json` budget `tokens: 18446744073709551615` and spent `tokens: 0`; lines 256-274 show `tm stats --ticket T-1 --json` with the actual `tokens_total: 3661949`. Source: `crates/tm-cli/src/tickets.rs` lines 481-572 serializes the ticket directly without event usage; `crates/tm-cli/src/stats.rs` lines 1-9 documents aggregation of durable usage events.
- [x] **t20260926-1903-gohugoio-hugo-15360-retry-scheduled-run-guidance** — Align provider failure guidance with scheduled retry state (landed a3eb434)
  model: sonnet · severity: medium · builds Rust: yes · area: provider · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: When a provider-unavailable failure has already scheduled `ticket.retry_scheduled`, do not end with an unconditional instruction to immediately run `tm run <ticket>` again. Report that a retry is scheduled, explain what will trigger it (or provide the exact resume command if it will not run automatically), and include any known rate-limit delay. Preserve the manual rerun instruction for failures without a scheduled retry.
  acceptance: A CLI test with a provider 429 and scheduled retry reports the scheduled state and a single correct next action, not a contradictory blind manual rerun; a failure without a scheduled retry still offers a valid manual recovery command.
  test: `cargo test -p tm-cli sched`
  evidence: `/tmp/tm-trials/20260926-1903/gohugoio-hugo-15360/tm.log:11` says `Run tm run T-1 again to retry`; line 33 records `ticket.retry_scheduled`.
- [x] **t20260926-1903-pallets-click-3822-provider-backoff** — Retry transient provider rate limits with clear progress (landed 5fc1802)
  model: sonnet · severity: high · builds Rust: yes · area: provider · deps: none
  files: `crates/tm-cli/src/sched.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-cli/src/sched.rs`
  change: When a run fails with a transient provider rate-limit response (HTTP 429), apply a bounded retry/backoff policy where safe; tell the user it is waiting and show the retry timing, rather than immediately ending with a generic provider-unavailable failure and a manual rerun suggestion.
  acceptance: A simulated 429 followed by a successful provider response completes the same ticket without manual `tm run`; logs and CLI output show the retry count and delay, while permanent/non-retryable failures remain clearly reported.
  test: `cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-1903/pallets-click-3822/tm.log` lines 17 and 47: `429 Too Many Requests` and `Run tm run T-1 again to retry.`
- [ ] **t20260926-1903-psf-requests-7432-rate-limit-recovery** — Make provider 429 recovery delay actionable
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: When a run fails because a provider rate-limits the request, carry any known Retry-After duration into the user-facing failure guidance and avoid recommending an immediate blind `tm run <ticket>` retry; if no duration is available, clearly say the retry delay is unknown.
  acceptance: A simulated provider 429 with Retry-After produces a failure message that tells the user how long to wait before retrying; a 429 without Retry-After explicitly says no delay was supplied and still preserves the ticket/session recovery command.
  test: `cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-1903/psf-requests-7432/tm.log:23,75` — each provider 429 returned “Rate limit exceeded. Please retry after a brief wait” followed by `Run tm run T-1 again to retry`; the immediate retry also returned 429.
- [ ] **t20260926-1903-sindresorhus-ky-878-run-retry-summary** — Surface scheduled retry and accurate elapsed time after provider errors
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: When `tm run` ends after a provider-unavailable error and a retry has been scheduled, include that retry state and the exact command to resume it in the terminal summary. Report wall time using the actual run duration rather than the one-second stats value observed for this attempt.
  acceptance: A simulated transient provider failure reports that a retry is scheduled, gives the correct recovery command, and displays elapsed time consistent with measured command duration; retain the ticket retry event.
  test: `cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-1903/sindresorhus-ky-878/tm.log:7-10` — run ends with provider 429 and suggests retry; recorded command wall time is 13.56s, while `tm stats` reports `wall_seconds: 1`; events show `ticket.retry_scheduled` at line 25.
- [ ] **t20260926-1903-spf13-cobra-2257-rate-limit-recovery-copy** — Make provider rate-limit retries recover cleanly
  model: sonnet · severity: medium · builds Rust: yes · area: provider · deps: none
  files: `crates/tm-provider/src/providers/compat.rs`, `crates/tm-cli/src/sched.rs`
  change: On a provider 429, honor `Retry-After` and avoid sending an immediate duplicate model request; make the final ticket/run message agree with whether `ticket.retry_scheduled` exists, state what will trigger it, and show one unambiguous recovery command. In the observed run, `tm run T-1` received 429 twice and the output advised running `tm run T-1` again while the event log showed `ticket.retry_scheduled`.
  acceptance: A deterministic 429-with-Retry-After test proves no request is retried before the indicated delay; a CLI-level test proves a retry-scheduled ticket does not display an immediate manual-retry instruction and instead explains the scheduler action, while unscheduled failures still provide a valid manual recovery command.
  test: `cargo test -p tm-provider providers::compat && cargo test -p tm-cli sched`
  evidence: `/tmp/tm-trials/20260926-1903/spf13-cobra-2257/tm.log` contains `429 Too Many Requests` twice and `ticket.retry_scheduled` (event 30 and event 73); the emitted failure message says `Run tm run T-1 again to retry`. Source: `crates/tm-provider/src/providers/compat.rs` (429 retry behavior) and `crates/tm-cli/src/sched.rs` lines 1164-1198 produce the manual and scheduler recovery text.
- [ ] **t20260926-1929-pallets-click-3822-provider-429-retry** — Retry transient provider rate limits in foreground ticket runs
  model: sonnet · severity: medium · builds Rust: yes · area: scheduler · deps: none
  files: `crates/tm-cli/src/sched.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When a provider returns HTTP 429 during `tm run`, honor any retry-after delay and retry the current ticket attempt within the bounded foreground run instead of returning immediately with a failed ticket and only scheduling a later retry. Emit concise progress that makes the wait and retry visible.
  acceptance: A deterministic provider test returning 429 once and then success completes the same foreground run successfully, records the retry, and emits an understandable retry message; persistent 429s still terminate clearly after the configured bound.
  test: `cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-1929/pallets-click-3822/tm.log` line 7 — `provider was unavailable ... 429 Too Many Requests ... Run tm run T-1 again to retry.`
- [ ] **t20260926-1929-psf-requests-7432-no-progress-read-loop** — Steer repeated source rereads toward implementation before the no-progress cutoff
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Track repeated reads/searches of the same relevant source range within an agent turn and, before `NO_PROGRESS_STEP_LIMIT` fails the ticket, send a concise steering message containing the already-read location and require a concrete next action (edit, focused test, or submitted evidence); avoid returning the same broad search result as fresh context.
  acceptance: An agent fixture that repeats reads of one source file reaches a focused edit or a clearly reported bounded stop before the hard no-progress failure; its trace shows the repeated-read warning and does not silently consume the full no-progress allowance repeating identical exploration.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1929/psf-requests-7432/tm.log:13-23` — the run repeatedly read `src/requests/models.py` (including four consecutive read events) and repeated a search, then reported “stopped after 10 steps without a repository change or submitted evidence.”
- [ ] **t20260926-1929-spf13-cobra-2257-no-progress-continuation** — Continue bounded investigations after the no-progress stop
  model: sonnet · severity: medium · builds Rust: yes · area: agent
  files: `crates/tm-agent/src/agent_loop.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: When the no-progress guard fires after ten steps, preserve the accumulated investigation and provide a clear, usable recovery path that actually resumes those findings within an explicit bound, rather than ending the only `tm run` attempt after repetitive reads/searches. Make the human-facing message agree with the ticket's retry state and whether another command is required.
  acceptance: A deterministic agent-loop test with ten investigation-only steps followed by a valid patch proves the bounded continuation can complete and retains prior findings; a failure-path test proves the emitted recovery instruction agrees with whether a retry is scheduled.
  test: `cargo test -p tm-agent no_progress`
  evidence: `/tmp/tm-trials/20260926-1929/spf13-cobra-2257/tm.log` lines 6-16 show repeated reads/searches followed by `stopped after 10 steps without a repository change or submitted evidence` and `Run tm run T-1 again to retry`; `tm.log` lines 32-39 record `ticket.retry_scheduled`. `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs` lines 1760-1767 returns `AgentOutcome::Failed` immediately at the no-progress limit.
- [ ] **t20260926-1929-sindresorhus-ky-878-no-progress-retry-guidance** — Make no-progress cutoff and retry guidance actionable
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs`
  change: When a ticket reaches the no-progress step limit, include whether a retry was scheduled, clarify whether the user should wait or rerun `tm run <ticket>`, and summarize the useful work actually retained (including whether a source patch exists) rather than offering generic recovery text.
  acceptance: A no-progress failure with `ticket.retry_scheduled` produces a terminal summary consistent with the event and states one unambiguous next action; tests cover both scheduled and unscheduled retry cases.
  test: `cargo test -p tm-agent && cargo test -p tm-cli`
  evidence: `/private/tmp/tm-trials/20260926-1929/sindresorhus-ky-878/tm.log:18` — “stopped after 10 steps without a repository change or submitted evidence” and “Run `tm run T-1` again to retry”; events 64–65 record `ticket.retry_scheduled`.
- [ ] **t20260926-1946-BurntSushi-ripgrep-3376-provider-429-foreground-retry** — Recover a foreground `tm run` from transient provider rate limits
  model: sonnet · severity: medium · builds Rust: yes · area: provider · deps: none
  files: `crates/tm-provider/src/providers/compat.rs`, `crates/tm-cli/src/sched.rs`
  change: When a provider returns HTTP 429 during `tm run`, honor any retry-after guidance and apply bounded backoff/retry to the current foreground attempt instead of ending the run after the rate-limited turn; report the delay and each retry clearly, and preserve the ticket/session if the retry bound is exhausted.
  acceptance: A deterministic provider test returning 429 once and then success completes the same foreground run with retry progress shown; persistent 429 responses terminate within the configured bound with the ticket/session and a specific recovery step intact.
  test: `cargo test -p tm-provider && cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-1946/BurntSushi-ripgrep-3376/tm.log:13` — `429 Too Many Requests`; run ended with exit code 2 and escalated ticket T-1 without implementing or verifying a patch.
- [ ] **t20260926-1946-pallets-click-3822-no-progress-reorientation** — Reorient after repeated investigation before failing a coding ticket
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`
  change: When the ticket agent reaches the no-progress step limit after investigation-only tool calls, use the retained investigation summary to issue one explicit, focused reorientation turn toward a repository change and relevant check before returning `AgentOutcome::Failed`; if it still cannot progress, report the failed state and the exact `tm ticket retry <id>` recovery command clearly.
  acceptance: A ticket that performs 10 investigation-only steps but has actionable findings is given a focused patch attempt instead of immediately ending with the current generic continuation text; if the retry attempt also stalls, output explicitly says the ticket failed and names the command to retry it.
  test: `cargo test -p tm-agent repeated_investigation_hits_bounded_threshold_and_keeps_actionable_summary`
  evidence: `/tmp/tm-trials/20260926-1946/pallets-click-3822/tm.log` lines 38-41 — “stopped after 10 steps without a repository change or submitted evidence” followed by “Run `tm ticket retry T-1` to try again.”
- [ ] **t20260926-1946-psf-requests-7432-rate-limit-recovery** — Preserve rate-limit classification and recover from transient 429s
  model: sonnet · severity: medium · builds Rust: yes · area: provider · deps: none
  files: `crates/tm-provider/src/anthropic.rs`, `crates/tm-cli/src/sched.rs`
  change: Preserve a provider's HTTP 429/RateLimited classification through the agent-run failure path instead of presenting it as generic “provider unavailable”; when the provider supplies Retry-After, surface the wait duration and perform a bounded retry or give one clear timed retry action rather than an untimed generic retry instruction.
  acceptance: An integration test simulating 429 with Retry-After shows the actual rate-limit reason, does not immediately exhaust the ticket attempt, and either retries after the specified delay within a configured bound or tells the user exactly when/how to retry; non-rate-limit 5xx failures retain their existing behavior.
  test: `cargo test -p tm-provider && cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-1946/psf-requests-7432/tm.log:25` — “provider was unavailable” despite the embedded “429 Too Many Requests” / “Rate limit exceeded. Please retry after a brief wait.”; the run exits 2 and the following event records ticket failure.
- [ ] **t20260926-1946-sindresorhus-ky-878-report-existing-work-on-stall** — Report existing patch state before declaring a ticket made no change
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`
  change: Before returning the no-progress failure in `agent_loop.rs`, distinguish the current no-progress streak from the whole run's workspace state; when a prior turn has left a source diff, avoid saying “No patch ... was submitted” without checking it, and tell the user whether that patch remains unverified and how to resume verification. Preserve the bounded stop rather than silently claiming success.
  acceptance: A run that edits a source file and later reaches the no-progress threshold reports that a patch remains in the workspace, clearly states that verification/submission did not complete, and gives a precise retry or verification recovery command; an actually unchanged run continues to report no patch.
  test: `cargo test -p tm-agent no_progress_tests`
  evidence: `/private/tmp/tm-trials/20260926-1946/sindresorhus-ky-878/tm.log` lines 46-46 and 118-127 — tm reported “No patch or evidence was submitted” and “Run `tm ticket retry T-1` to try again,” while the same run left a diff in `source/utils/body.ts` implementing the fix.
- [ ] **t20260926-1946-spf13-cobra-2257-guided-no-progress-continuation** — Guide implementation after an investigation stalls
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`
  change: When the no-progress step limit is reached after useful file/search investigation but no edit or submitted evidence, start one bounded continuation turn with a concise actionable instruction grounded in the findings (identify the likely edit target, make the change, run its focused check, and submit evidence) before failing/escalating. Preserve the current terminal failure behavior if that continuation also makes no progress.
  acceptance: A scripted task whose first investigation reaches the current no-progress limit but has identified the relevant code can make an edit and submit evidence in the guided continuation without restarting the ticket; a continuation that still makes no progress fails with an actionable message and does not loop indefinitely.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-1946/spf13-cobra-2257/tm.log` line 27: “stopped after 10 steps without a repository change or submitted evidence” after repeated reads of `completions.go` and `command.go`; lines 28 and 49-67 show a 101.808s run and 357389 reported tokens.
- [ ] **t20260926-2011-gohugoio-hugo-15360-recover-from-provider-throttling** — Make repeated investigation and provider throttling failures more actionable
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs`
  change: When an agent repeats the same source reads/searches without edits and then hits a provider 429, stop redundant exploration sooner or summarize retained findings before retry; honor a provider retry delay where available and report clearly whether retry is scheduled, what happened, and the one best next action. Avoid presenting repeated dispatch notices as meaningful progress.
  acceptance: A scripted run that repeats a file read and receives a 429 produces one concise failure summary stating no patch/tests occurred, retains useful findings, and presents a bounded retry action consistent with retry state; it does not dispatch duplicate immediate provider requests.
  test: `cargo test -p tm-agent && cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-2011/gohugoio-hugo-15360/tm.log:13-36` — repeated reads/searches and dispatch notices, then “provider was unavailable” with `429 Too Many Requests`; event log lines 39-58 record follow-on events and failure.
- [ ] **t20260926-2011-pallets-click-3822-bound-repeated-investigation** — Stop repeated source inspection before the no-progress limit
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When consecutive overlapping reads/searches have already identified the target implementation and typing test, nudge the agent to act on the retained findings and run one focused check before spending more steps on the same files; keep the nudge bounded and retain the existing hard stop if it still makes no progress.
  acceptance: A deterministic run with repeated reads of one source file and a relevant typing test receives a single actionable nudge before the no-progress limit, then can edit and submit evidence; a scripted run that ignores the nudge still terminates without an unbounded loop.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-2011/pallets-click-3822/tm.log:6-31` — repeated `Read src/click/types.py` and overlapping `sed` calls, followed by repeated `Ticket T-1 dispatched for execution` notices; ticket T-1 attempt 1 ended after 10 steps without a repository change.
- [ ] **t20260926-2011-pallets-click-3822-provider-429-visible-recovery** — Make repeated provider rate-limit retries visible and actionable
  model: sonnet · severity: high · builds Rust: yes · area: provider · deps: none
  files: `crates/tm-cli/src/sched.rs`, `crates/tm-provider/src/providers/compat.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: Preserve HTTP 429/rate-limit classification through foreground `tm run`, surface any retry timing and whether a retry is automatic, and avoid making multiple opaque attempts before reporting a provider failure; after retry exhaustion, give one precise recovery action that reflects the ticket's actual state.
  acceptance: A deterministic provider returning 429 twice and then success shows bounded retry progress and completes the same run; a persistent 429 exits within the bound with the rate-limit reason, ticket/session state, and a valid non-contradictory next action.
  test: `cargo test -p tm-provider -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-2011/pallets-click-3822/tm.log:32,58` — attempt ended with `provider was unavailable ... 429 Too Many Requests`; event log recorded `ticket.escalated` and advised rerunning, while `tm ticket show T-1 --json` showed the same 429 on attempts 2 and 3.
- [ ] **t20260926-2011-psf-requests-7432-actionable-retry** — Show the direct retry command after a no-progress ticket run
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`
  change: In both no-progress failure details, retain the investigation summary and session-resume option but also print the exact ticket recovery commands (`tm ticket retry <ticket-id>` followed by `tm run <ticket-id>`), clearly distinguishing retrying the failed ticket from resuming a provider session.
  acceptance: When a ticket stops at the no-progress bound, the returned failure text gives the user a copyable retry-and-run command using the actual ticket ID and separately labels the optional session-resume path; the regression test asserts those details.
  test: `cargo test -p tm-agent repeated_investigation_hits_bounded_threshold_and_keeps_actionable_summary`
  evidence: `/tmp/tm-trials/20260926-2011/psf-requests-7432/tm.log:33` — "Continue the saved session with `tm --resume S-3`"; run report says "Run `tm ticket retry T-1` to try again."
- [ ] **t20260926-2011-sindresorhus-ky-878-provider-429-bounded-recovery** — Recover cleanly from provider rate limits during `tm run`
  model: sonnet · severity: high · builds Rust: yes · area: provider · deps: none
  files: `crates/tm-cli/src/sched.rs`, `crates/tm-provider/src/providers/compat.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: Preserve explicit HTTP 429/rate-limit classification through foreground `tm run`; honor retry timing with a small bounded backoff when a retry is useful, and report clearly when throttling prevents completion instead of leaving an immediate manual retry as the only recovery.
  acceptance: A deterministic provider returning 429 and then success completes the same run after bounded, visible retry; a persistent 429 exits within the run bound and reports the rate-limit reason, retry status, and one valid next action without claiming tests or changes were completed.
  test: `cargo test -p tm-provider -p tm-cli`
  evidence: `/private/tmp/tm-trials/20260926-2011/sindresorhus-ky-878/tm.log:34` — `provider was unavailable ... 429 Too Many Requests`; run exited 2 after 67 seconds and recommended `tm ticket retry T-1`, but no completion or verification followed.
- [ ] **t20260926-2011-spf13-cobra-2257-carry-context-across-transient-retries** — Preserve investigation context when a provider rate limit interrupts a run
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`
  change: When a retryable provider-unavailable response interrupts a ticket attempt, retain a concise carry-forward note of the relevant files/symbols already located and the next planned action, and inject it into the next attempt instead of restarting repository discovery from the original ticket alone. Keep retries bounded and report when a provider rate limit caused the retry.
  acceptance: A simulated transient 429 after the agent has located a target function leads the next attempt to continue from that location without repeating the same broad searches; the user-facing run result identifies the rate-limit retry and whether it recovered.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-2011/spf13-cobra-2257/tm.log:13-39` — the run repeats reads/searches in three dispatches, then fails with `provider was unavailable ... 429 Too Many Requests`.
- [ ] **t20260926-2037-BurntSushi-ripgrep-3376-diagnostic-preserving-tool-errors** — Preserve useful diagnostics in agent-facing command errors
  model: sonnet · severity: medium · builds Rust: yes · area: cli
  deps: none
  files: `crates/tm-cli/src/agent.rs`
  change: When normalizing `not found:` tool failures into progress text, include a bounded, sanitized summary of the underlying diagnostic and the relevant operation (for example, which lookup or command failed) instead of reducing it to the generic phrase `couldn't find that`.
  acceptance: An agent command that fails with a not-found error renders a concise human-readable explanation of what was missing while preserving the existing output length bound and avoiding secret/path leakage.
  test: `cargo test -p tm-cli agent::tests`
  evidence: `/tmp/tm-trials/20260926-2037/BurntSushi-ripgrep-3376/tm.log:76` — `Ran a command -> error: couldn't find that`

- [ ] **t20260926-2037-BurntSushi-ripgrep-3376-dispatch-attempt-progress** — Distinguish retries in ticket dispatch progress
  model: sonnet · severity: low · builds Rust: yes · area: cli
  deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: Avoid emitting indistinguishable `Ticket <id> dispatched for execution.` progress lines across retry/reattempt cycles; include the attempt number and whether this is a retry, or emit the dispatch notice only once and explicitly mark subsequent attempts.
  acceptance: A run with multiple dispatch attempts presents one unambiguous initial dispatch and labels each subsequent attempt/retry so a user can distinguish continued recovery from accidental duplicate work.
  test: `cargo test -p tm-cli sched::tests`
  evidence: `/tmp/tm-trials/20260926-2037/BurntSushi-ripgrep-3376/tm.log:72-91` — identical dispatch messages at lines 72, 83, and 88 before the provider rate-limit failure at line 91
- [ ] **t20260926-2037-gohugoio-hugo-15360-no-progress-run-does-not-finish-scoped-fix** — Prevent repeated investigation from ending a focused ticket before a minimal implementation can be attempted
  model: sonnet · severity: high · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When the repeated-inspection/no-progress guard fires on a ticket, preserve the useful stop signal but route the run through actionable recovery that asks for one focused edit and relevant check before returning terminal failure; avoid re-reading the same source and history when a relevant implementation file has already been located.
  acceptance: On a focused decoder bug where the agent has identified the target function but has not yet edited, the run makes a patch and executes a relevant test instead of terminating after 10 steps; if it still cannot proceed, the user-facing failure identifies the exact saved session and a direct retry/resume command, and usage totals match the run's reported totals.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-2037/gohugoio-hugo-15360/tm.log` — `stopped after 10 steps without a repository change or submitted evidence` followed by `Usage: 182057 tokens, 10 tool calls, 13 seconds. Continue the saved session with tm --resume S-3`; same log's stats output reports `"tokens_total": 376235`.
- [ ] **t20260926-2037-pallets-click-3822-rate-limit-recovery-guidance** — Make exhausted provider rate limits actionable in foreground runs
  model: sonnet · severity: high · builds Rust: yes · area: provider · deps: none
  files: `crates/tm-cli/src/sched.rs`, `crates/tm-provider/src/types.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: Preserve and present provider rate-limit classification and any retry timing in the final `tm run` failure summary; when the bounded provider retries are exhausted, state whether retrying is safe, show the ticket's resulting state, and offer a precise next action rather than only raw 429 text and a generic retry instruction.
  acceptance: A deterministic provider that returns 429 until retry exhaustion makes `tm run` exit with a concise message that identifies rate limiting, includes available retry timing, accurately names the ticket state, and gives a valid recovery action; a subsequent successful retry completes without confusing duplicate dispatch output.
  test: `cargo test -p tm-provider -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-2037/pallets-click-3822/tm.log:16-30,46-52` — `tm run` emitted repeated dispatch notices, ended with `provider was unavailable ... 429 Too Many Requests`, and the event stream recorded `ticket.escalated` without usage/retry timing.
- [ ] **t20260926-2037-psf-requests-7432-clear-recovery-path** — Make bounded-stop recovery instructions unambiguous
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`
  change: When a repeated-inspection stop occurs, distinguish resuming the provider session from retrying the failed ticket and give one explicit, ordered recovery sequence that matches the ticket's current state; do not present `tm --resume <session>` and `tm ticket retry <ticket>` as unexplained competing commands.
  acceptance: A regression test asserts the repeated-inspection failure message identifies the required recovery path for the current ticket/session state and provides copyable commands in the right order, with session resume clearly labeled as an alternative only when valid.
  test: `cargo test -p tm-agent repeated_source_inspections_have_an_early_run_level_bound`
  evidence: `/tmp/tm-trials/20260926-2037/psf-requests-7432/tm.log:32` — “Continue the saved session with `tm --resume S-3` ... Run `tm ticket retry T-1` to try again.”
- [ ] **t20260926-2037-sindresorhus-ky-878-machine-readable-ticket-id** — Provide a machine-readable ticket ID when creating a ticket
  model: sonnet · severity: medium · builds Rust: yes · area: cli · deps: none
  files: `crates/tm-cli/src/tickets.rs`
  change: Add a stable JSON output mode for `tm ticket new` that emits the created ticket ID as a structured field, while preserving the current human-readable confirmation for interactive use.
  acceptance: Running `tm ticket new "sample task" --json` emits valid JSON with an `id` field containing only a valid ticket ID, and that value can be passed directly to `tm run` without parsing human prose.
  test: `cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-2037/sindresorhus-ky-878/tm.log:3` — `ticket ID must look like T-<n> ... got "Created ticket T-1: ..."`
- [ ] **t20260926-2037-spf13-cobra-2257-clear-dispatch-and-rate-limit-recovery** — Clarify dispatch progress and recover cleanly from provider rate limits
  model: sonnet · severity: medium · builds Rust: yes · area: scheduler · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-cli/src/sched.rs`
  change: When a ticket run encounters a provider rate-limit response, apply a bounded backoff/retry policy before failing. Ensure repeated scheduler/dispatch progress notices identify whether a new attempt has started, and on exhaustion report the rate limit and one actionable recovery sequence rather than an opaque provider error.
  acceptance: A test simulating a 429 verifies bounded retry/backoff and eventual success or a final actionable failure; output tests prove repeated dispatch notices distinguish new attempts from repeated ticks and that exhausted retries name the provider issue and next step.
  test: `cargo test -p tm-cli rate_limit && cargo test -p tm-cli dispatch`
  evidence: `/tmp/tm-trials/20260926-2037/spf13-cobra-2257/tm.log:7-18` — three “Ticket T-1 dispatched for execution.” notices preceded “provider was unavailable: ... 429 Too Many Requests”; final failure advised `tm ticket retry T-1`.
- [ ] **t20260926-2225-ripgrep-no-progress-retry-loop** — Stop or redirect repeated no-progress retries
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`
  change: When a ticket repeatedly hits the no-progress/repeated-inspection guard, do not schedule another identical discovery attempt. Surface one clear next action to the user, or pass the retained investigation into a genuinely focused continuation; make the displayed `tm --resume <session>` instruction consistent with whether `tm run` is scheduling another attempt.
  acceptance: A deterministic test drives successive no-submit attempts that repeat the same file reads and searches; it proves the loop does not perform three identical 10-step attempts and that the terminal message accurately describes whether automatic retry or manual resume is required.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-2225/BurntSushi-ripgrep-3376/tm.log` lines 29, 41, 65, 72: three attempts each stopped at 10 steps without a change, the same `walk.rs` investigation was repeated, 612969 tokens were used, then `Ticket state: escalated. No retry is scheduled.`
- [ ] **t20260926-2225-pallets-click-3822-repeated-inspection-token-guard** — Bound repeated-inspection churn before it consumes excessive tokens
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Make the repeated-exploration guard account for repeated reads/searches and token spend, and emit a concise recovery message when the agent is stuck before allowing another identical inspection cycle.
  acceptance: A ticket-run test with repeated reads of the same source terminates or redirects before excessive token use, preserves the session for resume, and reports accurate token/tool-call counts with one clear next command.
  test: `cargo test -p tm-agent repeated_exploration`
  evidence: `/tmp/tm-trials/20260926-2225/pallets-click-3822/tm.log:104` — “stopped after 10 steps without a repository change”; same line reports “213149 tokens, 11 tool calls, 30 seconds.”
- [ ] **t20260926-2225-pallets-click-3822-provider-rate-limit-recovery** — Recover transient provider rate limits with bounded backoff
  model: sonnet · severity: medium · builds Rust: yes · area: provider-retry · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs`
  change: Classify provider 429/rate-limit responses as transient, apply a bounded backoff before retrying, and make the final ticket output explain whether the user should retry the ticket or resume its saved session.
  acceptance: A simulated sequence of provider 429s followed by availability retries after backoff without repeating completed investigation; when retries are exhausted, output names the exact recovery command and does not expose confusing internal provider identifiers as the primary explanation.
  test: `cargo test -p tm-agent provider_unavailable`
  evidence: `/tmp/tm-trials/20260926-2225/pallets-click-3822/tm.log:87` — “429 Too Many Requests”; `/tmp/tm-trials/20260926-2225/pallets-click-3822/tm.log:105` — final error requires manual `tm ticket retry T-2`.
- [ ] **t20260926-2225-psf-requests-7432-recover-from-repeated-inspection** — Prevent high-token repeated-inspection attempts from resuming the same unproductive search
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When the repeated-inspection limit trips, summarize the inspected symbols and require the next attempt to take a materially different action (make a focused edit, run a targeted check, or report a concrete blocker); avoid automatically scheduling retries that repeat the same reads and searches without patch/evidence.
  acceptance: A scripted agent that repeats the same inspections across attempts is redirected to a distinct action or stopped with a clear blocker, without accumulating another no-change inspection attempt; existing focused-recovery coverage remains green.
  test: `cargo test -p tm-agent a_first_no_submit_attempt_persists_its_investigation_and_reports_what_is_unverified`
  evidence: `/tmp/tm-trials/20260926-2225/psf-requests-7432/tm.log` lines 117-139 — three attempts report "stopped after 6 repeated inspections" and "No patch or evidence was submitted", with 136543, 118049, and 138589 usage tokens.
- [ ] **t20260926-2225-sindresorhus-ky-878-bound-retry-context-waste** — Bound no-progress retries and make recovery status actionable
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/agent.rs`
  change: When an agent run hits the no-progress step limit, avoid repeating exploratory work on automatic retry; surface a concise continuation instruction that includes useful findings and an actionable fix/test target. Replace opaque tool/schema and submission errors with messages identifying the failed action and valid recovery command, and report aggregate token/time consumption over retries.
  acceptance: A replay of a run that reaches the no-progress limit and then recovers does not repeat the same probes, makes a repository change, runs a relevant test, and submits evidence; the final user-facing result exposes retry count and aggregate usage. Invalid structured tool input and a failed submit each produce an error that names the cause and a valid next action.
  test: `cargo test -p tm-agent -p tm-cli`
  evidence: `/tmp/tm-trials/20260926-2225/sindresorhus-ky-878/tm.log:36` — `stopped after 10 steps without a repository change ... Usage: 170850 tokens, 10 tool calls ... A retry is scheduled`; lines 47, 62-64 show the malformed-field and submit errors.
- [ ] **t20260926-2225-spf13-cobra-2257-no-progress-recovery-guidance** — Make no-progress stops continue productively and explain recovery consistently
  model: sonnet · severity: medium · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When the step/no-progress guard fires after useful code investigation, perform one bounded reorientation toward a concrete edit or test before failing. If the run must stop, make the message state whether work will retry automatically or tell the user the exact manual command; do not say both “A retry is scheduled” and “Continue the saved session with tm --resume”.
  acceptance: On a task whose first turns repeatedly inspect the same source files, the run either attempts a scoped implementation/test or returns a single unambiguous recovery instruction consistent with ticket state and retry events; it does not issue contradictory automatic/manual retry guidance.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-2225/spf13-cobra-2257/tm.log:16-28` — provider retry followed by “stopped after 10 steps” and `tm --resume S-2`, while a retry is scheduled; lines 29-35 show the further retry and final `tm ticket retry T-2` instruction.
- [ ] **t20260926-2354-BurntSushi-ripgrep-3376-stop-repeat-inspection-earlier** — Redirect repeated source inspection before burning the retry budget
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When an attempt has already inspected the same two source files and repeated overlapping searches or shell ranges without making a change, carry forward those findings and require the next retry to inspect a new hypothesis or make a focused edit; stop escalating after the same unproductive pattern recurs instead of replaying near-identical reads.
  acceptance: A regression run with three attempts repeating reads/searches of the same two files redirects on the first repeated attempt and terminates before exceeding 50,000 cumulative tokens, with one explicit next action rather than another list of repeated tool calls.
  test: `cargo test -p tm-agent repeated_exploration`
  evidence: `/tmp/tm-trials/20260926-2354/BurntSushi-ripgrep-3376/tm.log:25` — Attempt 1 ended after repeated reads/ranges in `walk.rs` and `dir.rs`; lines 39 and 61 show attempts 2 and 3 repeat those files; line 69 reports “235757 tokens, 10 tool calls, 54 seconds.”
- [ ] **t20260926-2354-BurntSushi-ripgrep-3376-one-recovery-command-on-escalation** — Show one supported recovery command after retries are exhausted
  model: sonnet · severity: medium · builds Rust: yes · area: ticket-ux · deps: none
  files: `crates/tm-cli/src/sched.rs`, `crates/tm-agent/src/agent_loop.rs`
  change: Make the escalated failure summary choose either resuming a saved session or retrying the ticket, explain what that command does, and avoid presenting `tm --resume` as the action before subsequently telling the user the retry is unscheduled and to run `tm ticket retry`.
  acceptance: An escalated no-change run prints exactly one executable recovery command that matches the ticket/session state and succeeds in making the work runnable without requiring the user to choose between resume and retry.
  test: `cargo test -p tm-cli run_outcome_escalated_state_plain_message`
  evidence: `/tmp/tm-trials/20260926-2354/BurntSushi-ripgrep-3376/tm.log:69` — “Continue the saved session with `tm --resume S-3` and make a focused change”; line 78 ends “Ticket state: escalated. No retry is scheduled. Run `tm ticket retry T-1` to try again.”
- [ ] **t20260926-2354-pallets-click-3822-repeat-cycle-token-budget** — Redirect repeated investigation before it burns a large token budget
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When repeated reads/searches revisit the same files and symbols without an edit or evidence, stop the loop earlier or change the next-step prompt to a concrete implementation action; avoid replaying the same broad exploratory commands across retries.
  acceptance: A regression run with repeated reads of the same small set of source ranges redirects or terminates before accumulating more than 50,000 tokens, and the retained summary gives one focused next action rather than a list of prior tool invocations.
  test: `cargo test -p tm-agent repeated_exploration`
  evidence: `/tmp/tm-trials/20260926-2354/pallets-click-3822/tm.log:93` — “stopped after 10 steps without a repository change”; retries at lines 105 and 117 repeat overlapping source reads; line 117 reports “218490 tokens, 10 tool calls, 126 seconds.”
- [ ] **t20260926-2354-pallets-click-3822-single-recovery-command** — Give escalated users one consistent recovery action
  model: sonnet · severity: medium · builds Rust: yes · area: ticket-ux · deps: none
  files: `crates/tm-cli/src/sched.rs`, `crates/tm-agent/src/agent_loop.rs`
  change: Make the escalation summary select one supported next command based on whether the saved session is resumable or the ticket needs retry, and explain the choice in plain language; do not present `tm --resume` and `tm ticket retry` as competing instructions.
  acceptance: An escalated no-change run prints one executable recovery command, explains what it resumes/retries, and the chosen command transitions the ticket into a runnable state without requiring the user to infer which recovery path applies.
  test: `cargo test -p tm-cli run_outcome_escalated_state_plain_message`
  evidence: `/tmp/tm-trials/20260926-2354/pallets-click-3822/tm.log:93` — “Continue the saved session with `tm --resume S-1`”; line 117 advises `tm --resume S-3`, then ends “Run `tm ticket retry T-1` to try again.”
- [ ] **t20260926-2354-psf-requests-7432-stop-repeated-inspection-token-waste** — Redirect repeated source inspections into a concrete next action
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When an agent repeatedly reads/searches the same implementation without editing or submitting evidence, steer it toward a focused change or targeted test before allowing another attempt to replay the same investigation; carry forward only the needed findings rather than retransmitting repeated source context.
  acceptance: A deterministic agent-loop fixture that repeats the same source inspection across attempts is redirected to an edit/test or stopped with a concise, actionable blocker, and its recorded token usage does not grow through repeated replay of equivalent inspection context.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260926-2354/psf-requests-7432/tm.log:21-43` — three failed attempts report repeated inspections and no patch/evidence with 96,665, 90,717, and 167,092 usage tokens (354,474 total); the final message recommends continuing a saved session, then separately says `Run tm ticket retry T-1 to try again`.
- [ ] **t20260926-2354-psf-requests-7432-unify-run-recovery-guidance** — Give one recovery path after a no-progress run
  model: sonnet · severity: medium · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs`
  change: Reconcile the agent-loop no-progress guidance to resume a saved session with the foreground run's terminal recovery message, and render one unambiguous next command based on whether the ticket is ready, escalated, or has an automatic retry scheduled.
  acceptance: CLI tests for a no-progress failure verify that the run output and final ticket summary agree on whether to resume, retry, or wait for a scheduled retry, and expose only the valid action for the resulting ticket state.
  test: `cargo test -p tm-agent no_progress && cargo test -p tm-cli sched`
  evidence: `/tmp/tm-trials/20260926-2354/psf-requests-7432/tm.log:21-43` — the attempt failure advises `tm --resume S-3`, while the final run error says `Run tm ticket retry T-1 to try again`; `crates/tm-agent/src/agent_loop.rs:1777` emits the resume instruction and `crates/tm-cli/src/sched.rs` formats foreground recovery messages.
- [ ] **t20260926-2354-sindresorhus-ky-878-bound-no-progress-retries** — Bound and align recovery after no-progress agent stops
  model: sonnet · severity: high · builds Rust: yes · area: agent/recovery · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs`
  change: When the agent loop stops for repeated exploration or the no-progress step limit, avoid immediately launching another fresh attempt that repeats the same investigation. Either resume the recorded session with the focused continuation already named in the stop reason, or stop automatic retries and make that explicit in `tm run` output; enforce a bounded token budget for these continuations.
  acceptance: A scripted no-progress ticket does not reread/research the same files across multiple fresh attempts, the user sees one consistent recovery command, and the run exits within its configured token budget while preserving findings.
  test: `cargo test -p tm-agent && cargo test -p tm-cli`
  evidence: `/private/tmp/tm-trials/20260926-2354/sindresorhus-ky-878/tm.log:18-66` shows two 10-step no-submit attempts, each recommending `tm --resume S-*` before scheduling another attempt; `tm.log:109-117` records T-2's 675280 tokens and 36 tool calls.
- [ ] **t20260926-2354-spf13-cobra-2257-repeat-investigation-action** — Redirect repeated investigation into a focused code change
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When repeated reads and searches revisit the same source files and anchors without a patch or submitted evidence, stop early or change the continuation prompt to a concrete implementation action instead of replaying the same exploration in a fresh attempt.
  acceptance: A regression run that repeats reads/searches without changing the repository redirects or terminates before exceeding 50,000 reported tokens across retries, and its continuation gives a single scoped next action.
  test: `cargo test -p tm-agent repeated_exploration`
  evidence: `/tmp/tm-trials/20260926-2354/spf13-cobra-2257/tm.log:17` — “stopped after 6 repeated inspections without a repository change”; lines 29 and 41 show two more no-change attempts, with line 41 reporting “193365 tokens, 10 tool calls, 54 seconds.”
- [ ] **t20260926-2354-spf13-cobra-2257-single-escalation-recovery** — Show one consistent recovery command after escalation
  model: sonnet · severity: medium · builds Rust: yes · area: ticket-ux · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs`
  change: Make a no-progress escalation message choose either resume or retry based on the ticket/session state, and explain the selected command plainly; do not suggest resuming one session and then tell the user to retry the ticket.
  acceptance: An escalated no-change run displays one valid recovery command, its effect is stated, and executing it transitions the saved work into a runnable state.
  test: `cargo test -p tm-cli run_outcome_escalated_state_plain_message`
  evidence: `/tmp/tm-trials/20260926-2354/spf13-cobra-2257/tm.log:41` — “Continue the saved session with `tm --resume S-3`”; line 42 then says “Run `tm ticket retry T-1` to try again.”
- [ ] **t20260927-0158-BurntSushi-ripgrep-3376-no-progress-retry-context** — Turn repeated exploration into a concrete next action
  model: sonnet · severity: high · builds Rust: yes · area: agent-context · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When a run reaches the no-progress/repeated-inspection bound, carry forward a concise set of confirmed findings and give the next attempt a specific required action (for example, edit the identified matcher path or run a focused reproducer) instead of restarting with broad searches and repeated reads. If that action still does not produce a repository change or evidence, stop rather than launching another equivalent exploration cycle.
  acceptance: Replaying this ripgrep investigation does not repeat the same reads of `dir.rs` and `walk.rs` across all retries; after the first no-progress bound, the next attempt gets the prior findings and a focused action, and the run terminates after one unproductive recovery attempt.
  test: `cargo test -p tm-agent repeated_investigation_hits_bounded_threshold_and_keeps_actionable_summary && cargo test -p tm-agent repeated_source_inspections_have_an_early_run_level_bound`
  evidence: `/tmp/tm-trials/20260927-0158/BurntSushi-ripgrep-3376/tm.log:12-85` — three attempts repeat reads/searches of `crates/ignore/src/dir.rs` and `walk.rs`; each says no patch/evidence was submitted.
- [ ] **t20260927-0158-BurntSushi-ripgrep-3376-retry-state-guidance** — Make failure recovery instructions match scheduler retry state
  model: sonnet · severity: medium · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/sched.rs`, `crates/tm-agent/src/agent_loop.rs`
  change: Render one recovery instruction based on the scheduler's actual state. When an automatic retry is scheduled, tell the user it will continue and avoid presenting `tm --resume` as the immediate next step; when retries are exhausted, show the supported manual command (`tm ticket retry <ticket>`). Ensure the agent-loop failure detail and scheduler summary do not conflict.
  acceptance: A retryable no-progress failure reports that retry is scheduled without also asking the user to resume manually; after the retry budget is exhausted, the final message presents `tm ticket retry <ticket>` and does not refer to a stale session ID.
  test: `cargo test -p tm-cli repeated_no_submit_failure_gets_focused_recovery_guidance && cargo test -p tm-agent no_progress`
  evidence: `/tmp/tm-trials/20260927-0158/BurntSushi-ripgrep-3376/tm.log:29,52,77,85` — the log pairs `Continue the saved session with tm --resume S-1/S-2/S-3` with “A retry is scheduled,” then later uses `tm ticket retry T-1` after escalation.
- [ ] **t20260927-0158-tm-no-progress-retry** — Make no-progress retries use prior findings and stop repeating exploration
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs`
  change: When a ticket reaches the repeated-inspection or no-progress step limit, preserve the useful findings in the resumed prompt and prevent automatic retries from simply replaying the same source reads. Make the CLI distinguish a retry that is actually scheduled from a run that has stopped/escalated, and provide one unambiguous recovery command.
  acceptance: A regression test replaying a ticket with repeated reads of the same implementation ranges verifies the next attempt receives the prior findings and is steered to edit, test, or state a concrete blocker; repeated identical investigation is bounded and the user-facing final state/recovery instruction matches scheduler state.
  test: `cargo test -p tm-agent`
  evidence: `/private/tmp/tm-trials/20260927-0158/pallets-click-3822/tm.log:68-78` and `:79-160` show overlapping reads of `src/click/types.py`, three attempts ending without a repository change or evidence; `:175-176` says “A retry is scheduled” immediately before the final run error; usage lines report 177065, 241346, and 249100 tokens across attempts.
- [ ] **t20260927-0158-psf-requests-7432-limit-repeated-inspection-cost** — Prevent repeated file reads from consuming the run budget without progress
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When the no-progress/repeated-exploration guard sees the same file and search repeated, stop replaying the full inspection context and direct the next turn to a specific proposed edit or targeted test; make the guard's token/cost ceiling effective before several redundant large reads have already been charged.
  acceptance: A deterministic agent-loop scenario with repeated reads of one large source file terminates or redirects before redundant context is repeatedly sent, records bounded usage, and still allows a focused follow-up edit based on retained findings.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260927-0158/psf-requests-7432/tm.log:6-32` — attempts 1–3 repeat reads/searches in `src/requests/models.py`, submit no patch/evidence, and report 96,496, 97,947, and 124,752 tokens (319,195 total); `crates/tm-agent/src/agent_loop.rs:1768-1779` only returns failure after `REPEATED_EXPLORATION_LIMIT` and formats usage after those inspections.
- [ ] **t20260927-0158-psf-requests-7432-align-recovery-guidance** — Show one recovery action that matches the ticket's final state
  model: sonnet · severity: medium · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs`
  change: Reconcile the no-progress stop's saved-session continuation instruction with the foreground run's final retry/escalation summary; tell the user whether to resume that session or retry the ticket, and provide one valid next command for the final state.
  acceptance: A test exercising repeated-inspection failures through both scheduled retry and final escalation asserts that the agent failure message and terminal CLI summary provide consistent, state-valid recovery instructions.
  test: `cargo test -p tm-agent no_progress && cargo test -p tm-cli sched`
  evidence: `/tmp/tm-trials/20260927-0158/psf-requests-7432/tm.log:13-33` — first/second attempts say “Continue the saved session with `tm --resume S-1`/`S-2`” while noting a retry is scheduled; the final attempt still recommends `tm --resume S-3`, then reports escalation and `tm ticket retry T-1`.
- [ ] **t20260927-0158-sindresorhus-ky-878-specific-not-found-errors** — Preserve actionable details for missing agent resources
  model: sonnet · severity: medium · builds Rust: yes · area: agent-ux · deps: none
  files: `crates/tm-cli/src/agent.rs`
  change: In `plain_tool_error`, replace the generic `not found:` rendering (`couldn't find that`) with a concise diagnostic naming the missing resource and the relevant identifier, while keeping the message short enough for tool output.
  acceptance: A missing artifact/ticket identifier appears in the rendered tool error with a concrete correction hint; unrelated error categories retain their current wording.
  test: `cargo test -p tm-cli plain_tool_error`
  evidence: `/tmp/tm-trials/20260927-0158/sindresorhus-ky-878/tm.log` — `Tried to submit the ticket -> error: couldn't find that`
- [ ] **t20260927-0158-sindresorhus-ky-878-retry-guidance-state** — Align failed-attempt recovery guidance with automatic retries
  model: sonnet · severity: low · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: When `tm run` has already scheduled an automatic retry, make the recovery message clearly say the scheduler will continue the ticket and defer manual `tm --resume`/retry instructions unless the automatic retry does not start or is exhausted.
  acceptance: A captured no-submit run with an automatic retry scheduled does not present simultaneous manual continuation instructions as the immediate next action; a non-retryable failure still gives its direct recovery command.
  test: `cargo test -p tm-cli repeated_no_submit_failure_gets_focused_recovery_guidance`
  evidence: `/tmp/tm-trials/20260927-0158/sindresorhus-ky-878/tm.log` — `Continue the saved session with `tm --resume S-1` ... A retry is scheduled.` followed by a second `Ticket T-1 dispatched for execution.`
- [ ] **t20260927-0158-spf13-cobra-2257-repeat-investigation-action** — Redirect repeated investigation into a focused code change
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When repeated reads and searches revisit the same source files and anchors without a patch or submitted evidence, stop early or change the continuation prompt to a concrete implementation action instead of replaying the same exploration in a fresh attempt.
  acceptance: A regression run that repeats reads/searches without changing the repository redirects or terminates before exceeding 50,000 reported tokens across retries, and its continuation gives a single scoped next action.
  test: `cargo test -p tm-agent repeated_exploration`
  evidence: `/tmp/tm-trials/20260927-0158/spf13-cobra-2257/tm.log:23` — “stopped after 6 repeated inspections without a repository change”; lines 35 and 47 show two more no-change attempts, with line 47 reporting “194694 tokens, 10 tool calls, 26 seconds.”
- [ ] **t20260927-0158-spf13-cobra-2257-single-escalation-recovery** — Show one consistent recovery command after escalation
  model: sonnet · severity: medium · builds Rust: yes · area: ticket-ux · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs`
  change: Make a no-progress escalation message choose either resume or retry based on the ticket/session state, and explain the selected command plainly; do not suggest resuming one session and then tell the user to retry the ticket.
  acceptance: An escalated no-change run displays one valid recovery command, its effect is stated, and executing it transitions the saved work into a runnable state.
  test: `cargo test -p tm-cli run_outcome_escalated_state_plain_message`
  evidence: `/tmp/tm-trials/20260927-0158/spf13-cobra-2257/tm.log:23` — “Continue the saved session with `tm --resume S-1`”; line 48 then says “Run `tm ticket retry T-1` to try again.”
- [ ] **t20260927-0335-BurntSushi-ripgrep-3376-cumulative-no-progress-budget** — Prevent repeated tm retries from exhausting context without progress
  model: sonnet · severity: high · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Make repeated-exploration/no-progress limits cumulative across automatic retries in a single `tm run`, and avoid retrying identical inspection-only sessions unless the saved-session prompt directs a concrete next action; report aggregate spend and a concise recovery command.
  acceptance: A run that repeats the same source inspections across retries stops within one bounded no-progress budget, preserves the existing session, reports total tokens/tool calls/time across attempts, and does not burn a fresh full context budget on each retry.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260927-0335/BurntSushi-ripgrep-3376/tm.log:88-143` — three attempts repeated reads/searches without a patch, consuming 185380, 183596, and 236211 tokens respectively; the final recovery message directs resuming but `tm run` automatically retried the first two inspection-only failures.
- [ ] **t20260927-0335-pallets-click-3822-resume-repeated-investigation** — Avoid restarting the same no-progress investigation on automatic retries
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When a no-submit attempt fails and a retry is scheduled, carry the retained investigation into the next attempt and require a materially new action instead of repeating the same source reads/searches; make the user-facing recovery message consistent about whether tm will retry automatically or requires `tm --resume`.
  acceptance: A scripted ticket that returns the same source inspections without a patch does not repeat the inspections across three attempts, does not spend hundreds of thousands of tokens on the loop, and ends with one unambiguous recovery instruction.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260927-0335/pallets-click-3822/tm.log:109-161` — three attempts report no patch/evidence and suggest resuming saved sessions; the retries repeat source discovery, with `tm.log:161` reporting 221944 tokens for attempt 3; `/tmp/tm-trials/20260927-0335/pallets-click-3822/tm.log:233` reports 579262 total tokens.
- [ ] **t20260927-0335-psf-requests-7432-no-progress-recovery** — Recover productively from repeated repository inspections
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When ticket-mode runs reach the repeated-inspection or no-progress guard, feed a targeted recovery instruction back into the saved session (identify the relevant code, make one scoped change, and run a focused test) before declaring the attempt failed, rather than simply ending after exploration.
  acceptance: A regression test simulates repeated reads with no edits followed by a viable code change and verifies the run recovers and submits evidence; a genuinely stuck run still terminates within a bounded number of recovery turns.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260927-0335/psf-requests-7432/tm.log:14-35` — three attempts consumed 358,691 reported tokens overall with repeated reads and no patch; each attempt ended for lack of progress.
- [ ] **t20260927-0335-psf-requests-7432-terminal-recovery-guidance** — Make failure recovery guidance match ticket retry state
  model: sonnet · severity: medium · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Generate the no-progress failure detail based on whether a retry is actually scheduled or the ticket has been escalated; when no retry remains, state the exact available user command to resume/retry, rather than instructing users to continue a session while the run reports that no retry is scheduled.
  acceptance: Tests assert that retryable failures show the scheduled retry path, while escalated failures show an actionable manual recovery command and do not claim a retry is scheduled.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260927-0335/psf-requests-7432/tm.log:32-33` — final failure recommends `tm --resume S-3`, then immediately says “No retry is scheduled” and `tm ticket retry T-1` is required.
- [ ] **t20260927-0335-sindresorhus-ky-878-ticket-new-machine-readable-id** — Make new-ticket output directly usable by scripted runs
  model: sonnet · severity: high · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/tickets.rs`
  change: Add a stable machine-readable output mode for `tm ticket new` that emits the created ticket ID as a structured field while retaining the human-readable default; document the flag so callers can safely capture and pass the ID to `tm run`.
  acceptance: A scripted `tm ticket new ... --json` returns valid JSON with an `id` field that can be passed directly to `tm run`, while default human output remains understandable.
  test: `cargo test -p tm-cli ticket_new`
  evidence: `/tmp/tm-trials/20260927-0335/sindresorhus-ky-878/tm.log:6-7` — command substitution captured `Created ticket T-1: ...` instead of `T-1`; `tm run` rejected it as an invalid ticket ID.
- [ ] **t20260927-0335-sindresorhus-ky-878-tool-error-repair-context** — Reduce retries and context spent on malformed tool calls
  model: sonnet · severity: medium · builds Rust: yes · area: agent-runtime · deps: none
  files: `crates/tm-agent/src/tools.rs`
  change: On tool argument validation failures, return the exact expected schema/field type and a compact correction example to the model, then preserve the useful investigation state so a retry does not repeat already-completed reads and experiments.
  acceptance: A deterministic scenario with a malformed shell/tool input surfaces the required field shape, corrects the call on the next attempt, and does not repeat completed discovery; token use is bounded relative to a successful focused run.
  test: `cargo test -p tm-agent tools::tests`
  evidence: `/tmp/tm-trials/20260927-0335/sindresorhus-ky-878/tm.log:31-63,68-84` — malformed-field and shell-spawn errors led to repeated snippets and a retry; the agent reported 168,788 tokens for the run.
- [ ] **t20260927-0335-spf13-cobra-2257-no-progress-focused-continuation** — Turn repeated investigation into a focused implementation step
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When repeated inspections reach the no-progress threshold, use the retained findings to steer one bounded continuation toward a concrete edit, relevant check, and evidence submission instead of re-running the same discovery. Stop and summarize clearly if that continuation cannot proceed; avoid paying for repeated identical reads/searches.
  acceptance: A deterministic loop scenario with repeated reads followed by enough context to identify an edit produces a patch and runs the targeted check without repeating the investigation; an unrecoverable scenario stops with a concise no-change/no-test summary and bounded token use.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260927-0335/spf13-cobra-2257/tm.log:14-38` — three attempts stopped after repeated reads/searches; the run used 485,153 reported tokens and submitted no patch or evidence.
- [ ] **t20260927-0335-spf13-cobra-2257-recovery-message-consistency** — Give one recovery instruction matching ticket retry state
  model: sonnet · severity: high · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: Ensure no-progress stop output and the final `tm run` error agree on whether a retry is scheduled and whether the user should resume a session, run the ticket again, or call `tm ticket retry`. Emit one state-accurate next command and explain automatic retry status without presenting conflicting choices.
  acceptance: For scheduled retry, ready ticket, and escalated ticket fixtures, captured output states retry status accurately and presents exactly the corresponding valid next command; no output pairs `tm --resume` with a conflicting ticket-retry instruction.
  test: `cargo test -p tm-cli`
  evidence: `/tmp/tm-trials/20260927-0335/spf13-cobra-2257/tm.log:14-24,38-40` — stop messages requested `tm --resume S-*`, claimed `A retry is scheduled`, and the final message instead instructed `tm ticket retry T-1`.
- [ ] **t20260927-0538-BurntSushi-ripgrep-3376-cumulative-no-progress-retry-budget** — Stop retries from repeating repository inspections without progress
  model: sonnet · severity: high · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Carry no-progress usage and completed investigation findings across automatic retries in one `tm run`; stop scheduling another identical inspection-only attempt and report aggregate tokens, tool calls, elapsed time, and one actionable recovery command.
  acceptance: Three consecutive inspection-only attempts consume at most one configured no-progress budget, do not reread the same source files on retry, and produce one final summary with cumulative usage and a valid next command.
  test: `cargo test -p tm-agent`
  evidence: `/private/tmp/tm-trials/20260927-0538/BurntSushi-ripgrep-3376/tm/tm.log:25-49` — attempts 1–3 repeated reads/searches, each stopped after 10 steps without a repository change, and spent 192079, 183316, and 237931 tokens; the final user-visible recovery guidance only appears after all three attempts.
- [ ] **t20260927-0538-BurntSushi-ripgrep-3376-cargo-target-diagnostics** — Explain Cargo's configured target directory before broad artifact searches
  model: sonnet · severity: medium · builds Rust: yes · area: tools · deps: none
  files: `crates/tm-agent/src/tools.rs`
  change: When shell/build tool output indicates Cargo built successfully but the agent's expected `target/debug` artifact is absent, expose the effective Cargo target directory and relevant environment/configuration in the command result or a focused diagnostic so the agent can locate artifacts without scanning outside the project.
  acceptance: With `CARGO_TARGET_DIR` configured outside the repository and a successful cargo build, the agent receives the effective target path and can locate the binary without issuing a filesystem-wide search.
  test: `cargo test -p tm-agent`
  evidence: `/private/tmp/tm-trials/20260927-0538/BurntSushi-ripgrep-3376/tm/tm.log:25` — the first attempt ran `cargo build`, repeated `cargo build --bin rg`, then issued `find / -maxdepth 6 -name "rg"` after looking only in `target/debug`.
- [ ] **t20260927-0538-gohugoio-hugo-15360-no-progress-loop** — Turn repeated inspection stops into a focused continuation
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When repeated inspections/no-progress thresholds are reached, preserve the discovered target and require the next attempt to take a concrete action (edit, targeted test, or explain a blocker) rather than restarting the same file/history exploration. Enforce a cumulative token/context budget across attempts and stop or ask for user input when it is exhausted.
  acceptance: On this Hugo BOM task, the saved attempts do not repeatedly inspect decoder.go and git history without change; the run either submits a focused patch with relevant test evidence or exits with a concise, actionable blocker before exceeding the configured cumulative budget.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260927-0538/gohugoio-hugo-15360/tm.log:74-108` — the corrected verbatim-text ticket made three repeated-inspection/no-progress attempts with no patch or evidence; `tm stats --json` reported 466386 tokens for T-2 (996809 across both tickets).
- [ ] **t20260927-0538-gohugoio-hugo-15360-recovery-guidance** — Make no-progress recovery instructions match ticket state
  model: sonnet · severity: medium · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/sched.rs`
  change: Emit exactly one state-aware recovery action in the terminal failure summary. Do not tell users to resume a session while retries are being scheduled or end with a different retry command; explain whether the ticket is Ready or Escalated and give the valid next command.
  acceptance: A no-progress run that retries and later escalates presents a single final command valid for the final ticket state; the message does not simultaneously imply automatic retry and require manual retry/resume.
  test: `cargo test -p tm-cli run_outcome`
  evidence: `/tmp/tm-trials/20260927-0538/gohugoio-hugo-15360/tm.log:86-108` — stop text advised `tm --resume S-4/S-5/S-6` while retries were scheduled, then the final output said `Run tm ticket retry T-2`; the attempts ended escalated.
- [ ] **t20260927-0538-pallets-click-3822-no-progress-recovery** — Make repeated-investigation stops carry forward findings and give one accurate recovery action
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`, `crates/tm-cli/src/sched.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: When repeated reads/searches hit the no-progress guard, steer the same bounded run toward an edit and focused verification using the findings already gathered; if it cannot proceed, emit only the recovery instruction that matches actual ticket/session/retry state, and explain token totals across attempts. Avoid launching retries that replay the same inspection sequence.
  acceptance: A deterministic agent-loop test with repeated reads followed by a valid patch completes using retained investigation, and a stop-path test asserts the displayed recovery action matches retry state and reports coherent cumulative usage; a trial-like repeated-inspection fixture does not replay the same discovery on every retry.
  test: `cargo test -p tm-agent no_progress`
  evidence: `/private/tmp/tm-trials/20260927-0538/pallets-click-3822/tm.log:31-56` — three attempts repeatedly inspect `src/click/types.py` and `tests/typing/typing_prompt.py`, end with “No patch or evidence was submitted”, suggest `tm --resume S-1/S-2/S-3`, then say “A retry is scheduled” / “Run `tm ticket retry T-1`”; reported attempt usage is 168474, 209939, and 216616 tokens despite each attempt lasting 21–41 seconds.
- [ ] **t20260927-0538-psf-requests-7432-stop-repeating-investigation** — Make repeated-inspection recovery actionable and consistent
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`, `/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-cli/src/sched.rs`
  change: When a run hits the repeated-inspection limit, do not automatically relaunch the same investigation unchanged. Either stop and provide one accurate next action, or create a guided retry that makes a focused code change using the retained findings. Align the agent-loop failure text with the scheduler's actual behavior: this run told the user to `tm --resume S-1`, then automatically retried two more times, and finally told the user to run `tm ticket retry T-1`.
  acceptance: A repeated-inspection trial cannot silently repeat the same searches on automatic retries; its retained diagnosis is presented to the next attempt as a focused action, or the ticket is left stopped with one command that actually resumes/retries it. Add a regression test asserting the displayed recovery instruction matches the resulting ticket/session state.
  test: `cargo test -p tm-agent repeated_exploration`
  evidence: `/tmp/tm-trials/20260927-0538/psf-requests-7432/tm.log:15-40` — three attempts stop without a patch; retry messages name `tm --resume S-1`/`S-2`/`S-3` while runs restart automatically. `tm stats --json` reports `tokens_total: 456874`.
- [ ] **t20260927-0538-spf13-cobra-2257-no-progress-steering** — Turn repeated investigation into a bounded concrete next action
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/agent_loop.rs, /Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: When a run repeatedly reads the same source files and searches the same terms across automatic attempts without editing or submitting evidence, carry its findings forward and steer the next attempt to one focused code change plus a targeted check; stop redundant attempts before they repeat the same exploration and report cumulative usage.
  acceptance: A deterministic fixture with repeated inspections across retries demonstrates that later attempts receive prior findings, take a materially different edit/test action or stop within a configured bound, and report cumulative token/time usage without replaying the same sequence.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260927-0538/spf13-cobra-2257/tm.log:27-61` — three attempts reread `completions.go` and `command.go` and repeat searches; each ends without a repository change, and stats report 535499 total tokens.
- [ ] **t20260927-0538-spf13-cobra-2257-recovery-state-copy** — Give one recovery instruction that matches ticket retry state
  model: sonnet · severity: medium · builds Rust: yes · area: scheduler · deps: none
  files: `crates/tm-agent/src/agent_loop.rs, crates/tm-cli/src/sched.rs, /Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate`
  change: Make no-progress messages distinguish a retry that will run automatically from a failed/escalated ticket requiring user action; emit a single valid next command and avoid recommending `tm --resume <session>` when the actual next action is ticket retry or scheduler execution.
  acceptance: CLI tests cover scheduled retry, unscheduled failure, and escalation; each output states the actual ticket state and exactly one actionable recovery command that can proceed from that state.
  test: `cargo test -p tm-cli sched && cargo test -p tm-agent no_progress`
  evidence: `/tmp/tm-trials/20260927-0538/spf13-cobra-2257/tm.log:37-62` — each attempt recommends `tm --resume S-n` while stating a retry is scheduled; after escalation the run says `Run tm ticket retry T-1 to try again`.
- [ ] **t20260927-0742-BurntSushi-ripgrep-3376-resume-no-progress-attempts** — Resume from retained findings after repeated no-progress attempts
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs` (`/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate/crates/tm-agent/src/agent_loop.rs`)
  change: When a ticket attempt hits the repeated-inspection/no-progress limit, carry its investigation summary into the next automatic attempt and instruct the agent to make a focused change or run a targeted check instead of re-reading the same files and re-running the same irrelevant environment probes. Ensure the user-facing retry/resume instruction matches whether tm will automatically schedule a retry or requires an explicit resume.
  acceptance: A simulated ticket with repeated reads and no edits does not repeat those reads in its next attempt; the next-attempt prompt includes the prior useful findings, and the emitted next-step command accurately describes the actual automatic/manual retry behavior.
  test: `cargo test -p tm-agent repeated_exploration`
  evidence: `/tmp/tm-trials/20260927-0742/BurntSushi-ripgrep-3376/tm.log` lines 25, 42, 54 — three 10-step attempts re-read `crates/ignore/src/dir.rs` and `walk.rs`, used 205553/188817/219857 tokens, and reported “Continue the saved session” while scheduling retries; source: `crates/tm-agent/src/agent_loop.rs` lines 1781-1817.
- [ ] **t20260927-0742-pallets-click-3822-shell-cwd-validation** — Prevent shell launches from using invalid generated working directories
  model: sonnet · severity: medium · builds Rust: yes · area: agent · deps: none
  files: `crates/tm-agent/src/tools.rs` (/Users/allie/Develop/ticket-master/.claude/worktrees/tm-integrate)
  change: Before dispatching shell.run, resolve and validate its effective cwd against the project root; on an invalid or missing cwd, fall back only when protocol semantics permit, otherwise return a concise error that names the bad path and gives a valid project-relative correction. Ensure `/bin/sh` is never spawned with a synthetic `/null` path.
  acceptance: A regression test passes an absent/null cwd through the shell.run dispatch path and proves either safe project-root execution per documented defaults or a structured actionable error; no process-start error refers to a synthesized `<project>/null` directory.
  test: `cargo test -p tm-agent shell_run`
  evidence: `/tmp/tm-trials/20260927-0742/pallets-click-3822/tm.log:29-39` — command `sed -n '1039,1220p' src/click/types.py` failed to start `/bin/sh` in `/private/tmp/tm-trials/20260927-0742/pallets-click-3822/tm/null` with “No such file or directory”.
- [ ] **t20260927-0742-psf-requests-7432-stop-repeated-investigation-across-retries** — Prevent automatic retries from repeating a no-progress inspection loop
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: When a run is stopped for repeated exploration, do not automatically launch a fresh attempt that begins the same read/search sequence; carry forward a concise investigation summary and require the next attempt to perform a distinct action such as editing, testing, or submitting evidence, or stop with an actionable user-facing recovery state.
  acceptance: A scripted/model test that repeats inspection-only tool calls across an attempt boundary terminates without another identical retry, preserves the prior useful findings, and presents a clear next action; productive retries remain allowed.
  test: `cargo test -p tm-agent repeated_exploration`
  evidence: `/tmp/tm-trials/20260927-0742/psf-requests-7432/tm.log` lines 20, 29, and 38 — retries each stopped on the same `src/requests/models.py` / `prepare_body` inspection with no patch; line 39 says resume manually, although the run then automatically retries through three attempts.
- [ ] **t20260927-0742-sindresorhus-ky-878-actionable-not-found-errors** — Make ticket submission errors identify the missing resource
  model: sonnet · severity: medium · builds Rust: yes · area: cli-ux · deps: none
  files: `crates/tm-cli/src/agent.rs`
  change: Replace the generic `not found` rendering (“couldn't find that”) with an actionable message that identifies the missing ticket/artifact/resource and states the expected ID or recovery command when the operation context provides it; preserve a concise fallback only when the missing entity is unknown.
  acceptance: A submission that references a nonexistent evidence artifact tells the user which artifact ID could not be resolved and how to obtain/use a valid ID; generic not-found failures remain understandable without exposing internal debug output.
  test: `cargo test -p tm-cli plain_tool_error`
  evidence: `/tmp/tm-trials/20260927-0742/sindresorhus-ky-878/tm.log` line 69: “tried to submit the ticket -> error: couldn't find that”.
- [ ] **t20260927-0742-sindresorhus-ky-878-bound-retry-context-cost** — Bound token use across repeated exploration retries
  model: sonnet · severity: high · builds Rust: yes · area: agent-loop · deps: none
  files: `crates/tm-agent/src/agent_loop.rs`
  change: Carry concise retained findings and inspected-file/search history into automatic retries after a no-progress stop, and enforce a cumulative context/token budget so retries take a new actionable step or stop early with a clear reason instead of replaying discovery.
  acceptance: A repeat-exploration fixture cannot spend multiple full-context attempts rereading the same files; the next attempt receives the prior findings, makes a distinct edit/test/evidence step or exits with actionable guidance, and reported cumulative usage agrees with recorded provider usage.
  test: `cargo test -p tm-agent`
  evidence: `/tmp/tm-trials/20260927-0742/sindresorhus-ky-878/tm.log` lines 69 and 84 show no-submit attempts reporting 190855 and 225600 tokens respectively before the retry eventually submits; ticket stats reported 615955 total tokens.
