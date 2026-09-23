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

## What this costs, stated plainly

- It's a lot of surface. Several items need new core support: cancelling a turn mid-flight,
  approval prompts answered asynchronously from the UI, persisted and resumable conversations,
  permission modes that change what the agent may do without approval, `/bg`, and an embedded
  scheduler. Each needs its own tests; none can be faked in the UI alone.
- Copying Claude Code's conventions ties us to them. When Claude Code changes a key we either
  follow or diverge knowingly. The docs linked above are the reference to re-check.
- The D-018 home screen is replaced, not kept as an alternative.
