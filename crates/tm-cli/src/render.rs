//! Human and JSON rendering, shared by every command module.
//!
//! Every subcommand produces a typed payload and hands it to a [`Renderer`], which is the only
//! place that decides *how* that payload reaches the terminal: as one JSON value (stable schema,
//! snapshot-tested) when `--json` is set, or as formatted, optionally-colored text otherwise.
//! `--quiet` suppresses incidental narration but never suppresses the payload itself or an
//! error. Color is disabled by `--no-color` and also auto-disabled whenever stdout is not a
//! terminal, so piping `tm` output never embeds escape codes.
//!
//! [`exit_code`] is the single place a [`tm_types::TmError`] becomes the process exit code
//! documented in `SPEC.md` §15.

use std::io::IsTerminal;

use serde::Serialize;
use tm_types::TmError;

/// How a command's output should be written.
#[derive(Debug, Clone, Copy)]
pub struct Renderer {
    /// Emit JSON instead of human text.
    json: bool,
    /// Suppress incidental narration.
    quiet: bool,
    /// Whether ANSI color may be used (already accounts for `--no-color` and tty detection).
    color: bool,
}

impl Renderer {
    /// Build a renderer from the parsed global flags and stdout's terminal status.
    ///
    /// # IMPL
    /// `color` is `!no_color && stdout_is_terminal`; `stdout_is_terminal` should be
    /// `std::io::stdout().is_terminal()` in `main`, threaded through explicitly so this
    /// constructor stays pure and testable without swapping global stdout.
    pub fn new(json: bool, quiet: bool, no_color: bool, stdout_is_terminal: bool) -> Self {
        Renderer {
            json,
            quiet,
            color: !no_color && stdout_is_terminal,
        }
    }

    /// Convenience constructor that reads stdout's terminal status itself.
    pub fn from_flags(json: bool, quiet: bool, no_color: bool) -> Self {
        Renderer::new(json, quiet, no_color, std::io::stdout().is_terminal())
    }

    /// Whether `--json` output was requested.
    pub fn is_json(&self) -> bool {
        self.json
    }

    /// Whether `--quiet` was requested.
    pub fn is_quiet(&self) -> bool {
        self.quiet
    }

    /// Whether ANSI color may be emitted.
    pub fn color_enabled(&self) -> bool {
        self.color
    }

    /// Emit a fully-formed payload: one JSON value in `--json` mode, or `human` (already
    /// formatted, e.g. by [`Table`] or [`Tree`]) otherwise.
    pub fn emit<T: Serialize>(&self, payload: &T, human: &str) -> tm_types::Result<()> {
        if self.json {
            serde_json::to_writer_pretty(std::io::stdout(), payload)?;
            println!();
        } else {
            println!("{human}");
        }
        Ok(())
    }

    /// Print an incidental status line, suppressed entirely under `--quiet`.
    pub fn note(&self, text: &str) {
        if !self.quiet {
            println!("{text}");
        }
    }

    /// Print an error to stderr, human-formatted or as a JSON error object depending on `--json`.
    pub fn error(&self, err: &TmError) {
        if self.json {
            let kind = match err {
                TmError::NotFound { .. } => "NotFound",
                TmError::Conflict(_) => "Conflict",
                TmError::InvalidTransition(_) => "InvalidTransition",
                TmError::AuthorityDenied(_) => "AuthorityDenied",
                TmError::BudgetExhausted(_) => "BudgetExhausted",
                TmError::LeaseExpired(_) => "LeaseExpired",
                TmError::Storage(_) => "Storage",
                TmError::Provider(_) => "Provider",
                TmError::Io(_) => "Io",
                TmError::Parse(_) => "Parse",
                TmError::Invariant(_) => "Invariant",
            };
            let json = serde_json::json!({
                "error": {
                    "kind": kind,
                    "message": err.to_string(),
                }
            });
            let _ = serde_json::to_writer_pretty(std::io::stderr(), &json);
            eprintln!();
        } else {
            eprintln!("{}", self.apply_color(Color::Red, &format!("error: {err}")));
        }
    }

    /// Wrap `text` in an ANSI color code when `self.color` is set, otherwise return it
    /// unchanged.
    pub fn apply_color(&self, color: Color, text: &str) -> String {
        if self.color {
            format!("\x1b[{}m{text}\x1b[0m", color.code())
        } else {
            text.to_string()
        }
    }
}

/// A small palette, enough to distinguish state/severity in human output without pulling in a
/// terminal-styling dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Errors, denials, invariant violations.
    Red,
    /// Warnings, stale docs, degradations.
    Yellow,
    /// Success, closed tickets, healthy checks.
    Green,
    /// Headings and ids.
    Blue,
    /// De-emphasized text (timestamps, secondary detail).
    Dim,
}

