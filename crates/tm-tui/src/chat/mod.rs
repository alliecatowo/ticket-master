//! The building blocks of the chat screen ([`crate::screens::chat::ChatScreen`]): everything a
//! Claude-Code-style conversation view needs that is not the screen's own layout and routing.
//!
//! Like every other module in this crate these are domain-agnostic: a transcript entry is plain
//! text plus a handful of enums, not a `tm_agent::StepRecord`. `tm-cli`'s `tui` module is the one
//! place that translates the agent's real step records into [`transcript::Entry`] values.
//!
//! - [`glyphs`] — the one table of symbols the chat views draw with, with an ASCII fallback
//!   chosen from [`crate::caps::UnicodeSupport`].
//! - [`sanitize`] — strips ANSI escapes and control characters out of text that came from a tool
//!   or a model, before it is ever written into a cell.
//! - [`lines`] — a styled, owned line type plus the helpers that draw one into a buffer.
//! - [`markdown`] — a lightweight Markdown renderer for assistant replies.
//! - [`transcript`] — the conversation model (entries, the live turn, scrolling) and its render,
//!   tool calls drawn the way Claude Code draws them (`⏺ Bash(cmd)`, results under `⎿`).
//! - [`diff`] — unified diffs reduced to numbered lines for an edit's inline diff.
//! - [`viewer`] — the Ctrl+O transcript viewer (every tool call's full input and output).
//! - [`shortcuts`] — the `?` shortcuts panel.
//! - [`mention`] — `@` file-path completion (fuzzy ranking over the project's files).
//! - [`approval`] — Claude Code's numbered permission prompt.
//! - [`picker`] — the `/resume` conversation picker.
//! - [`editor`] — Ctrl+G, editing the prompt in `$VISUAL`/`$EDITOR`.
//! - [`input`] — the multi-line prompt editor with history.
//! - [`commands`] — the slash-command table, its filter, and its parser.
//! - [`status`] — the status bar's segments and their width-aware layout.

pub mod approval;
pub mod commands;
pub mod diff;
pub mod editor;
pub mod glyphs;
pub mod input;
pub mod lines;
pub mod markdown;
pub mod mention;
pub mod picker;
pub mod sanitize;
pub mod shortcuts;
pub mod status;
pub mod transcript;
pub mod viewer;
