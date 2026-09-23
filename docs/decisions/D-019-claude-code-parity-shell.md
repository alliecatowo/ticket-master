# D-019 — `tm` is Claude Code first; `tm tickets` is `claude agents`

**Status:** accepted · **Date:** 2026-09-22 · **Supersedes:** the home-screen parts of
`D-018-tui-chat-first-shell.md` (its chat screen stays and is extended here)

## Context

The project owner, after seeing D-018's "sessions & tickets" home screen live:

> I HATE THAT TM HOME SCREEN I HATE IT I WANT IT TO LOOK LIKE CLAUDES HOME SCREEN TM TICKETS SHOULD
> BE THE EQUIVALENT OF CLAUDE AGENTS AND THAT SCREEN WE DIDNT DO THE SEPERATION OF SESSION FROM
> TICKET WE DIDNT MAKE IT FAMILIAR TO CC USERS TO WIN THEM OVER WE DIDNT DO THE "EVEN WITHOUT THE
> CORE TICKETS OF TICKET MASTER THIS SHOULD BE BETTER THEN ANY TERMINAL BASED CODING HARNESS" STUFF

That restates what they had already asked for (quoted in D-017 and `AGENT_HANDOFF.md`): "tm=claude",
"same interface basically", "you do <- in a claude convo and it goes to the cross session 'agents'
view, the same thing `claude agents` goes to", and a chat screen "more beautiful and functional
than claude code / codex / opencode". D-018 built a chat screen but invented its own home screen
instead of matching the one Claude Code users already know.