impl Color {
    /// The bare ANSI SGR parameter for this color.
    pub fn code(self) -> u8 {
        match self {
            Color::Red => 31,
            Color::Yellow => 33,
            Color::Green => 32,
            Color::Blue => 34,
            Color::Dim => 2,
        }
    }
}

/// A simple column-aligned table for human output.
#[derive(Debug, Clone)]
pub struct Table {
    /// Column headers.
    pub headers: Vec<String>,
    /// Row data, each inner `Vec` the same length as `headers`.
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// Build a table from headers and rows.
    pub fn new(headers: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        Table { headers, rows }
    }

    /// Render as a column-aligned string, one row per line, columns padded to the widest cell
    /// (header included) plus a two-space gutter.
    pub fn render(&self) -> String {
        if self.headers.is_empty() {
            return String::new();
        }

        // Compute per-column width
        let mut col_widths = vec![0usize; self.headers.len()];

        // Update widths from headers
        for (i, header) in self.headers.iter().enumerate() {
            col_widths[i] = header.len();
        }

        // Update widths from rows
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < col_widths.len() {
                    col_widths[i] = col_widths[i].max(cell.len());
                }
            }
        }

        let mut output = String::new();

        // Render header line
        for (i, header) in self.headers.iter().enumerate() {
            if i > 0 {
                output.push_str("  ");
            }
            output.push_str(&format!("{:<width$}", header, width = col_widths[i]));
        }
        output.push('\n');

        // Render data rows
        for row in &self.rows {
            for (i, &width) in col_widths.iter().enumerate() {
                if i > 0 {
                    output.push_str("  ");
                }
                let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                output.push_str(&format!("{cell:<width$}"));
            }
            output.push('\n');
        }

        // Remove trailing newline
        if output.ends_with('\n') {
            output.pop();
        }
        output
    }
}

/// A simple indented tree for human output (ticket parent/child trees, dependency graphs).
#[derive(Debug, Clone)]
pub struct Tree {
    /// This node's label.
    pub label: String,
    /// This node's children, in display order.
    pub children: Vec<Tree>,
}

impl Tree {
    /// A leaf node.
    pub fn leaf(label: impl Into<String>) -> Self {
        Tree {
            label: label.into(),
            children: Vec::new(),
        }
    }

    /// Render as an indented, ASCII-connector tree (`├──`/`└──`/`│`), the same shape `tree(1)`
    /// or `cargo tree` produce.
    pub fn render(&self) -> String {
        let mut output = String::new();
        self.render_recursive(&mut output, &[], false);
        if output.ends_with('\n') {
            output.pop();
        }
        output
    }

    fn render_recursive(&self, output: &mut String, ancestors: &[bool], is_last: bool) {
        // Add prefix based on ancestor chain
        for (i, &ancestor_is_last) in ancestors.iter().enumerate() {
            if i > 0 {
                output.push_str(if ancestor_is_last { "    " } else { "│   " });
            }
        }

        // Add connector and label
        if !ancestors.is_empty() {
            if is_last {
                output.push_str("└── ");
            } else {
                output.push_str("├── ");
            }
        }
        output.push_str(&self.label);
        output.push('\n');

        // Render children
        let children_count = self.children.len();
        for (idx, child) in self.children.iter().enumerate() {
            let child_is_last = idx + 1 == children_count;
            let mut new_ancestors = ancestors.to_vec();
            new_ancestors.push(is_last);
            child.render_recursive(output, &new_ancestors, child_is_last);
        }
    }
}

