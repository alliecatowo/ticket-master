//! Visualization widgets: the ticket graph, a diff viewer, and a streaming output pane.
//!
//! These are the three D-002 names explicitly as needing "an app-level component and focus layer
//! we build and own": "a ticket graph with navigable nodes, streaming panes, a diff viewer". One
//! file per widget so the implementing agent for this part can work across all three without
//! collisions.

/// A navigable graph of tickets and their dependency edges.
pub mod graph;
/// A unified diff viewer with syntax-aware line styling.
pub mod diff;
/// A live, auto-scrolling pane for an agent/session's streaming output.
pub mod stream;
