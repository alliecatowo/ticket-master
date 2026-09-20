# D-006 — A real navigation shell and Kanban board for the TUI

**Status:** accepted · **Date:** 2026-09-19 · **Supersedes:** nothing (extends D-002)

## Context

D-002 chose ratatui/crossterm and named the cost plainly: "ratatui gives no retained widget tree,
no focus management, no event bubbling and no click hit testing... a ticket graph with navigable
nodes, streaming panes, a diff viewer and a verification ladder needs an app-level component and
focus layer we build and own." `tm-tui` built that layer (`Component`/`ComponentParent`/
`FocusTree`) and, alongside it, a full set of real screens — `ticket_detail.rs`, `ticket_graph.rs`,
`dashboard.rs`, `session_stream.rs`, `verification_ladder.rs`, `diff_viewer.rs`,
`command_palette.rs` — every one tested in isolation.

None of them were reachable. `crates/tm-cli/src/tui.rs`'s `App` wrapped exactly one screen,
`screens::home::Home`, permanently, with no way to leave it. A user-reported complaint made the
gap concrete: pressing left-arrow from chat did nothing like `claude agents`' navigable list, and
there was no literal Kanban board despite `tm_core::ticket::TicketState`'s real 14-state machine
(`Draft`, `Blocked`, `Ready`, `Leased`, `Running`, `Submitted`, `Verifying`, `Auditing`, `Rework`,
`Replan`, `Recovery`, `Escalated`, `Closed`, `Cancelled`) being exactly the kind of structured,
multi-state data a Kanban board is for — a fact D-002's own scaffolding anticipated but nothing
built on top of yet.

## Decision

1. **The router lives in `tm-cli`'s `App`, not in `tm-tui`.** `App` gained a `ScreenId` enum
   (`Home`/`Kanban`/`Detail`) and a `Vec<ScreenId>` back-stack (`App::back_stack`,
   `push_screen`/`pop_screen`). `tm-tui` gained exactly one new screen
   (`screens::kanban::Kanban`) and one new trait impl (`ComponentParent` for the existing
   `TicketDetailScreen`) — no generic router primitive. `App` already depends on `tm_core`/
   `tm_agent` and already owns "which domain data feeds which screen" (`build_dashboard`,
   `refresh`); a router that decides *when* to show a Kanban board built from real ticket state is
   the same kind of domain-aware wiring, not a reusable ratatui-level primitive `tm-tui` has any
   other consumer for yet. A generic `Screen<T>` abstraction in `tm-tui` would need to be
   generic over N unrelated concrete screen types with different constructors and no shared
   trait beyond `Component` itself — Rust does not make that free, and speculatively building it
   for a router with exactly one caller is exactly the kind of premature abstraction this
   workspace's own philosophy (`SPEC.md` §0) argues against.
