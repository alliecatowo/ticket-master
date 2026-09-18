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
/// The home screen: an overview of tickets and active sessions.
pub mod dashboard;
/// The diff viewer screen, browsing changed files and their diffs.
pub mod diff_viewer;
/// A single agent/session's live streaming output.
pub mod session_stream;
/// A single ticket's full detail view.
pub mod ticket_detail;
/// The ticket dependency graph screen.
pub mod ticket_graph;
/// The verification ladder: a ticket's ordered verification steps and their status.
pub mod verification_ladder;
