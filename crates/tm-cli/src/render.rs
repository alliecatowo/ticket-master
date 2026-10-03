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
use tm_core::milestone::MilestoneState;
use tm_core::ticket::{DependencyKind, TicketKind, TicketState};
use tm_types::{Authority, Budget, TmError};

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
    /// `color` is `!no_color && stdout_is_terminal`; environment policy is resolved by
    /// [`Renderer::from_flags`], keeping this constructor pure and testable.
    pub fn new(json: bool, quiet: bool, no_color: bool, stdout_is_terminal: bool) -> Self {
        Renderer {
            json,
            quiet,
            color: !json && !no_color && stdout_is_terminal,
        }
    }

    /// Convenience constructor that reads stdout's terminal status itself.
    pub fn from_flags(json: bool, quiet: bool, no_color: bool) -> Self {
        let no_color = no_color
            || std::env::var_os("NO_COLOR").is_some()
            || std::env::var("CLICOLOR").is_ok_and(|value| value == "0");
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
            println!("{}", tm_types::sanitize::sanitize(text));
        }
    }

    /// Print a successful human-readable status, highlighted when color is enabled.
    pub fn status(&self, text: &str) {
        if !self.quiet {
            println!(
                "{}",
                self.apply_color(Color::Green, &tm_types::sanitize::sanitize(text))
            );
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
                TmError::TurnFailed(_) => "TurnFailed",
                TmError::CheckFailed(_) => "CheckFailed",
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
            eprintln!(
                "{}",
                self.apply_color(
                    Color::Red,
                    &tm_types::sanitize::sanitize(&format!("error: {err}"))
                )
            );
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
        self.render_with_header_color(false)
    }

    /// Render with a blue header when color is enabled by the caller.
    pub fn render_colored(&self, color: bool) -> String {
        self.render_with_header_color(color)
    }

    fn render_with_header_color(&self, color: bool) -> String {
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

        // The last column is never padded: one long value there (a doctor detail, an objective)
        // would otherwise pad every row out to its width and wrap each one in a real terminal.
        let last = self.headers.len() - 1;

        // Render header line
        for (i, header) in self.headers.iter().enumerate() {
            if i > 0 {
                output.push_str("  ");
            }
            let padded = if i == last {
                header.clone()
            } else {
                format!("{header:<width$}", width = col_widths[i])
            };
            let rendered = if color {
                format!("\x1b[34m{padded}\x1b[0m")
            } else {
                padded
            };
            output.push_str(&rendered);
        }
        output.push('\n');

        // Render data rows
        for row in &self.rows {
            for (i, &width) in col_widths.iter().enumerate() {
                if i > 0 {
                    output.push_str("  ");
                }
                let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                if i == last {
                    output.push_str(cell);
                } else {
                    output.push_str(&format!("{cell:<width$}"));
                }
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

/// The one place a [`TicketState`] becomes the word a person reads (and the word `tm ticket list
/// --state` parses back, via `TicketStateArg` in `args.rs`) — every other call site should call
/// this instead of `{:?}`-formatting the enum. States that
/// `TicketStateArg` doesn't expose as a filter (`Leased`, `Submitted`, `Rework`, `Replan`,
/// `Recovery`, `Escalated`) still get a plain lowercase word, just not one that round-trips
/// through a CLI flag.
pub fn state_label(state: TicketState) -> &'static str {
    match state {
        TicketState::Draft => "draft",
        TicketState::Blocked => "blocked",
        TicketState::Ready => "ready",
        TicketState::Leased => "leased",
        TicketState::Running => "active",
        TicketState::Submitted => "submitted",
        TicketState::Verifying => "verification",
        TicketState::Auditing => "audit",
        TicketState::Rework => "rework",
        TicketState::Replan => "replan",
        TicketState::Recovery => "recovery",
        TicketState::Escalated => "escalated",
        TicketState::Closed => "closed",
        TicketState::Cancelled => "cancelled",
    }
}

/// The word a person reads for a [`TicketKind`].
pub fn kind_label(kind: TicketKind) -> &'static str {
    match kind {
        TicketKind::Work => "work",
        TicketKind::Verification => "verification",
        TicketKind::Audit => "audit",
        TicketKind::Investigation => "investigation",
        TicketKind::Recovery => "recovery",
        TicketKind::Harness => "harness",
    }
}

/// The word a person reads for a [`MilestoneState`].
pub fn milestone_state_label(state: MilestoneState) -> &'static str {
    match state {
        MilestoneState::Open => "open",
        MilestoneState::Closed => "closed",
    }
}

/// The word a person reads for a [`DependencyKind`]: what one ticket depending on another means
/// in practice, not the enum's internal name.
pub fn dep_kind_label(kind: DependencyKind) -> &'static str {
    match kind {
        DependencyKind::Hard => "blocks",
        DependencyKind::Soft => "advisory",
        DependencyKind::Loop => "loop",
    }
}