2. **`Kanban` is one column per real `TicketState` variant, built from `TicketState::ALL` in
   declaration order** (`tui.rs`'s `build_kanban_columns`), not a hand-curated "phase" grouping
   with invented labels. Column count is never hardcoded anywhere in `Kanban` or in
   `build_kanban_columns` — both read it from the collection's own length. Columns scroll
   horizontally (mirroring `Table`'s vertical `scroll_offset`, just on the other axis) and each
   column's cards scroll vertically and independently, so a real 14-state machine never needs
   pagination, relabelling, or squeezing to fit a terminal's width.
3. **Navigation chord is `Ctrl+T`** (`is_tickets_chord`), not a bare letter: `Home`'s chat input is
   always focused and claims every plain keystroke as typed text (this is not new — `q` already
   quits unconditionally regardless of focus, an existing, tested, if slightly surprising,
   convention `tests/tui_turn.rs` already works around). **Back is `Esc` everywhere, and `Left`
   everywhere except `Kanban`**, where `Left`/`Right` already mean "previous/next column" —
   binding `Left` to both would make "leftmost column, press Left again" ambiguous between
   "nothing happens" and "leave the screen". This is a deliberately narrower reading of "Left/Esc
   goes back" for the one screen where `Left` already means something else, not an oversight.
4. **`FocusTree` is rebuilt twice around the event loop, not once before it.**
   `component::FocusTree::rebuild` walks `root.focusable_children()`, and until this change that
   ran exactly once, before the event loop started — meaning a component tree whose
   `focusable_children()` legitimately changes at runtime (exactly what switching screens does)
   would leave `FocusTree`'s tab order pointing at whatever was focusable at startup, forever. This
   was latent and untriggered before this change (nothing before now ever changed what `App`
   considered focusable after `Runtime::start`), and it is a `tm-tui` fix, not a `tm-cli`-local
   workaround, because any future screen that changes its own `focusable_children()` at runtime —
   not just `App`'s top-level screen switch — would hit the identical bug. `Runtime::run` now
   rebuilds once before the loop starts (so the very first dispatched event routes somewhere) and
   once more inside `if needs_redraw` immediately before `self.draw(root)` on every iteration that
   actually paints — not at the top of every iteration unconditionally. Dispatch for a given
   iteration always runs against whatever tree the *previous* iteration's pre-draw rebuild (or the
   initial one) produced, which is correct because that tree reflects the screen active going into
   this iteration's event; the pre-draw rebuild's job is only to make sure the frame about to be
   painted — which may reflect a screen change dispatch itself just made — is not drawn against a
   now-stale tree for one visible frame (see "what this costs" below for the earlier, wrong version
   of this fix and why it was replaced).
5. **`Home`'s default behavior is unchanged.** Bare `tm` still opens directly into the chat screen
   with the input focused, zero prompts — D-002's own "you should be able to just code without
   looking at tickets" holds exactly as before. Kanban/navigation is additive, reachable on demand.

## Why

The alternative to (1) — a generic router in `tm-tui` — was considered and rejected: it would ask
`tm-tui` (explicitly meant to stay domain-agnostic, per its own crate docs) to either grow a
`tm_core` dependency or accept an opaque `Box<dyn Component>` stack with no compile-time guarantee
the caller wired transitions correctly, for a benefit (reuse) that has exactly one consumer today.
The alternative to (2) — grouping states into 3-4 "phase" columns with friendlier names — was
rejected because it invents vocabulary the product does not have (`SPEC.md` §4.3 names 14 states,
not phases) and, per this repo's own documented failure mode (`CLAUDE.md`'s "Parallel tracks
against a moving `main`"), a hardcoded grouping keyed to specific variant names silently drifts
out of sync the next time someone adds a `TicketState` variant, exactly the kind of enum-derived
staleness that section warns about generically.

## What this costs, stated plainly

- `TicketDetailScreen`'s own intra-screen Tab-based pane switching (`Pane::Fields`/
  `Pane::Activity`) still cannot move keyboard focus onto `self.activity`'s `List` specifically —
  nothing in `runtime.rs` calls `FocusTree::focus_next`/`focus_prev` yet, a gap `screens::home`'s
  own docs already called out for `Dashboard`'s two panes before this change. Detail is a
  read-only drill-down for now; this is pre-existing and unrelated to (4) above, not introduced by
  it, and not fixed by it either — the fix here is scoped to "focus follows the currently-reachable
  screen," not "Tab moves focus within a screen."
- The Kanban board's activity feed (ticket detail) is built entirely from fields
  `tm_core::ProjectView` already carries (dependencies, children, a live lease, recorded
  failures), not a dedicated per-ticket event-log query — there is no such query wired up yet. Real
  data, not a placeholder, but a narrower slice of "activity" than a full event history would be.
- Rebuilding `FocusTree` (point 4) resets tab-order position to the first entry each time it runs,
  same as it always implicitly did (nothing ever called `focus_next` to move away from entry 0
  before this change either) — a no-op for existing behavior and exactly the desired behavior for a
  screen that just became reachable, but it does mean intra-screen Tab-order state (once something
  does start calling `focus_next`) will not itself survive a redraw the way a more surgical "only
  rebuild on a reachability change" signal would. An earlier version of this fix rebuilt
  unconditionally at the *top* of every loop iteration instead of before each draw; that was
  simpler but left a real, if purely cosmetic, one-frame bug: dispatch (which can itself change
  `self.current`) ran *after* that rebuild, so the very first frame of a newly-entered screen still
  painted using the *previous* screen's stale `FocusTree`, drawing the new screen's selection in
  its unfocused/dim style until the next tick repainted it (100ms later, per
  `Runtime`'s `tick_interval`) — self-healing, invisible to any test that matches on text rather
  than style, but real. Moving the rebuild to immediately before the draw it actually needs to be
  correct for (see point 4) closes that gap without adding a second unconditional rebuild per
  iteration. The remaining dependency — every reachability-changing dispatch must also set
  `needs_redraw` — is satisfied today (every `tm-cli` navigation transition returns
  `Propagation::Consumed`) but is a real invariant a future screen must keep honoring, not a
  property that holds for free.
