# D-024 — Command surfaces: the CLI tree, the slash table, and the tickets hub

**Status:** accepted · **Date:** 2026-09-23 · **Supersedes:** nothing

## Context

Three entry points into `tm` grew independently and now disagree with each other. `tm --help`
lists every subcommand — `lease`, `harness`, `bench`, `browser`, `computer`, `sched plan`, `events
replay` — at the same weight as `ticket`, `run` and `status`, so a first-time user's help output is
mostly plumbing they will never type. The chat's slash-command table (D-019's "Implemented: chat")
covers session and transcript commands but has no equivalent for the project-management surfaces
(`tm milestone`, `tm dep graph`, ticket search) that already exist as CLI verbs — a chat user has
to drop to a shell to reach them. And the tickets screen (D-019's "Implemented: tickets screen")
is one screen with no way to reach the milestones, timeline or dependency-graph views a design
pass called for, so those views, once built, have nowhere to live. A design pass over the CLI
tree, the slash table and the tickets-hub hierarchy (`docs/tasks/TASKS.md`, the `s1-*` design
tasks this doc's own task depends on being read alongside) found all three gaps at once; this doc
is the single place that names the shape all of them converge on, instead of three separate tasks
each inventing a slightly different grouping.

## Decision

**1. The CLI tree splits into four groups: daily / planning / serving / more, with plumbing hidden
but never removed.** `tm --help`'s `display_order` puts daily verbs first — `tm`, `-p`, `init`,
`status`, `tickets`, `ticket`, `run`, `search`, `symbol`, `doctor` — then planning (`milestone`,
`dep`, `decision`), then serving (`serve`, `mcp`). The fourth group, "more," is not a
`display_order` bucket at all: everything that must exist but nobody types on day one — `lease`,
`harness`, `bench`, `browser`, `computer`, `sched plan`/`sched tick`, `events replay`/`events
verify`, `ticket submit`/`ticket delegate` — gets `#[command(hide = true)]`, absent from `--help`
and top-level completion, and surfaces instead as an `after_help` "More commands" list for the
person who goes looking. Hidden is not deprecated; nothing on this list loses a test or a caller.
`tm search` also gains explicit `--exact`/`--semantic` flags, keeping `--mode` as a hidden alias.
`provider` and `auth` are untouched — they are not plumbing, they are where a new user configures
the thing before anything else works.

**2. The slash-command table grows to cover the project-management surfaces the CLI already has,**
plus the session mechanics Claude Code users expect and this project has been missing. Today's
table (`/help`, `/clear`, `/resume`, `/compact`, `/model`, `/status`, `/cost`, `/connect`,
`/provider`, `/config`, `/init`, `/bg`, `/tickets`, `/attach`, `/detach`, `/decide`, `/exit`)
gets:
- **Project-management views**, each a thin chat-side window onto a CLI verb or TUI screen:
  `/board` (opens the Kanban tab), `/milestones` (opens the Milestones tab), `/timeline` (opens
  the Timeline tab), `/deps` (renders `tm dep graph`'s output inline), `/ticket <T>` (renders `tm
  ticket show <T>`'s text inline, no screen change), `/run <T>` (activates and queues a ticket,
  replying with where to watch it).
- **Introspection**: `/context` (token use by section, plus an attached ticket's prefetched
  context-pack sections and their token cost — the same `tm-context::SectionKind` accounting the
  agent itself pays for, not an invented number) and `/todos` (the existing Ctrl+T checklist, now
  reachable by name).
- **Code navigation**: `/search <q>` (the same hybrid search `tm search` runs, inline, and it says
  "not indexed yet" rather than showing an empty result when that's the real reason nothing
  matched) and `/review [focus]` (sends a turn reviewing `git diff HEAD`, the way `/init` sends
  `init_prompt`).
- **Session and environment**: `/memory` (opens the project's `AGENTS.md` in `$EDITOR`), `/export`
  (writes the transcript to a markdown file and replies with the path), `/doctor` (runs `tm
  doctor`'s checks inline), `/permissions` (shows or sets the auto/plan/ask mode Shift+Tab
  cycles), `/workflow [name]` (lists workflows with no argument, starts one as a background ticket
  with one).

Every one of these is a window onto a surface that already exists elsewhere (a CLI verb, a TUI
screen, a config value) — the slash table's job is discoverability and staying in one screen, not
a second implementation of anything.

**3. The tickets hub gets a tab strip: Tickets · Board · Milestones · Timeline · Graph.** Today's
single tickets screen (D-019 §2) becomes the first tab of five, cycled with Tab/Shift+Tab and
still reachable directly (`Ctrl+B` still jumps straight to Board, the existing binding). A tab
whose screen doesn't exist yet shows a "coming next" placeholder rather than being absent from the
strip — the strip's shape is decided here even though Milestones, Timeline and Graph land as
separate follow-on tasks. Esc from any tab returns to the chat, matching every other screen's Esc
behavior.

**4. One set of display labels, used everywhere.** `render.rs` gets `state_label`, `kind_label`,
`milestone_state_label`, `dep_kind_label`, `budget_label` and `authority_label` — lowercase,
human words (`ready`, `in progress`, `blocks`), each the exact string `tm ticket list --state`
parses back. Every place that today does `format!("{:?}", ...)` on a domain enum (ticket list,
ticket show, milestone list, the Kanban column titles) calls the matching label function instead.
This is a rule, not just a list of call sites: a new enum-to-text path added after this doc adds a
label function next to the others rather than reaching for `{:?}` again, and a new display surface
reuses an existing label function rather than formatting the enum itself.

**5. Tickets get an optional due date.** `Ticket` gains `due: Option<NaiveDate>`, settable with
`tm ticket new/edit --due YYYY-MM-DD` (`--due none` clears it), shown in `ticket show`/`ticket
list`, and rolled up to a milestone's due date as the max of its tickets' due dates in `milestone
show`. This is what the Milestones tab (§3) and the milestone list's due-date column render.

## Why

Three people (the CLI-tree design pass, the slash-command design pass, and D-019's own tickets
screen) each ran into the same underlying problem — a surface that grew by accretion instead of
by plan — and each proposed a fix in isolation. Landing them separately risked three different
groupings that disagree with each other (a CLI `planning` group that doesn't match a slash
`/board`-`/milestones`-`/timeline` list that doesn't match a tab strip in a different order).
Naming the shape once, here, is cheaper than reconciling three drifted implementations later. The
underlying goal is the one D-019 already states: a Claude Code user should recognize every key and
every command, and the parts of `tm` that go beyond Claude Code (the ticket system's planning
surfaces) should be reachable through the same kind of shallow, discoverable command a Claude Code
user already trusts, not a separate CLI they have to already know exists.

## What this costs, stated plainly

- Hiding plumbing verbs from `--help` is a real tradeoff, not free organization: someone who
  actually needs `tm lease acquire` or `tm events replay` for debugging now has to know to check
  `after_help` or this doc, where before it was one `--help` scroll away. The `after_help` list is
  the mitigation, not a full fix.
- The slash table's new project-management commands are thin wrappers, but "thin" still means six
  more `CommandId` variants, six more match arms, and six more things that can drift from the CLI
  verb or screen they mirror if that verb's behavior changes and the slash wrapper isn't updated
  alongside it. There is no shared implementation forcing them to stay in sync beyond code review.
- The tab strip commits to five tabs before three of the screens (Milestones, Timeline, Graph)
  exist. A "coming next" placeholder is a real, visible unfinished state that ships before its
  content does, not a hidden one.
- `due: Option<NaiveDate>` is a new persisted field on every ticket going forward; it needs a
  migration path in `materialize.rs` for tickets recorded before this date, same as any other
  schema addition.

## Implemented: slash table — session and environment (2026-09-24)

The five "Session and environment" commands §2 lists (`/memory`, `/export`, `/doctor`,
`/permissions`, `/workflow`) are wired: `crates/tm-tui/src/chat/commands.rs` (the `CommandId`
variants and `COMMANDS` rows), `crates/tm-cli/src/tui/chat_ops.rs` (each command's handler,
matched in `App::run_command`), and `crates/tm-cli/src/tui/slash_views.rs` (the pure text/data
renderers each handler hands to `ChatScreen::push_notice`).

- **`/memory`** reads `AGENTS.md` (empty if it doesn't exist yet) and hands that text to the same
  `tm_tui::chat::editor::edit` Ctrl+G already uses, then writes what the editor saved back to the
  real path — no new editor entry point was needed, since `edit` already accepts arbitrary
  starting text rather than assuming a scratch buffer.
- **`/export [path]`** turns the attached `AgentSession`'s saved turns into markdown
  (`slash_views::export_markdown`) and writes it to `path`, or `tm-session-<id>.md` in the current
  directory when no path is given.
- **`/doctor`** calls `crate::project::doctor` — the exact function `tm doctor` calls — with
  `skip_computer_probe: true`. One real wrinkle: `doctor` always prints its own report through
  `Renderer::emit`, which (unlike `Renderer::note`) is not suppressed by `--quiet`, so there is no
  quiet way to call it and get only the returned `DoctorReport` back. `run_doctor` deliberately
  does *not* use Ctrl+G's editor's leave-the-alternate-screen dance for this: that would leave
  `emit`'s raw table sitting in the user's real terminal scrollback (the primary screen buffer)
  even after the TUI quits. Instead it lets `emit` print into the alternate screen buffer and
  immediately asks for a full repaint (`ChatScreen::force_full_repaint`) to overwrite it, and
  separately renders `DoctorReport.checks` into a transcript notice, so the pass/fail summary is
  what's actually left behind in scrollback.
- **`/permissions [mode]`** reads/sets `ChatScreen::mode`/`set_mode` plus the session's own mode
  the same way `ChatAction::SetMode` (Shift+Tab) already does.
- **`/workflow [name]`** bare discovers and parses `.tm/workflows/*.toml` directly in
  `chat_ops.rs` rather than calling `crate::workflow::workflow_list`, whose discovery/parsing
  helpers (`discover_names`, `load`) are private to that module and whose own list rendering goes
  through the same always-prints `Renderer::emit` path `doctor` does. Given a name, it mirrors
  `crate::workflow::workflow_run`'s own expand → `tm_genesis::compile::validate_graph` → commit
  pipeline locally, because `workflow_run` reports success only through quiet-suppressible
  `Renderer::note` calls and does not hand its `CommitOutcome` back to the caller; running the
  pipeline directly gives `/workflow` the created ticket ids, which it then queues with
  `Store::activate`, the same way `/bg` queues a single ticket, and reports per-ticket
  queued/failed status back into the transcript.

What this leaves open: the small duplication between `chat_ops.rs`'s inlined workflow
discover/expand/commit logic and `crate::workflow`'s own copies is a real seam — a future pass
that makes `discover_names`/`load` `pub(crate)` and gives `workflow_run` a variant that returns
`CommitOutcome` instead of only printing would let `/workflow` call through to `tm-cli`'s existing
workflow module instead of re-implementing its core, the same way `/doctor` reuses
`crate::project::doctor` outright.