/// A one-line human summary of a [`Budget`]. `"unlimited"` when every component is `u64::MAX`
/// (see [`Budget::unlimited`]), `"none"` when every component is zero (see [`Budget::none`]),
/// otherwise the populated components joined by `, ` (e.g. `"50k tokens, 300s, $2.00"`). Any
/// individual component that is `u64::MAX` on its own (unlimited tokens but a real dollar cap,
/// say) reads as `"unlimited tokens"` rather than the raw number.
pub fn budget_label(budget: &Budget) -> String {
    if budget.tokens == u64::MAX
        && budget.dollars_micros == u64::MAX
        && budget.wall_seconds == u64::MAX
    {
        return "unlimited".to_string();
    }

    let mut parts = Vec::new();
    if budget.tokens == u64::MAX {
        parts.push("unlimited tokens".to_string());
    } else if budget.tokens > 0 {
        parts.push(if budget.tokens >= 1000 {
            format!("{}k tokens", budget.tokens / 1000)
        } else {
            format!("{} tokens", budget.tokens)
        });
    }
    if budget.wall_seconds == u64::MAX {
        parts.push("unlimited time".to_string());
    } else if budget.wall_seconds > 0 {
        parts.push(format!("{}s", budget.wall_seconds));
    }
    if budget.dollars_micros == u64::MAX {
        parts.push("no spending cap".to_string());
    } else if budget.dollars_micros > 0 {
        parts.push(format!(
            "${:.2}",
            budget.dollars_micros as f64 / 1_000_000.0
        ));
    }

    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}

/// A one-line human summary of an [`Authority`]: full authority, then the powers that most
/// distinguish one grant from another, rather than the whole nested struct.
pub fn authority_label(authority: &Authority) -> String {
    if *authority == Authority::root() {
        return "full authority".to_string();
    }
    if *authority == Authority::none() {
        return "no authority".to_string();
    }

    let mut parts = Vec::new();
    if authority.repository.write == tm_types::PatternSet::all() {
        parts.push("read/write repo".to_string());
    } else if authority.repository.read == tm_types::PatternSet::all() {
        parts.push("read-only repo".to_string());
    } else if authority.repository.read == tm_types::PatternSet::empty() {
        parts.push("no repo access".to_string());
    } else {
        parts.push("scoped repo access".to_string());
    }
    if authority.shell.enabled {
        parts.push("shell".to_string());
    }
    if authority.git.push {
        parts.push("push".to_string());
    }
    if authority.tickets.close {
        parts.push("can close tickets".to_string());
    }
    if authority.network.arbitrary {
        parts.push("arbitrary network".to_string());
    }
    parts.join(", ")
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

    #[test]
    fn state_label_is_lowercase_for_every_state() {
        for state in TicketState::ALL {
            let label = state_label(*state);
            assert_eq!(label, label.to_ascii_lowercase());
            assert!(!label.contains(' '));
        }
    }

    #[test]
    fn state_label_running_matches_the_state_filter_flag() {
        // `tm ticket list --state` (`TicketStateArg` in args.rs) calls the running state
        // "active", not "running" — the label must match what that flag parses.
        assert_eq!(state_label(TicketState::Running), "active");
        assert_eq!(state_label(TicketState::Verifying), "verification");
        assert_eq!(state_label(TicketState::Auditing), "audit");
    }

    #[test]
    fn kind_label_is_lowercase_for_every_kind() {
        for kind in [
            TicketKind::Work,
            TicketKind::Verification,
            TicketKind::Audit,
            TicketKind::Investigation,
            TicketKind::Recovery,
            TicketKind::Harness,
        ] {
            let label = kind_label(kind);
            assert_eq!(label, label.to_ascii_lowercase());
        }
    }

    #[test]
    fn milestone_state_label_matches_lowercase() {
        assert_eq!(milestone_state_label(MilestoneState::Open), "open");
        assert_eq!(milestone_state_label(MilestoneState::Closed), "closed");
    }

    #[test]
    fn dep_kind_label_uses_plain_words() {
        assert_eq!(dep_kind_label(DependencyKind::Hard), "blocks");
        assert_eq!(dep_kind_label(DependencyKind::Soft), "advisory");
        assert_eq!(dep_kind_label(DependencyKind::Loop), "loop");
    }

    #[test]
    fn budget_label_unlimited() {
        assert_eq!(budget_label(&Budget::unlimited()), "unlimited");
    }

    #[test]
    fn budget_label_none() {
        assert_eq!(budget_label(&Budget::none()), "none");
    }

    #[test]
    fn budget_label_renders_populated_components() {
        let budget = Budget {
            tokens: 50_000,
            dollars_micros: 2_000_000,
            wall_seconds: 300,
            spent: Default::default(),
        };
        let label = budget_label(&budget);
        assert_eq!(label, "50k tokens, 300s, $2.00");
    }

    #[test]
    fn budget_label_handles_a_component_that_is_unlimited_on_its_own() {
        // Unlimited tokens with a real dollar cap must not print the raw u64::MAX as a number.
        let budget = Budget {
            tokens: u64::MAX,
            dollars_micros: 2_000_000,
            wall_seconds: 0,
            spent: Default::default(),
        };
        let label = budget_label(&budget);
        assert_eq!(label, "unlimited tokens, $2.00");
    }

    #[test]
    fn authority_label_root_is_full_authority() {
        assert_eq!(authority_label(&Authority::root()), "full authority");
    }

    #[test]
    fn authority_label_none_is_no_authority() {
        assert_eq!(authority_label(&Authority::none()), "no authority");
        assert_eq!(authority_label(&Authority::default()), "no authority");
    }

    #[test]
    fn authority_label_worker_lists_its_powers() {
        let label = authority_label(&Authority::worker());
        assert!(label.contains("read/write repo"));
        assert!(label.contains("shell"));
        assert!(!label.contains("push"));
        assert!(!label.contains("can close tickets"));
    }
}