/// The process exit code a [`TmError`] maps to, per `SPEC.md` §15: 1 domain failure, 3 authority
/// denied, 4 budget exhausted, 5 invariant violation, and 1 for every other domain error.
/// Usage errors (exit 2) are produced by `clap` itself before any `TmError` exists, so this
/// function is only ever consulted after argument parsing has already succeeded.
pub fn exit_code(err: &TmError) -> i32 {
    err.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_code_red() {
        assert_eq!(Color::Red.code(), 31);
    }

    #[test]
    fn color_code_yellow() {
        assert_eq!(Color::Yellow.code(), 33);
    }

    #[test]
    fn color_code_green() {
        assert_eq!(Color::Green.code(), 32);
    }

    #[test]
    fn color_code_blue() {
        assert_eq!(Color::Blue.code(), 34);
    }

    #[test]
    fn color_code_dim() {
        assert_eq!(Color::Dim.code(), 2);
    }

    #[test]
    fn tree_leaf_has_no_children() {
        let t = Tree::leaf("T-1");
        assert_eq!(t.label, "T-1");
        assert!(t.children.is_empty());
    }

    #[test]
    fn tree_single_leaf_renders() {
        let t = Tree::leaf("root");
        assert_eq!(t.render(), "root");
    }

    #[test]
    fn tree_parent_and_single_child() {
        let mut root = Tree::leaf("root");
        root.children.push(Tree::leaf("child"));
        let rendered = root.render();
        assert_eq!(rendered, "root\n└── child");
    }

    #[test]
    fn tree_parent_with_multiple_children() {
        let mut root = Tree::leaf("root");
        root.children.push(Tree::leaf("child1"));
        root.children.push(Tree::leaf("child2"));
        let rendered = root.render();
        assert!(rendered.contains("├── child1"));
        assert!(rendered.contains("└── child2"));
    }

    #[test]
    fn tree_nested_children() {
        let mut root = Tree::leaf("root");
        let mut child = Tree::leaf("child");
        child.children.push(Tree::leaf("grandchild"));
        root.children.push(child);
        let rendered = root.render();
        assert!(rendered.contains("└── child"));
        assert!(rendered.contains("grandchild"));
    }

    #[test]
    fn renderer_new_computes_color() {
        let r1 = Renderer::new(false, false, false, true);
        assert!(r1.color_enabled());

        let r2 = Renderer::new(false, false, true, true);
        assert!(!r2.color_enabled());

        let r3 = Renderer::new(false, false, false, false);
        assert!(!r3.color_enabled());
    }

    #[test]
    fn renderer_is_json() {
        let r = Renderer::new(true, false, false, true);
        assert!(r.is_json());

        let r2 = Renderer::new(false, false, false, true);
        assert!(!r2.is_json());
    }

    #[test]
    fn renderer_is_quiet() {
        let r = Renderer::new(false, true, false, true);
        assert!(r.is_quiet());

        let r2 = Renderer::new(false, false, false, true);
        assert!(!r2.is_quiet());
    }

    #[test]
    fn apply_color_when_enabled() {
        let r = Renderer::new(false, false, false, true);
        let colored = r.apply_color(Color::Red, "text");
        assert!(colored.contains("\x1b[31m"));
        assert!(colored.contains("\x1b[0m"));
        assert!(colored.contains("text"));
    }

    #[test]
    fn apply_color_when_disabled() {
        let r = Renderer::new(false, false, true, true);
        let colored = r.apply_color(Color::Red, "text");
        assert_eq!(colored, "text");
    }

    #[test]
    fn table_render_empty_headers() {
        let t = Table::new(vec![], vec![]);
        assert_eq!(t.render(), "");
    }

    #[test]
    fn table_render_headers_only() {
        let t = Table::new(vec!["Name".to_string(), "Age".to_string()], vec![]);
        let rendered = t.render();
        assert!(rendered.starts_with("Name"));
        assert!(rendered.contains("Age"));
    }

    #[test]
    fn table_render_with_rows() {
        let t = Table::new(
            vec!["Name".to_string(), "Age".to_string()],
            vec![
                vec!["Alice".to_string(), "30".to_string()],
                vec!["Bob".to_string(), "25".to_string()],
            ],
        );
        let rendered = t.render();
        assert!(rendered.contains("Name"));
        assert!(rendered.contains("Age"));
        assert!(rendered.contains("Alice"));
        assert!(rendered.contains("30"));
        assert!(rendered.contains("Bob"));
        assert!(rendered.contains("25"));
    }

    #[test]
    fn table_render_column_alignment() {
        let t = Table::new(
            vec!["Short".to_string(), "A".to_string()],
            vec![vec!["X".to_string(), "LongValue".to_string()]],
        );
        let rendered = t.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.len() >= 2);
    }

    #[test]
    fn table_render_short_row() {
        let t = Table::new(
            vec!["A".to_string(), "B".to_string(), "C".to_string()],
            vec![vec!["1".to_string(), "2".to_string()]],
        );
        let rendered = t.render();
        assert!(rendered.contains("A"));
        assert!(rendered.contains("B"));
        assert!(rendered.contains("C"));
        assert!(rendered.contains("1"));
        assert!(rendered.contains("2"));
    }

    #[test]
    fn exit_code_authority_denied() {
        let err = TmError::AuthorityDenied("test".to_string());
        assert_eq!(exit_code(&err), 3);
    }

    #[test]
    fn exit_code_budget_exhausted() {
        let err = TmError::BudgetExhausted("test".to_string());
        assert_eq!(exit_code(&err), 4);
    }

    #[test]
    fn exit_code_invariant() {
        let err = TmError::Invariant("test".to_string());
        assert_eq!(exit_code(&err), 5);
    }

    #[test]
    fn exit_code_conflict_returns_one() {
        let err = TmError::Conflict("test".to_string());
        assert_eq!(exit_code(&err), 1);
    }

    #[test]
    fn exit_code_not_found_returns_one() {
        let err = TmError::not_found("ticket", "T-1");
        assert_eq!(exit_code(&err), 1);
    }

    #[test]
    fn exit_code_storage_returns_one() {
        let err = TmError::Storage("test".to_string());
        assert_eq!(exit_code(&err), 1);
    }
}
