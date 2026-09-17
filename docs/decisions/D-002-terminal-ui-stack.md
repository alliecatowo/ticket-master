# D-002 — The terminal UI stack

**Status:** accepted · **Date:** 2026-09-16 · **Supersedes:** nothing

## Context

`tm` is the flagship surface and the bar is that it beats Claude Code, Codex, opencode and pi while
being sturdy enough that it never tears a layout or leaves a wedged tty. Until now no stack had been
chosen at all: `tm-cli` was `clap` plus a `Renderer` that prints. SPEC §27 listed Ratatui only as a
*template* we offer users, never as our own choice. The alternative raised was a TypeScript/Ink
front end over the Rust core.

## Decision

**ratatui + crossterm, in-process in `tm-cli`.** The TUI is a mode of the existing binary, entered on
the bare-`tm` TTY path; every `clap` subcommand and `--json` keeps working untouched.

## Why

The decisive evidence is what the comparison set actually did, not what is pleasant to build:

- **Codex was TypeScript/Ink and was rewritten to Rust + ratatui** (`codex-tui`). The team whose
  ambition most closely matches ours moved *off* Ink.
- **opencode was Go + Bubble Tea and moved off that too** — but to OpenTUI, an in-house renderer with
  a native Zig core, having explicitly evaluated and declined Ink.
- **Nobody moved toward Ink.** Claude Code and gemini-cli use it because Node was already their
  runtime, not because it was chosen for the terminal.
- Ink's own ecosystem answer to streaming-output performance is a separate non-Ink renderer with
  double-buffering and damage-tracked diffing — i.e. reimplementing what ratatui already does.

Beyond the field evidence: ratatui keeps the single static binary, which matters for a tool that must
be installable on a box you just attached to (see the remote-control backlog item), and lets the TUI
call `tm-core`/`tm-events` in process instead of round-tripping HTTP to a sibling Node process.
`insta` and `assert_cmd` are already workspace dependencies, so the testing story costs nothing new.

Immediate mode is not the streaming risk it appears to be: application code redraws from state every
frame, but ratatui diffs buffers and writes only changed cells, so the wire stays cheap.

## What this costs, stated plainly

ratatui gives no retained widget tree, no focus management, no event bubbling and no click hit
testing. A ticket graph with navigable nodes, streaming panes, a diff viewer and a verification
ladder needs an app-level component and focus layer we build and own. That is the strongest argument
for Ink and it is a real one; we are choosing sturdiness and distribution over a free component model.

## Consequences

These are **not** free with ratatui/crossterm and are therefore explicit requirements, each with a
test:

- `SIGTERM`/`SIGHUP` restore. ratatui's panic hook covers `panic!` only; a killed process or a closed
  SSH session bypasses unwinding and leaves the tty in raw/alt-screen mode. Needs a signal handler,
  not just the hook.
- `SIGTSTP`/`SIGCONT` suspend and resume is entirely ours.
- Truecolor detection over SSH and tmux, where `COLORTERM` is frequently not forwarded. Codex has an
  open bug for exactly this; we should not inherit it.
- Synchronized output (DEC 2026) available via crossterm but verify it is actually enabled.
- Grapheme widths: `unicode-width`'s narrow/wide model still mishandles emoji with variation
  selectors, and upstream is mid-fix. Needs a cross-emulator test matrix rather than trust.

Testing is layered: `TestBackend` + `insta` snapshots per screen, a PTY harness for the event loop
and the signal behaviour above, and VHS tapes as visual-regression gates on the highest-value screens.

A future web client consumes the same resumable SSE stream rather than sharing render code; if literal
sharing is ever wanted, `ratzilla` (ratatui to WASM) is a smaller lift than parallel renderers. We do
not build a shared view-model crate until a second front end actually exists.
