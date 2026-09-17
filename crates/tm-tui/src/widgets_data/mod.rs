//! Data widgets: tables, lists, trees, and forms.
//!
//! ratatui ships widgets shaped like these as a *reference*, not a dependency of this design
//! (D-002): each widget here owns its own interaction state (selection, scroll offset, expansion)
//! as a [`crate::component::Component`] — implementing `render` and `handle_event` itself — rather
//! than the immediate-mode "render given external state, mutate that state yourself" pattern
//! ratatui's built-ins use. That is what lets a table or a tree sit in the focus tree and react to
//! its own keybindings without a screen re-deriving that logic per widget.
//!
//! One file per widget so the implementing agent for this part can work across all four without
//! any of them colliding on the same file.

/// A scrollable, sortable table.
pub mod table;
/// A scrollable, single-select list.
pub mod list;
/// An expandable/collapsible tree (e.g. the ticket dependency graph's list view).
pub mod tree;
/// A vertical form of labelled fields with tab-order navigation between them.
pub mod form;
