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
- **`/model`.** `AgentSession::set_model("provider/model" | "model" | "default")`, plus
  `model()` and `model_choices()`. The choice is saved with the conversation, and
  `tm_provider::Fabric::prefer` routes the chat role to it first, keeping the configured candidates
  behind it as fallbacks. Building this exposed a real bug: the fabric chose a `(provider, model)`
  candidate but handed only the request to the provider, which always served the model it was
  built with. So every role table's `model` field did nothing (the "Haiku fallback" was Sonnet),
  and `Registry` refused tables that named two models under one provider rather than misroute.
  `CompletionRequest::model` now carries the routed model to every backend; see
  `docs/providers.md`. The default table's stale ids (`claude-haiku-3.5`, `claude-opus-4-1`) became
  `claude-haiku-4-5` and `claude-opus-5-5` in the same change, since they now actually get sent.
- **`/compact` and auto-compact.** `AgentSession::compact(instructions)` asks the model to
  summarize a plain-text transcript of the conversation (no tool blocks, so any provider accepts
  it) and replaces the conversation with that summary. Once a turn's context reaches 150k tokens,
  the next turn compacts first and reports `TurnEvent::Compacted`. A failed or empty summary
  leaves the conversation whole.
- **`/init`.** `init_prompt(root)` is the turn `/init` sends: improve the existing `AGENTS.md`
  or `CLAUDE.md`, or create `AGENTS.md`.
- A ticketless chat turn's context pack is 3k tokens (a ticket's is 8k); every token of it rides
  along on every step of the turn.

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
  - `/tickets` still opens it. (Ctrl+T did too until the chat took it back for the task
    checklist; see "Implemented: chat".)
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
    updated `tui_navigation.rs` (double `←`, `/tickets`, board and detail, Esc chain).

**Known gaps, stated plainly:**
- **`b` collides with typing.** The dispatch input always has focus, so `b` on an empty input
  opens the board. A task typed starting with "b" loses its first letter to the board. `a`/`r`
  have the same collision, but only on a Ready-for-review row. The brief asked for bare-letter
  keys, and Claude Code's agent view avoids them. If this bites, the fix is Ctrl-chords.
- **The peek panel has no reply input.** Claude Code's has one. Answering an escalation means
  attaching.
- **No filters or pinning.** The `a:`/`s:` filters, Ctrl+S grouping by directory, Ctrl+T pin,
  Ctrl+R rename, and Shift+↑/↓ reorder are not implemented. (Ctrl+T's conflict with §1's task
  checklist is settled in "Implemented: chat": the chat owns it.)
