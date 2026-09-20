//! Screens: the actual `tm` application views, each composed from the widgets in
//! [`crate::widgets_data`] and [`crate::widgets_viz`].
//!
//! A screen is itself a [`crate::component::Component`] — a container that owns child widgets as
//! plain struct fields, computes their sub-`Rect`s in `render` (typically via
//! `crate::theme::split_vertical`/`split_horizontal`), and forwards events to whichever child has
//! its own internal focus in `handle_event`. Screens do not depend on `tm-core`/`tm-events`; they
//! work with `tm-types` identifiers and plain data handed to them by whatever wires this crate up
//! to the rest of the workspace (`tm-cli`), matching this crate's dependency list.
//!
//! One file per screen so the implementing agent for this part can work across all of them
//! without collisions.

/// The command palette overlay: fuzzy-searchable actions and navigation.
pub mod command_palette;
/// The ticket/session overview: a ticket table on the left, a session list on the right.
pub mod dashboard;
/// The diff viewer screen, browsing changed files and their diffs.
pub mod diff_viewer;
/// The default screen bare `tm` opens into: the chat input, the running turn's live output, and
/// the dashboard alongside it.
pub mod home;
/// The Kanban board: tickets as cards in columns named after their real `tm_core::TicketState`,
/// reachable from `home` via `tm-cli`'s own navigation chord (`tm-cli`'s `tui.rs` owns *when* a
/// screen is on screen; this crate only supplies the screen itself, plain-data-in like every
/// other screen here).
pub mod kanban;
/// A single agent/session's live streaming output.
pub mod session_stream;
/// A single ticket's full detail view.
pub mod ticket_detail;
/// The ticket dependency graph screen.
pub mod ticket_graph;
/// The verification ladder: a ticket's ordered verification steps and their status.
pub mod verification_ladder;
