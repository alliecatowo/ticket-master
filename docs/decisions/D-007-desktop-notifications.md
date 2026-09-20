# D-007 — Desktop notifications on `approval.requested`/`ticket.escalated`

**Status:** accepted · **Date:** 2026-09-19 · **Supersedes:** nothing

## Context

`docs/audit-2026-09-18-fable.md`'s M-04 ("Ecosystem table stakes") flagged desktop notifications
as the single clearest gap in the whole competitive survey: none of Claude Code, Codex, or
gemini-cli ship native desktop notifications despite it being the most-requested signal across all
three, to the point that each spawned near-identical community bridge projects
(`terminal-notifier`, `ntfy`, OSC-9 capture) just to bolt it on externally. `docs/backlog.md`
independently flags the same gap. The audit's "done looks like": desktop notifications
(`notify-rust`/`terminal-notifier`, OSC-9 fallback) on `approval.requested`/`ticket.escalated`.

Two things had to be settled before writing any code:

1. **Does `notify-rust` already cover macOS well enough to drop `terminal-notifier` entirely?**
   `notify-rust` does build and run on macOS (via `mac-notification-sys`, using
   `NSUserNotification`/`UNUserNotificationCenter`), so the audit's phrasing could plausibly have
   been overcautious. In practice it is not: those macOS notification APIs require the calling
   process to carry a real `.app` bundle identity to reliably show a banner, which a bare
   `cargo build`-produced CLI binary (`tm`'s own shape) does not have. `terminal-notifier` exists
   specifically because it *is* a tiny signed `.app`, invoked as a CLI wrapper around that bundle,
   which is exactly the gap an unbundled Rust binary falls into. So the audit's three-tier chain is
   kept as specified, not simplified away.
2. **Where does the observation have to live?** `approval.requested` is appended by `tm-agent`'s
   loop (`crates/tm-agent/src/agent_loop.rs`) when a tool call needs approval mid-run; `tm-agent`
   itself runs as a background task spawned by `tm-scheduler`'s dispatcher
   (`crates/tm-scheduler/src/dispatch.rs`'s `run_and_report`, spawned from `ExecutorDispatcher::
   dispatch`'s synchronous `Lease` handling). `ticket.escalated` is appended synchronously from
   `tm-scheduler`'s own `SchedulerLoop::apply_action` (`SchedulerAction::Escalate`). Both happen
   *inside the same OS process* as `tm sched run`/`tm run` (the dispatcher spawns onto that
   process's own `tokio::runtime::Handle`), but neither is guaranteed to show up in
   `SchedulerLoop::tick`'s own return value — the agent-loop append happens several async frames
   away from any tick's synchronous return. So "read `tick()`'s output" is not a reliable
   observation point even within the same process.

## Decision

A new `tm-notify` crate, split the way `SPEC.md` §0 asks anything real-I/O-shaped to be split:

- **`tm_notify::decision`** — pure, fully unit-tested functions: `should_notify(EventKind) -> bool`
  (`ApprovalRequested`/`TicketEscalated`, closed and exhaustive-by-intent), `format_notification
  (&Event) -> Option<Notification>` (renders a real title/body from the event's own payload — the
  ticket id and the approval's `note`/the escalation's `reason` — not a generic "something
  happened"), and `notifications_enabled(Option<&str>) -> bool`, the opt-out gate.
- **`tm_notify::notifier`** — the `Notifier` trait every test in the crate dispatches through via
  an in-memory recording fake, plus `SystemNotifier`, the one real implementation: `notify-rust` on
  Linux/Windows, a `terminal-notifier` shell-out on macOS when it is on `PATH`, and an OSC 9
  escape sequence (`\x1b]9;<message>\x1b\\`) to the controlling terminal (`/dev/tty` on Unix,
  `CONOUT$` on Windows, stderr as a last resort) whenever the platform-appropriate mechanism is
  missing or fails. This exact chain, unmodified from the audit's phrasing — see Context above for
  why it is not simplified. `SystemNotifier::notify` is deliberately outside this crate's test
  suite: it is real OS/process/terminal I/O, the same category
  `crates/xtask/src/hygiene.rs`'s `check_network_in_tests`-style checks exist to keep out of
  `cargo test`, and is marked as such in its own module docs.
- **`tm_notify::watch::spawn_notification_watcher`** — opens a *second*, read-only
  `tm_events::EventLog` on the project's own `project.db` and polls `read_from`/`head` on an
  interval, exactly the pattern `tm-server`'s `AppState::spawn_broadcast_poller`
  (`crates/tm-server/src/state.rs`) already uses for the identical reason: `tm_core::Store` does
  not expose its internal `EventLog` or a subscribe hook, and `tm_events::stream::EventHub` only
  fans events out to subscribers of the *same* `EventLog` instance that appended them — a second,
  freshly opened handle on the same WAL-mode SQLite file sees committed writes fine for reads, just
  not through its own (empty) hub. This is also independently the right seam here: since neither
  notification-worthy event is guaranteed to surface through any single synchronous return value
  in-process (see Context), polling the log itself is the one place that sees everything, without
  needing a second `tm` invocation to observe a different one's writes.

Wired into `tm-cli`'s two long-running/interactive surfaces that can produce either event kind
from within their own process — `tm sched run` (`sched_run`) and `tm run <ticket>` (`run_ticket`),
both in `crates/tm-cli/src/sched.rs` — via a shared `start_notification_watcher(&Project)` helper
called at the top of each, fire-and-forget (the `JoinHandle` is discarded, matching `tm-server`'s
own poller convention: it runs for the process's lifetime, never explicitly joined or stopped).

**Opt-out:** the `TM_NOTIFY` environment variable, following this codebase's own `TM_`-prefixed
convention (`TM_ACTOR`, `TM_HOME`, `TM_SERVER_TOKEN`, `TM_COMPUTER_BACKEND`) rather than a bare
`NO_NOTIFY`. Unset, empty, or any value other than a recognized falsy spelling (`0`, `false`,
`off`, `no`, case-insensitive) leaves notifications on by default; setting one of those disables
the watcher entirely before it ever opens a second `EventLog` handle — the intended path for
headless/CI/server contexts where a notification call is pointless or, absent a controlling
terminal, could turn the OSC 9 fallback into a stderr-spam no-op instead of a clean skip.

## Why

- The audit's fallback chain is not arbitrary; each tier covers a real gap the tier above it
  leaves open (see Context's `notify-rust`-on-macOS investigation), so it is kept exactly as
  specified rather than invented anew or prematurely simplified.
- Following `tm-server`'s already-established "second read-only `EventLog`, polled" pattern rather
  than adding a new observation primitive to `tm-core` keeps this additive: zero changes to
  `Store`, `EventLog`'s public API, or any existing call site that appends `approval.requested`/
  `ticket.escalated`.
- Splitting pure decision logic from the real I/O edge is the same shape as
  `tm-scheduler`'s own split (`plan()` pure, `SchedulerLoop` the thin I/O-touching shell) — it
  makes the interesting behavior (which events, what the message says, the opt-out) fully unit
  tested, and keeps the untestable-by-necessity part (`SystemNotifier`) small and clearly marked
  rather than smeared across the crate.

## What this costs, stated plainly

- **A poll, not a push.** `DEFAULT_POLL_INTERVAL` is 500ms; a notification lands up to half a
  second after the event that triggered it, not instantly. Acceptable for "tell a human something
  needs them," not acceptable if a future caller wanted this for tight-loop automation.
- **One extra SQLite read connection per poll tick**, for the lifetime of `tm sched run`/`tm run`.
  Same cost `tm-server`'s poller already accepts; not new to this codebase, just newly paid by the
  CLI too.
- **`terminal-notifier` is an unmanaged, optional external dependency** on macOS: if it is not
  installed, macOS silently falls through to the OSC 9 fallback rather than erroring, which means
  a macOS user who wants a real banner (not just an escape sequence their terminal emulator may or
  may not render as one) has to `brew install terminal-notifier` themselves — this crate does not
  manage or bundle it, unlike `tm-browser`'s managed-Chromium story.
- **Only two event kinds fire, on purpose.** `should_notify` is a closed match, not a default-on
  catch-all — extending it to a third kind (e.g. `provider.exhausted`) is a deliberate future
  change to that one function, not something that falls out of adding an `EventKind` variant
  elsewhere.
- **`tm run <ticket>`'s watcher can lose the exact notification it exists for.** `run_ticket`
  itself polls the ticket's state every 500ms and returns as soon as it leaves `Leased`/`Running`
  (`RUN_TICKET_POLL_INTERVAL`) — and `approval.requested` is precisely the event that causes that:
  the agent loop appends it, returns `AwaitingApproval`, the ticket leaves `Running`, and
  `run_ticket` notices and returns, ending the process (and, with it, the spawned watcher task)
  before its own next poll is guaranteed to have run. The watcher reads-then-sleeps (matching
  `tm-server`'s poller order) specifically to narrow this window, but does not eliminate it — two
  same-cadence polls racing the same event is not a fix, just a smaller race. `tm sched run`, the
  surface the audit actually names, does not have this problem: it is long-lived, so there is no
  "the process exits out from under the watcher" failure mode. A future fix, if `tm run`'s case
  turns out to matter in practice, is having `run_ticket` do one final synchronous drain of the
  watcher's log position before it returns, rather than trusting the background poll to have
  already caught up.
- **No per-notification rate limiting or coalescing.** A ticket that escalates repeatedly (e.g.
  flapping between `Recovery` and `Escalated` across several scheduler ticks) fires one
  notification per event, not a de-duplicated summary. Acceptable at today's volumes; would need
  revisiting if a project's escalation rate ever became genuinely noisy.
