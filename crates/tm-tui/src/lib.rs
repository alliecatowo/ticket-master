//! `tm-tui`: the terminal UI ratatui and crossterm do not give us.
//!
//! # The seam (D-002)
//!
//! `tm` depends on `ratatui`/`ratatui-core` and `crossterm` for the boring, well-tested, sturdy
//! layer: the cell [`Buffer`](ratatui_core::buffer::Buffer), the double-buffer diff, and the
//! backend that writes bytes to a real terminal. We do not fork or vendor ratatui — it is
//! actively maintained and fixing real Unicode bugs upstream, and every limitation we care about
//! sits *above* the buffer layer.
//!
//! Everything a user can see or feel, this crate owns: the component tree, focus, hit testing,
//! event bubbling, layout helpers, theming, animation, and the entire widget set. ratatui's
//! built-in widgets are a reference, not a dependency of our design — we write our own so the
//! result is ours to make beautiful. **They own cells, we own everything above cells.**
//!
//! # Layout of this crate
//!
//! Two files are the finished contract every other module — and every one of the nine parts
//! scaffolded for parallel implementation — codes against:
//!
//! - [`event`] — the [`event::Event`] type (input, resize, tick, application messages) and
//!   [`event::Propagation`], the bubbling result a [`component::Component`] returns.
//! - [`component`] — the [`component::Component`] trait, [`component::ComponentId`], and
//!   [`component::FrameContext`] (theme, capabilities, injected clock, focus state). This file
//!   also holds [`component::FocusTree`], which is a stub: the tree/focus/bubbling *machinery*
//!   built on top of the trait, left for parallel implementation like the modules below.
//!
//! The remaining modules are stubs — type definitions and method signatures with `// IMPL:`
//! comments precise enough to implement against, `todo!()` bodies, one part per implementing
//! agent:
//!
//! - [`runtime`] — terminal lifecycle, the event loop, and the signal handling
//!   (`SIGTERM`/`SIGHUP` restore, `SIGTSTP`/`SIGCONT` suspend/resume) ratatui's panic hook does
//!   not cover.
//! - [`caps`] — terminal capability detection and colour degradation.
//! - [`text`] — grapheme segmentation, display width, and wrapping.
//! - [`theme`] — the palette, layout helpers, and animation.
//! - [`widgets_data`] — tables, lists, trees, and forms.
//! - [`widgets_viz`] — the ticket graph, diff viewer, and streaming output pane.
//! - [`screens`] — the actual `tm` application views, composed from the widget modules above.
//! - [`testing`] — the TUI test harness: an in-memory `TestBackend` harness for `insta`
//!   snapshots, and a PTY harness for the event-loop and signal behaviour above.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod caps;
pub mod component;
pub mod event;
pub mod runtime;
pub mod screens;
pub mod testing;
pub mod text;
pub mod theme;
pub mod widgets_data;
pub mod widgets_viz;
