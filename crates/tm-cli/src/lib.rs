//! `tm`: the Ticketmaster command-line surface.
//!
//! `tm` is the thing that proves the rest of the workspace actually works, so it is built to
//! full depth: every subcommand `SPEC.md` §15 lists, `--json` everywhere with a stable schema,
//! `--quiet`, `--no-color`, and the exit codes `TmError::exit_code` maps to (0 ok, 1 domain
//! failure, 2 usage error via `clap`, 3 authority denied, 4 budget exhausted, 5 invariant
//! violation). Bare `tm` opens the interactive coding agent (`tm -p <prompt>` is its scriptable
//! form); `tm status` is the "since you left" report.
//!
//! The crate is split so parsing and execution stay independently testable:
//! - [`args`] — the complete `clap` derive command tree. No execution logic lives here.
//! - [`render`] — human/JSON rendering, color, table/tree helpers, and the `TmError`-to-exit-code
//!   mapping.
//! - [`project`] — locating and opening a project; `init`/`attach`/`genesis`/`status`/`doctor`.
//! - [`tickets`] — the `ticket`, `dep`, `milestone`, and `decision` command groups.
//! - [`sched`] — the `sched`, `lease`, and `run` command groups.
//! - [`search`] — the `search`, `symbol`, and `history` command groups over `tm-codeintel`.
//! - [`ops`] — the `docs`, `provider`, `harness`, `bench`, `mirror`, and `events` command groups.
//! - [`drive`] — the `browser` and `computer` command groups over `tm-browser`/`tm-computer`.
//! - [`serve`] — the `serve` command.
//! - [`agent`] — the bare-`tm` interactive coding client and its `tm -p` scriptable form.
//! - [`tui`] — the bare-`tm` ratatui TUI mode (D-002) and the `should_launch` gate that decides
//!   between it and [`agent`]'s plain loop.
//!
//! `main.rs` is deliberately thin: parse, install tracing, open the project when needed, route
//! to one of the modules above, render, exit with the mapped code.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent;
pub mod args;
pub mod drive;
pub mod ops;
pub mod project;
pub mod render;
pub mod sched;
pub mod search;
pub mod serve;
pub mod tickets;
pub mod tui;