- **Summaries are mechanical,** not Haiku-written as Claude Code's are.
- (Settled in "Implemented: chat": the chat's "sessions & tickets" and `/home` wording.)

## Implemented: chat (2026-09-22)

Section 1 is built on the core pieces above. The screen is `crates/tm-tui/src/screens/chat.rs`
(tests in `screens/chat/tests.rs`) with its parts in `crates/tm-tui/src/chat/`; the wiring into
`AgentSession` is `crates/tm-cli/src/tui/chat_ops.rs`, and `tui/steps.rs` turns step records into
transcript entries.

- **Transcript, drawn the way Claude Code draws it** (`chat/transcript.rs`). Every assistant
  message and tool call starts with `⏺` (green, red for a failure, yellow for a denial) and the
  result hangs under `⎿`. Tools show under Claude Code's names (`tool_label`): `shell.run`/
  `test.run`/`build.run` are `Bash(cmd)`, `fs.read` is `Read(path)` → "Read N lines", `fs.list`
  is `List`, `search.*` is `Search("q")` → "Found N results", `edit.apply_patch`/`write_file` are
  `Update(path)` → "Updated path with X additions and Y removals" plus a numbered inline diff
  (removals on a red band, additions on a green one, xterm 52/22 on 256-colour terminals, coloured
  text below that), `edit.create_file` is `Write(path)` → "Wrote N lines to path", `ticket.*` is
  `Ticket`. Commands show their first four lines then `… +N lines (ctrl+o to expand)`; a failure
  shows `Error: Exit code N` and the *last* lines instead, where the cause is. The diff comes from
  the `unified_diff` the edit tools already return (`chat/diff.rs` parses it by hunk counts, so a
  removed line starting with `--` is not a header); `apply_patch`'s per-edit patches are shown in
  order, and an edit that returned `applied: false` is now red, where it used to render as a
  success. A result over `MAX_INLINE_RESULT_BYTES` says where it was stored.
- **Welcome box** (`✻ Welcome to tm!`, `/help`/`/status`, `cwd` with the path cut from the left,
  model) and "Tips for getting started". **Status line:** `? for shortcuts` on the left, replaced
  by `! for bash mode`, the mode indicator (`⏸ plan mode on (shift+tab to cycle)`, `⏵ ask mode
  on`), popup hints or a transient message; model, cwd, ticket, context and token segments on the
  right, which yield first.
- **Input** (`chat/input.rs`, `mention.rs`). `!` on an empty prompt is shell mode (orange box,
  `!` marker; Backspace, Esc or Ctrl+U on empty leaves it; pasting `!cmd` enters it); Enter runs
  `AgentSession::run_shell`, shown as a `! cmd` block with its output. `@` completes file paths,
  fuzzy over a `.gitignore`-honouring walk of the project done on a background thread (state dir
  excluded, 50k files at most); Tab or Enter inserts `@path `. ↑/↓ history, persisted to
  `<state_dir>/prompt-history.jsonl`; Ctrl+R searches it (Ctrl+R/↑ older, Tab/Esc accept, Enter
  send, Ctrl+C cancel). Pastes over 800 characters or 3 lines become `[Pasted text #N +M lines]`,
  deleted as one unit and expanded on send. Readline editing: Ctrl+A/E/B/F, Ctrl+K/U/W kill into
  a ring and Ctrl+Y yanks, Alt+B/F/D on alphanumeric words (Ctrl+W to whitespace, as Claude Code
  does), Ctrl+_ undo (arrives as Ctrl+7), Ctrl+G `$VISUAL`/`$EDITOR`. Shift+Enter, Alt+Enter,
  Ctrl+J and a trailing `\` insert a newline.
- **Turns.** A message sent while a turn (or `!` command) runs is queued, shown dimmed under the
  spinner as `(queued)`, and sent when it ends; consecutive queued messages go as one. ↑ takes
  them back. The spinner reads `✻ Thinking… (12s · 1.2k tokens · esc to interrupt)`. Esc
  interrupts (`TurnInterrupter`; a `!` command's task is aborted), Esc Esc clears the draft into
  history. Ctrl+C closes a dialog, else interrupts a running turn, else clears the prompt; the
  last two arm a second press to quit. Ctrl+D on an empty prompt quits. Shift+Tab cycles
  auto → plan → ask and calls `set_mode` (again at the start of every turn, so a change mid-turn
  is not lost).
- **Permission prompts** (`chat/approval.rs`) replace the input box: "Bash command" / "Edit file",
  the command or path, the reason, "Do you want to proceed?", then `1. Yes`, `2. Yes, and don't
  ask again this session`, `3. No, and tell tm what to do differently (esc)`. Number keys pick,
  ↑/↓ and Enter pick, Esc and Ctrl+C are No, Shift+Tab is 2. No also interrupts the turn so the
  human says what instead. The answer is noted in the transcript where the question came up.
- **Viewer** (`chat/viewer.rs`): Ctrl+O shows every entry with each tool call's real name, full
  input (pretty JSON) and full output, scrollable (↑↓, PgUp/PgDn, g/G, wheel), closed by q, Esc,
  Ctrl+C or Ctrl+O. **`?`** on an empty prompt (and `/help`) shows the shortcuts panel under the
  prompt in up to three columns. **Ctrl+T** toggles a task checklist above the prompt: the
  attached ticket's subtasks and the tickets this conversation sent to `/bg`, with ☐/☒ marks.
- **Commands.** `/help`, `/clear` (`/new`), `/resume` (a picker of saved conversations with first
  message, age and turns), `/compact [focus]` ("Compacting conversation…", then `⎿ Compacted N
  turns`, the summary in the viewer), `/model` (a picker over `model_choices`, current marked;
  `/model <spec>` switches), `/status` (model, provider, directory, scope, mode, ticket, session,
  tokens), `/cost` (tokens by turn), `/init` (`init_prompt` sent as a turn), `/bg [task]`
  ("Moved to the background as T-5"), `/tickets` (`/home`, `/agents`), `/attach`, `/detach`
  (now `detach_ticket`, the conversation carries on), `/decide`, `/exit`. A resumed conversation
  (`tm -c`, `tm -r`, `/resume`) is rendered from its saved turns, `!` commands included.
- **Keys the app root owns.** `←` `←` opens tickets only when `ChatScreen::left_opens_tickets` is
  true (empty prompt, no popup, panel, viewer, picker, search, prompt or shell mode), so the key
  never leaves an overlay. Ctrl+T in the chat is the checklist; from other screens it still
  returns to the chat.
- **Tests.** Unit tests for every rendering and key path above (`tm-tui`), and pty tests through
  the real binary in `crates/tm-cli/tests/tui_chat_parity.rs`: `!` shell and Ctrl+O, `@`
  completion, ↑/Ctrl+R/history surviving a restart, paste collapse, Ctrl+C clear-then-exit,
  Shift+Tab indicator, `?` panel, `/status`, a queued message sent after a `!sleep`, and Esc
  interrupting `!sleep 30`. `cargo run -p tm-tui --example chat_demo` plays every tool shape,
  a diff, a long and a failing command, and a permission prompt.

**Known gaps, stated plainly:**
- **Steps arrive per turn, not live.** `AgentLoop` reports steps when a turn ends or suspends, so
  a long turn shows the spinner and then everything at once. Tool calls cannot show a running
  state until the loop streams them.
- **Esc on a `!` command abandons, it does not kill.** `run_shell` runs on a blocking thread;
  aborting the task frees the session, but the process runs to completion in the background and
  its output is not recorded.
- **Ctrl+G was checked by reasoning, not by a pty test.** The editor runs while the event loop is
  blocked (so crossterm's reader is idle and cannot steal its keystrokes), and a `SIGCONT` to the
  process reuses the runtime's resume path for the full repaint. An editor that forks and returns
  at once (`code` without `--wait`) returns the prompt unchanged.
- **The checklist is tickets, not model-written to-dos.** tm has no to-do tool; a model-maintained
  list needs one in `tm-agent`.
- **No Claude-Code-style mid-prompt `/` completion, `Ctrl+S` stash, image paste, vim mode, or
  rewind (Esc Esc on an empty prompt).**
- `⏺`/`⎿` need a font with U+23FA/U+23BF (every mainstream terminal font's fallback has them;
  the terminal-mcp PNG renderer does not, and draws boxes).

## What this costs, stated plainly

- It's a lot of surface. Several items need new core support: cancelling a turn mid-flight,
  approval prompts answered asynchronously from the UI, persisted and resumable conversations,
  permission modes that change what the agent may do without approval, `/bg`, and an embedded
  scheduler. Each needs its own tests; none can be faked in the UI alone.
- Copying Claude Code's conventions ties us to them. When Claude Code changes a key we either
  follow or diverge knowingly. The docs linked above are the reference to re-check.
- The D-018 home screen is replaced, not kept as an alternative.
