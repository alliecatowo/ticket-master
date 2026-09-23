# D-018 — The TUI is chat-first: a Claude-Code-style chat, with tickets one `←` away

**Status:** accepted · **Date:** 2026-09-22 · **Supersedes:** D-006 points 3, 3b and 5 (the
navigation chords and "`Home` is the default"); D-006's router, back-stack, Kanban board and
`FocusTree` rebuild all stand.

## Context

The owner's ask, verbatim: "tm=claude. tickets and shit is all in the background ... its a coding
tool thats better then codex and claude. same interface basically." And: "when you do 'claude' you
get to normal chat screen ... you do <- in a claude convo and it goes to the cross session 'agents'
view ... the home scren is lack luster."

What bare `tm` actually opened (checked live, screen by screen): a ticket table and a "Sessions"
list that really showed scheduler leases, a stream pane of `  * shell.run -> ok` lines, and a
one-line `tm›` field. No echo of what you typed, no help, no status, no welcome. The command
palette screen existed but was unreachable. And the showstopper: the quit key was a bare `q`,
checked before any focus routing, so typing "quick question" quit the app and lost the message.

D-017 had meanwhile made chat turns ticketless and the core now streams each step live
(`TurnEvent::Steps`, cumulative) with `served_by`, token spend, and inline command output. The
screen was the part still built for the old ticket-per-message model.

## Decision

1. **Bare `tm` opens `tm_tui::screens::chat::ChatScreen`.** Transcript, a bordered prompt box, and
   a one-row status bar; no ticket table. The transcript echoes the human's turns on a raised band,
   renders the model's text as lightweight Markdown (`chat::markdown`: headings, emphasis, inline
   and fenced code on a surface band, lists, quotes, rules, links; source newlines kept), and shows
   each tool call as one scannable line — status glyph, name, the salient argument (the command for
   `shell.run`/`test.run`/`build.run`, the path for `fs.*`/`edit.*`, the quoted query for
   `search.*`), and the outcome (`exit 0`, `3 results`, or the error in the danger colour). A
   command that exits non-zero shows the last three lines of its output under a gutter. Everything
   displayed from a tool or model is sanitized first (`chat::sanitize`: ANSI/OSC escapes, C0/C1
   controls, bidi overrides, `\r` progress redraws).
2. **Typing never triggers a shortcut.** Every printable key is text. The screen-level shortcuts
   are gated on an *empty* prompt: `/` opens the command popup, `?` toggles help, `←` goes to the
   home screen. Globals are modifier chords only: Ctrl+T (home / back to chat) and Ctrl+C. Quit is
   Ctrl+C twice within 1.2s (the first press clears a half-written prompt into history and shows
   "Press Ctrl+C again to quit"), Ctrl+D on an empty prompt, or `/exit`.
3. **The prompt is a real editor** (`chat::input`): multi-line with soft wrap; Enter sends;
   Shift+Enter / Alt+Enter / Ctrl+J (and a trailing `\` then Enter) insert a newline; Up/Down on the
   first/last row walk history (the draft is restored); Ctrl+U/Ctrl+W/Ctrl+A/Ctrl+E behave as in a
   shell. `Runtime` now enables bracketed paste, so a multi-line paste arrives as one
   `InputEvent::Paste` instead of submitting at its first newline. While a turn runs the prompt
   stays editable; Enter is refused with a status-bar hint, and nothing is queued.
4. **One command table drives the popup, help, and the parser** (`chat::commands`): `/help`,
   `/home` (`/agents`, `/tickets`), `/attach <ticket>`, `/detach`, `/decide <text>`, `/clear`,
   `/exit` (`/quit`). The popup ranks name prefix, alias prefix, substring, then subsequence;
   ↑/↓ select, Tab completes, Enter runs (or completes a command that needs an argument), Esc
   dismisses until the query changes. `/usr/bin is broken` is a message, not an unknown command.
   `command_palette.rs` was not reused: it is a centred full-screen fuzzy palette, and this is an
   anchored inline completion menu with argument handling — different shapes.
5. **The status bar is prioritized segments** (`chat::status`): turn state (idle ● / spinner /
   failed ●) and model (the latest `served_by`, else the configured provider — `mock/m1`,
   `devpass/<model>`, or `anthropic/<AGENT_MODEL>`, matching `agent::build_fabric`'s own choice so
   it does not jump after the first turn), cwd `~`-shortened with the git branch read from
   `.git/HEAD`, `no ticket` or the attached id, the open-ticket count, and session tokens. When
   narrow, segments drop lowest-priority first and the cwd truncates before it disappears; a
   right-aligned hint yields unless it is urgent.
6. **Home is the cross-session view** (`screens::home::Home`, the `claude agents` analogue):
   sessions (this conversation as a selectable row — Enter on it returns to the chat — plus earlier
   ones), background work grouped attention → active → queued → done with the raw `TicketState` as
   a coloured badge (finished ones collapse past three), and active workers (live leases, labelled
   as workers, not sessions). Enter on a ticket opens the existing detail screen; `b` opens the
   existing Kanban board; Esc/→ return to the chat. Empty states say what to do next.
   Past sessions come from one grouped read-only query over the event log's `session` column
   (each turn brackets itself with `session.started`, so the count of those is the turn count).
   They are history only: transcripts are not persisted, so there is no resume, and none is faked.
7. **Domain stays out of `tm-tui`.** `tm-cli`'s `tui/steps.rs` is the only code that reads a
   `StepRecord`; it hands the chat plain `Entry`/`ToolCallView` values through a new
   `AppMessage::Turn { session, update }`. Updates for a session the chat no longer shows (after
   `/clear`) are dropped.

## Why

Chat-first is the product: the owner wants tm to feel like Claude Code/Codex, with tickets as
background machinery. Gating the shortcuts on an empty prompt (rather than a mode, or a leader
key) is what Claude Code and Codex converge on because it keeps discoverability without ever
stealing a keystroke from a message. The prioritized status bar was chosen over a fixed layout
because 80 columns is a real width and a clipped `devpass/muse-spark-1.3-contr` reads as broken.

## What this costs, stated plainly

- **Activity is coarse.** Steps arrive after their tool calls finish, so the spinner says
  "Thinking" rather than "running shell.run…"; naming the in-flight tool needs a pre-execution
  hook in `AgentLoop`.
- **`/detach` starts a fresh conversation.** `AgentSession` has `attach_ticket` but no way to
  clear the attachment, and this change deliberately did not edit `agent.rs`, so detaching swaps
  in a new session (the notice says so). A `detach_ticket` on `AgentSession` would fix it.
- **Approval is still auto-denied in the TUI**, now with an explicit transcript notice pointing at
  `tm --plain`. An in-TUI approval prompt is the next piece of work.
- **No interrupt.** Esc does not cancel a running turn; aborting the task mid-step would skip the
  pty/browser teardown `run_turn_streaming` does. Quitting still works mid-turn.
- **Mouse capture stays on** (inherited from D-002's runtime): the wheel scrolls the transcript,
  but selecting text to copy needs the terminal's bypass modifier (Shift/Option).
- The tool-call and Markdown rendering are covered by render tests and by
  `cargo run -p tm-tui --example chat_demo` (a scripted turn through the real runtime), not by the
  pty suite: the mock provider only ever replies with text.