The reference is Claude Code itself, from its own documentation, not memory:
[Interactive mode](https://code.claude.com/docs/en/interactive-mode) and
[Agent view](https://code.claude.com/docs/en/agent-view).

## Decision

**1. The chat is Claude Code's chat.** Someone who uses Claude Code should sit down at `tm` and
already know every key. Same keys, same input modes, same slash commands, then better where we can
be. That means:
- Esc interrupts a running turn (Esc Esc clears the draft). Ctrl+C clears the input, and a second
  press exits. Ctrl+D on an empty prompt exits.
- Shift+Tab cycles permission modes: normal, accept edits, plan. `?` on an empty prompt shows
  shortcuts, and typed text is never a shortcut.
- Input modes: `/` commands, `!` runs a shell command directly and feeds its output into the
  conversation, `@` autocompletes file paths. ↑/↓ recall history and Ctrl+R searches it.
  Shift+Enter and Ctrl+J insert a newline, and Ctrl+G opens `$EDITOR`. Long pastes collapse to
  `[Pasted text #N]`. Messages typed while a turn runs are queued.
- Emacs-style editing (Ctrl+A/E/K/U/W/Y, Alt+B/F).
- Ctrl+O toggles the transcript viewer and Ctrl+T toggles the task checklist.
- Commands: `/help`, `/clear`, `/resume`, `/compact`, `/model`, `/status`, `/cost`, `/init`,
  `/bg`, `/exit`.
- Permission prompts in Claude Code's numbered form ("1. Yes / 2. Yes, and don't ask again this
  session / 3. No, and tell tm what to do differently").
- Tool calls rendered the way Claude Code renders them (`⏺ Bash(cmd)` with a `⎿` result line), and
  edits shown as diffs.
- A welcome box and a status line in the same spirit.
- **`CLAUDE.md` is honored** alongside `AGENTS.md`, so a Claude Code user's repository instructions
  just work.
- Every conversation is saved: `tm --continue`, `tm --resume`, and `/resume` bring one back.

**2. `tm tickets` is `claude agents`.** Background work in Ticketmaster *is* tickets being worked by
workers, which is exactly what Claude Code's agent view lists. So the screen `←` opens from the chat
(on an empty prompt), and `tm tickets` opens from the shell, is the agent view with tickets as its
rows. It has the same grammar:
- A header with version, model, cwd and counts ("2 working · 1 needs input").
- Rows grouped by state: **Needs input** (escalated, awaiting approval), **Working** (leased,
  running, verifying), **Ready for review** (submitted), **Queued** (ready, blocked, draft), and
  **Completed** (closed; failures shown red, cancelled grey as Stopped).
- Each row: a status glyph (`✻`, animated `✽` while working, `∙` for no live worker) colored by
  state, then ticket id and title, a one-line summary (latest activity, result, or failure), and
  age.
- A dispatch input at the bottom. Typing a task and pressing Enter creates a ticket and queues it
  for a background worker.
- Space opens a peek panel. Enter or → attaches, opening the chat attached to that ticket with a
  recap. Ctrl+X cancels (press again to confirm). `?` shows shortcuts. Esc returns to the
  conversation `←` came from.
- The first `←` in the chat shows "Press ← again to open tickets", as Claude Code does.

**3. Sessions and tickets are separate, visibly.** Conversations are never listed as rows in the
tickets view. They are found with `/resume` and `tm --resume` (a picker of past conversations),
exactly as in Claude Code. The bridge between the two is explicit:
- `/bg <prompt>` hands the current conversation's work to a background worker as a ticket ("moved
  to the background as T-5").
- The model creates tickets itself when asked to plan work for the background (D-017).

**4. Background work runs while `tm` is open.** Claude Code has a supervisor daemon; `tm`'s
equivalent is a scheduler that runs inside the TUI process while it's open (leases keep it
from double-working anything a separate `tm sched run` or `tm serve` is also running), so dispatched
tickets actually get worked without the user having to know `tm sched run` exists.

## Why

Claude Code's users are the audience Ticketmaster has to win. A familiar surface costs them nothing
to adopt; an unfamiliar one is a reason to leave. The ticket system is Ticketmaster's advantage
underneath, but the owner's bar is explicit: even with tickets ignored, `tm` has to be the best
terminal coding harness, and that starts with parity with the one people already use.

## Implemented: core (2026-09-22)

These are the pieces the UI is built on, all in `crates/tm-cli/src/agent.rs` unless noted, and
each has its own test.
- **Interrupt.** `AgentSession::interrupter()` gives a `TurnInterrupter` that works while the
  session is locked. The turn ends as `AgentOutcome::Interrupted` with its completed steps, and
  the conversation records "[Request interrupted by user]" for the next turn.
  `tm_provider::Fabric` now releases a cancelled call's concurrency slot. Before this, a single
  interrupt wedged a long-lived fabric.
- **Approvals answered from a UI.** `run_turn_with(prompt, on_event, &mut dyn Approver)` takes an
  `ApprovalAnswer`: `Yes`, `YesForSession` (via `Oversight::approved_for_session` and
  `AgentLoop::approve_for_session`), or `No { feedback }`. On `No`, the human's words are exactly
  what the model sees, through `AgentLoop::resume_with`.
- **Permission modes.** `PermissionMode::{Auto, Plan, Ask}` and `set_mode`. Plan narrows
  authority to read-only and adds a plan-mode note to the system prompt; Ask requires approval for
  writes, shell, git, pty, and computer use.
- **Saved conversations.** Each conversation saves after every turn to
  `<state_dir>/sessions/<id>.json`. `list_sessions` lists them, `AgentSession::resume` and
  `resume_latest` reopen one, and on the CLI that's `tm -c/--continue` and `tm -r/--resume
  [ID]` (no id lists them).
- **`!` shell.** `run_shell` runs the command and adds it, with its output, to the conversation.
- **`/bg`.** `background` creates and queues a worker ticket carrying the recent conversation.
- **Background runner.** `crates/tm-cli/src/sched.rs::spawn_background_runner` runs the scheduler
  inside the process.
- **`CLAUDE.md` honored.** Every directory's `AGENTS.md` then `CLAUDE.md` is read. Separately, a
  ticketless turn now gets the project root's instructions at all: before this, instructions were
  only discovered from paths a ticket claimed, so chat saw none.
- `-p` is no longer a global flag, so subcommand `--help` output stays uncluttered.

## Implemented: tickets screen (2026-09-22)

Section 2 is built. It is a clone of the agent view as documented at
<https://code.claude.com/docs/en/agent-view> and shown in that page's screenshot, with tickets as
the rows.

- **Screen.** `crates/tm-tui/src/screens/tickets.rs` (`TicketsScreen`) replaces D-018's
  `screens/home.rs`, which is deleted along with its "Sessions" section and the `logged_sessions`
  query.
  - **Header:** a ticket-stub mark, `Ticketmaster vX.Y.Z`, `model · cwd`, and the counts
    (`2 needs input · 1 working · …`). It compacts to one line under 18 rows or 40 columns.
  - **Groups:** Needs input, Working, Ready for review, Queued, Completed. Enter on a heading
    collapses it. Completed fills the space the live groups leave and folds into `… N more`.
    That row, or the heading itself when there is no room, expands with Enter, and the fold never
    hides the selected row.
  - **Rows:** a glyph (animated `✽` while a worker works it, `✻` when a worker holds it without
    working, `∙` with no worker), then the id and title (the objective's first clause, at most 32
    columns, width-safe), then the summary, then an age aligned right. The age freezes at the
    run's length once completed.
  - **Colours** come from the theme: Needs input yellow, Working accent, Review green, Queued
    dim, failures red, stopped grey.
  - **Selection** is keyed by ticket id. It follows a ticket into another group. If the ticket
    disappears, the selection lands on whatever now sits at its old position.
- **Keys.**
  - ↑/↓ move the selection.
  - Space toggles the peek panel. The panel shows the row's summary, then the objective, state
    and attempts, time waiting, latest activity, failures, submission, and evidence. ↑/↓ walk
    ticket to ticket with it open.
  - Enter or `→` attaches the chat to the ticket (`AgentSession::attach_ticket`) and posts a
    one-line `Recap of T-n: …`. Opening tickets from an attached chat selects that ticket.
  - Ctrl+X, then Ctrl+X again within 2 s, cancels the ticket. Esc disarms.
  - `a` accepts and `r` rejects (`Store::accept`/`reject`). Reject asks for the reason in the
    input and refuses an empty one.
  - `b` opens the board and `?` shows shortcuts.
  - Esc closes the peek panel, then clears the input, then returns to the chat.
  - Ctrl+C clears the input first, then counts toward quitting.
  - Shift+Enter and Ctrl+J insert a newline.
- **Dispatch.** Typing and pressing Enter calls `tickets::create_and_queue`. That is
  `create_worker_ticket` (the defaults `tm ticket new` and `/bg` share) plus `Store::activate`.
  The new row is then selected. The footer says "Dispatched T-n to a background worker", or, if
  no worker can run it, "Queued T-n. No worker is running here: tm sched run".
- **Summaries.** `crates/tm-cli/src/tickets/overview.rs` produces them for both the TUI and
  `--json`, so the two cannot disagree. `ActivityIndex` folds the log's
  `command.started`/`goal.step_added`/`ticket.submitted`/`ticket.escalated`/`ticket.cancelled`/
  `ticket.retry_scheduled`/`ticket.leased`/`approval.*` events incrementally. It reads up to the
  head it saw first, so a concurrent append is never skipped. What each group's rows say:
  - **Working:** the latest command (`$ cargo test`) or goal step. A working-state ticket with no
    live lease says its lease lapsed rather than pretending it is being worked.
  - **Review:** the submission summary.
  - **Failed or retrying:** the failure, and `retrying in Ns`.
  - **Escalated:** the reason, and `gave up after N attempts: …` when retries ran out.
  - **Awaiting approval:** only while a worker is actually mid-run (leased or running). A new
    lease clears a dead attempt's unanswered approval.
  - **Queued with no worker anywhere:** "queued, no worker running (tm sched run)".
  - **Stopped:** the cancel reason.
  - Event-derived text is sanitized and collapsed to one line.
- **Navigation.**
  - In the chat, the first `←` on an empty prompt shows the urgent status hint "Press ← again to
    open tickets". A second press within 2 s opens tickets. `tm-cli`'s `App` intercepts this, so
    the chat screen was not edited.
  - Ctrl+T and `/tickets` still open it.
  - `tm tickets` (`args.rs`, `main.rs`) opens the TUI on the tickets screen (`tui::run_tickets`),
    and Esc goes to a fresh chat.
  - `tm tickets --json [--all]` prints `TicketOverview`s. With no project it prints `[]` and
    creates nothing. Without a tty and without `--json`, it prints a grouped plain list.
- **Background work (§4).** The TUI starts `sched::spawn_background_runner` (2 s interval) at
  launch and aborts it on exit. If the scheduler cannot start, its error shows in the header, in
  the warning colour.
- **Tests.**
  - Unit tests for grouping, summaries, the activity fold, title, age, rendering at 80x24 and
    smaller in Unicode and ASCII, peek, Ctrl+X, a/r gating, folding, the empty state, and
    selection stability.
  - Pty tests through the real binary: `crates/tm-cli/tests/tui_tickets.rs` (grouping, dispatch,
    peek, Ctrl+X twice, accept and reject, attach recap, `--json`/`--all`/no project) and the
    updated `tui_navigation.rs` (double `←`, Ctrl+T, `/tickets`, board and detail, Esc chain).

**Known gaps, stated plainly:**
- **`b` collides with typing.** The dispatch input always has focus, so `b` on an empty input
  opens the board. A task typed starting with "b" loses its first letter to the board. `a`/`r`
  have the same collision, but only on a Ready-for-review row. The brief asked for bare-letter
  keys, and Claude Code's agent view avoids them. If this bites, the fix is Ctrl-chords.
- **The peek panel has no reply input.** Claude Code's has one. Answering an escalation means
  attaching.
- **No filters or pinning.** The `a:`/`s:` filters, Ctrl+S grouping by directory, Ctrl+T pin,
  Ctrl+R rename, and Shift+↑/↓ reorder are not implemented. Ctrl+T keeps its D-018 meaning (tickets
  and back), which conflicts with §1's "Ctrl+T toggles the task checklist". The chat track has to
  settle that.
- **Summaries are mechanical,** not Haiku-written as Claude Code's are.
- **Chat text still says "sessions & tickets" and `/home`** in its welcome card, `/home`'s
  description, and a doc link in `screens/chat.rs`. That file belongs to the chat track and was
  left alone.

## What this costs, stated plainly

- It's a lot of surface. Several items need new core support: cancelling a turn mid-flight,
  approval prompts answered asynchronously from the UI, persisted and resumable conversations,
  permission modes that change what the agent may do without approval, `/bg`, and an embedded
  scheduler. Each needs its own tests; none can be faked in the UI alone.
- Copying Claude Code's conventions ties us to them. When Claude Code changes a key we either
  follow or diverge knowingly. The docs linked above are the reference to re-check.
- The D-018 home screen is replaced, not kept as an alternative.
