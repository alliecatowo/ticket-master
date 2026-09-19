//! `WorkflowDef`: the parsed and validated contents of one workflow definition `.toml` file.
//!
//! One file is one workflow (`.tm/workflows/<name>.toml`, by this crate's convention), unlike
//! `SPEC.md` §25.2's illustrative `[workflow.review-change]`-wrapped snippet, which shows several
//! definitions nested in one file. That nesting buys nothing once a definition is its own
//! versioned, content-hashed unit (`tm-core`'s `workflows` table, keyed by whole-file content
//! hash) discovered by filename (`tm workflow list`) -- so this crate's wire shape is flat, with
//! `name` as an explicit top-level field instead of a table key, following the same
//! "adapt the illustrative shape to this crate's own conventions" latitude `mirror.toml`
//! (`crates/tm-mirror/src/config.rs`) and `browser.toml` (`crates/tm-browser/src/config.rs`) take
//! with *their* `SPEC.md` sections: parse-then-validate, `TmError::parse`/`TmError::invariant`
//! for the two failure classes, doc comments on every field.
//!
//! `role`'s wire form is `tm_types::Role`'s own `#[serde(rename_all = "snake_case")]` derive
//! (e.g. `"reviewer_semantic"`), not §25.2's dotted illustrative `"reviewer.semantic"` -- `Role`
//! is reused as-is rather than given a second, DSL-specific parser (`Role::from_str` accepts
//! both forms, but plain derived (De)serialize does not, and duplicating `FromStr`'s normalization
//! into a custom `Deserialize` impl for one field would be more code than the underscored form
//! costs in readability).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tm_core::ticket::{CycleBudget, VerificationPolicy};
use tm_types::{Budget, Result as TmResult, Role, TmError};

/// A node's budget, in TOML. `tm_types::Budget` itself requires every one of `tokens`/
/// `dollars_micros`/`wall_seconds` on deserialize (only `spent` has `#[serde(default)]`, since a
/// budget silently defaulting to unlimited would be a real hazard for `tm-core`'s own callers --
/// see that type's doc comment); a workflow definition author naming just the one limit that
/// matters for a node (as `SPEC.md` §25.2's own `budget = { tokens = 20000 }` does) is a
/// reasonable, lower-stakes convenience, so this crate defines its own all-defaulted TOML shape
/// and converts explicitly via [`BudgetSpec::to_budget`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetSpec {
    /// Token allowance. Defaults to `0` (no tokens) when omitted, matching `Budget::default`'s
    /// own "absence means zero, unlimited must be asked for" convention.
    #[serde(default)]
    pub tokens: u64,
    /// Money allowance in millionths of a dollar.
    #[serde(default)]
    pub dollars_micros: u64,
    /// Wall-clock allowance in seconds.
    #[serde(default)]
    pub wall_seconds: u64,
}

impl BudgetSpec {
    /// Convert to the real `tm_types::Budget` an expanded ticket carries, with `spent` at zero.
    pub fn to_budget(self) -> Budget {
        Budget::new(self.tokens, self.dollars_micros, self.wall_seconds)
    }
}

/// The declared type of a `[params.*]` entry. Informational only today (every parameter value is
/// carried as a `String` end to end -- `tm workflow run --param k=v`, `{{param}}` substitution,
/// and `WorkflowDef::params`' `default` are all strings); `kind` exists so a definition documents
/// its own contract and a future CLI can validate/coerce `--param` values against it without a
/// wire-shape change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    /// Free text.
    #[default]
    String,
    /// A number, still carried as a string.
    Number,
    /// `"true"`/`"false"`, still carried as a string.
    Bool,
}

/// One `[params.<name>]` entry: this workflow's declared parameter contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamDef {
    /// The parameter's declared type (informational; see [`ParamKind`]).
    #[serde(default)]
    pub kind: ParamKind,
    /// The value used when `tm workflow run` does not supply this parameter. `None` means the
    /// parameter is required.
    #[serde(default)]
    pub default: Option<String>,
    /// Human-readable note on what this parameter controls, surfaced by `tm workflow show`.
    #[serde(default)]
    pub description: String,
}

/// A node's fan-out: how many tickets it expands into, and over what.
///
/// TOML-untagged: `for_each = ["a", "b", "c"]` (a literal list, `SPEC.md` §25.2's `dimension`
/// node) deserializes as [`ForEach::Static`]; `for_each = "dimension.findings"` (a dotted
/// `node_id.field` reference to another node's [`NodeDef::produces`], §25.2's `verify` node)
/// deserializes as [`ForEach::FromOutput`]. A node with no `for_each` key at all runs once (see
/// [`NodeDef::for_each`], which wraps this in an `Option`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ForEach {
    /// A fixed, definition-time-known list of fan-out items.
    Static(Vec<String>),
    /// `"<node_id>.<field>"`: fan out over the named upstream node's realized output field, one
    /// ticket per element, once that node's result is known. See `crate::expand`'s module doc for
    /// how this is actually staged across two expansion passes -- the one genuinely novel piece
    /// of this DSL, since at definition-expand time the upstream node has not run and the
    /// cardinality of its output is not yet known.
    FromOutput(String),
}

impl ForEach {
    /// The `(node_id, field)` named by a [`ForEach::FromOutput`] reference, splitting on the
    /// first `.`. `None` for [`ForEach::Static`], or for a [`ForEach::FromOutput`] string with no
    /// `.` in it (caught by [`WorkflowDef::validate`], not by this accessor).
    pub fn from_output_parts(&self) -> Option<(&str, &str)> {
        match self {
            ForEach::FromOutput(s) => s.split_once('.'),
            ForEach::Static(_) => None,
        }
    }
}

/// `join.kind`: how a node with more than one dependency (typically a fan-out's downstream
/// synthesis step) waits on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinKind {
    /// Wait for every dependency.
    All,
    /// Proceed once any one dependency completes.
    Any,
}

/// `join.merge`: how fanned-out results are combined back into the joining node's input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// Concatenate every dependency's result, in dependency-declaration order.
    #[default]
    Concat,
    /// Take only the first dependency's result (by declaration order for `all`, by completion
    /// order for `any`).
    First,
}

/// One `[[node]].join` table: `SPEC.md` §25.2's "every join declares expected inputs, timeout and
/// merge rule".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinDef {
    /// `all` or `any`.
    pub kind: JoinKind,
    /// How long to wait for the join to become satisfiable before the joining ticket's own
    /// lease/verification timeout machinery treats it as failed. Plain data (a `u64`), not a
    /// `tm_types::Timestamp` -- no clock is read to compute it, only carried as configuration
    /// (the workspace's non-deterministic-time hygiene rule concerns reading a clock outside
    /// `tm-types`, not carrying a duration).
    #[serde(default = "default_join_timeout_seconds")]
    pub timeout_seconds: u64,
    /// How to combine the joined results.
    #[serde(default)]
    pub merge: MergeStrategy,
}

fn default_join_timeout_seconds() -> u64 {
    3600
}

/// One `[[node]]` table: one unit of work in the workflow, expanding to one or more tickets (see
/// [`ForEach`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeDef {
    /// This node's id, unique within the definition. Referenced by other nodes' `depends` and
    /// `for_each: FromOutput("<id>.<field>")`.
    pub id: String,
    /// The executor role this node's expanded ticket(s) request.
    pub role: Role,
    /// A template string with `{{param}}` substitution (plus `{{item}}`/`{{item.field}}` inside
    /// a fanned-out node -- see `crate::template`), rendered into each expanded ticket's
    /// objective.
    pub objective: String,
    /// This node's fan-out, if any. Absent means the node expands to exactly one ticket.
    #[serde(default)]
    pub for_each: Option<ForEach>,
    /// The field name this node's result is available under to a downstream node's
    /// `for_each = "this_node_id.<produces>"`. Required for a node another node's `for_each`
    /// names; [`WorkflowDef::validate`] rejects a `for_each: FromOutput` reference whose field
    /// does not match any upstream node's declared `produces` at definition-validate time, per
    /// `SPEC.md` §25.2's "rejected at compile time, not at runtime".
    #[serde(default)]
    pub produces: Option<String>,
    /// Node ids this node's ticket(s) depend on (must close first).
    #[serde(default)]
    pub depends: Vec<String>,
    /// How this node waits on more than one dependency. `SPEC.md` §25.2 requires "every join
    /// declares expected inputs, timeout and merge rule"; `depends` is the expected-inputs list,
    /// so a `join` only needs its own `kind`/`timeout`/`merge`.
    #[serde(default)]
    pub join: Option<JoinDef>,
    /// This node's ticket budget.
    pub budget: BudgetSpec,
    /// This node's verification policy.
    pub verification: VerificationPolicy,
    /// Bounded-iteration budget for a node whose own ticket may need to retry/cycle
    /// (`crates/tm-core/src/ticket.rs:254`). `SPEC.md` §25.2: "loops must carry a `CycleBudget`;
    /// a cycle without one is rejected at compile time, not at runtime" -- see
    /// [`WorkflowDef::validate`]'s cycle check for the compile-time half of that rule.
    #[serde(rename = "loop", default)]
    pub cycle: Option<CycleBudget>,
}

/// The parsed and validated contents of one workflow definition `.toml` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDef {
    /// The workflow's name, e.g. `"review-change"`. By this crate's convention this also names
    /// the file it's discovered from (`.tm/workflows/<name>.toml`), though [`WorkflowDef::parse`]
    /// itself does not check that -- discovery is `tm-cli`'s concern.
    pub name: String,
    /// Named parameters this workflow is invoked with (`tm workflow run <name> --param k=v`).
    #[serde(default)]
    pub params: BTreeMap<String, ParamDef>,
    /// Every node, in declaration order. `#[serde(default)]` so a definition with no `[[node]]`
    /// tables at all parses (as an empty list) rather than failing deserialization outright,
    /// letting [`WorkflowDef::validate`]'s own "has no nodes" check produce the actionable error
    /// instead of a generic missing-field one.
    #[serde(rename = "node", default)]
    pub nodes: Vec<NodeDef>,
}

impl WorkflowDef {
    /// Parse and validate a workflow definition's TOML source.
    pub fn parse(source: &str) -> TmResult<Self> {
        let def: WorkflowDef =
            toml::from_str(source).map_err(|e| TmError::parse(format!("workflow: {e}")))?;
        def.validate()?;
        Ok(def)
    }

    /// Structural validation beyond what serde already enforces:
    ///
    /// - `nodes` is non-empty and every `id` is unique.
    /// - every `depends` entry names a declared node id (no dangling ref, no self-dependency).
    /// - every `for_each: FromOutput("<id>.<field>")` names a node declared *earlier* in the
    ///   file (fanning out over a node's result requires that result to already exist, which
    ///   `crate::expand` only guarantees for already-processed nodes -- see that module's doc)
    ///   whose `produces` equals `<field>` exactly.
    /// - the `depends` graph's cycles (`SPEC.md` §25.2) each have every member node carrying a
    ///   `loop` (`CycleBudget`); a cycle with any member lacking one is rejected here, at
    ///   validate time, rather than surfacing only once `tm_core::check_invariants` rejects the
    ///   committed graph.
    pub fn validate(&self) -> TmResult<()> {
        if self.nodes.is_empty() {
            return Err(TmError::invariant(format!(
                "workflow {:?}: has no nodes",
                self.name
            )));
        }

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for node in &self.nodes {
            if node.id.is_empty() || node.id.contains(['[', ']', '.']) {
                return Err(TmError::invariant(format!(
                    "workflow {:?}: node id {:?} must be non-empty and must not contain '[', ']' or '.' \
                     (those are reserved: '[' for fan-out ticket-ref indices, '.' for for_each's \
                     node.field references)",
                    self.name, node.id
                )));
            }
            if !seen.insert(node.id.as_str()) {
                return Err(TmError::invariant(format!(
                    "workflow {:?}: duplicate node id {:?}",
                    self.name, node.id
                )));
            }
        }

        let index_of: BTreeMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();

        for (i, node) in self.nodes.iter().enumerate() {
            if node.depends.iter().any(|d| d == &node.id) {
                return Err(TmError::invariant(format!(
                    "workflow {:?}: node {:?} depends on itself",
                    self.name, node.id
                )));
            }
            for dep in &node.depends {
                if !index_of.contains_key(dep.as_str()) {
                    return Err(TmError::invariant(format!(
                        "workflow {:?}: node {:?} depends on unknown node {:?}",
                        self.name, node.id, dep
                    )));
                }
            }
            if let Some(for_each) = &node.for_each {
                if let ForEach::Static(items) = for_each {
                    if items.is_empty() {
                        return Err(TmError::invariant(format!(
                            "workflow {:?}: node {:?}'s for_each is an empty list; an empty \
                             Static fan-out silently produces zero tickets for this node, \
                             leaving every node that `depends` on it unblocked as if it had \
                             already run -- write `for_each = [\"...\"]` with at least one item, \
                             or remove `for_each` for a node that runs once",
                            self.name, node.id
                        )));
                    }
                }
                if let ForEach::FromOutput(raw) = for_each {
                    let Some((source_id, field)) = for_each.from_output_parts() else {
                        return Err(TmError::invariant(format!(
                            "workflow {:?}: node {:?} has a malformed for_each {:?}; expected \"<node_id>.<field>\"",
                            self.name, node.id, raw
                        )));
                    };
                    let Some(&source_index) = index_of.get(source_id) else {
                        return Err(TmError::invariant(format!(
                            "workflow {:?}: node {:?}'s for_each references unknown node {:?}",
                            self.name, node.id, source_id
                        )));
                    };
                    if source_index >= i {
                        return Err(TmError::invariant(format!(
                            "workflow {:?}: node {:?}'s for_each references node {:?}, which is not declared earlier in the file",
                            self.name, node.id, source_id
                        )));
                    }
                    let source_node = &self.nodes[source_index];
                    if source_node.produces.as_deref() != Some(field) {
                        return Err(TmError::invariant(format!(
                            "workflow {:?}: node {:?}'s for_each references {}.{field}, but node {:?} produces {:?}",
                            self.name, node.id, source_id, source_id, source_node.produces
                        )));
                    }
                }
            }
        }

        self.check_cycles()?;
        Ok(())
    }

    /// Every strongly-connected component of size > 1 in the `depends` graph is a cycle; every
    /// node in it must carry a `loop` (`CycleBudget`), per `SPEC.md` §25.2. Plain Tarjan-free
    /// detection (definitions have at most a handful of nodes): repeatedly walk from each
    /// undischarged node along `depends` edges, and any node reachable back to itself is a cycle
    /// member.
    fn check_cycles(&self) -> TmResult<()> {
        let index_of: BTreeMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        let adjacency: Vec<Vec<usize>> = self
            .nodes
            .iter()
            .map(|n| {
                n.depends
                    .iter()
                    .filter_map(|d| index_of.get(d.as_str()).copied())
                    .collect()
            })
            .collect();

        // Standard white/gray/black DFS cycle detection, collecting every node seen on a cycle.
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }
        let mut color = vec![Color::White; self.nodes.len()];
        let mut cyclic: BTreeSet<usize> = BTreeSet::new();
        let mut stack: Vec<usize> = Vec::new();

        fn visit(
            n: usize,
            adjacency: &[Vec<usize>],
            color: &mut [Color],
            stack: &mut Vec<usize>,
            cyclic: &mut BTreeSet<usize>,
        ) {
            color[n] = Color::Gray;
            stack.push(n);
            for &next in &adjacency[n] {
                match color[next] {
                    Color::White => visit(next, adjacency, color, stack, cyclic),
                    Color::Gray => {
                        // Back edge to `next`: every node on `stack` from `next` onward is on
                        // this cycle.
                        if let Some(pos) = stack.iter().position(|&s| s == next) {
                            cyclic.extend(&stack[pos..]);
                        }
                    }
                    Color::Black => {}
                }
            }
            stack.pop();
            color[n] = Color::Black;
        }

        for start in 0..self.nodes.len() {
            if color[start] == Color::White {
                visit(start, &adjacency, &mut color, &mut stack, &mut cyclic);
            }
        }

        let unbudgeted: Vec<&str> = cyclic
            .iter()
            .map(|&i| &self.nodes[i])
            .filter(|n| n.cycle.is_none())
            .map(|n| n.id.as_str())
            .collect();
        if !unbudgeted.is_empty() {
            return Err(TmError::invariant(format!(
                "workflow {:?}: depends-cycle through {unbudgeted:?} has no `loop` (CycleBudget) on every member",
                self.name
            )));
        }
        Ok(())
    }

    /// `SPEC.md` §25.3: "`tm doctor` should warn on a workflow whose graph is one node wide and
    /// one node deep, because that is a prompt wearing a costume." True for a definition with
    /// exactly one node, no fan-out (`for_each` absent, or a `Static` list of at most one item --
    /// a `FromOutput` fan-out is excluded even though `crate::expand` only emits one coordinator
    /// ticket for it today, since the *definition*'s intent is still genuine fan-out once
    /// `crate::expand::expand_fan_out` runs), and no dependencies (nothing to sequence).
    pub fn is_one_by_one(&self) -> bool {
        if self.nodes.len() != 1 {
            return false;
        }
        let node = &self.nodes[0];
        if !node.depends.is_empty() {
            return false;
        }
        match &node.for_each {
            None => true,
            Some(ForEach::Static(items)) => items.len() <= 1,
            Some(ForEach::FromOutput(_)) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> &'static str {
        r#"
name = "one-step"

[[node]]
id = "solo"
role = "coder_fast"
objective = "Do the one thing."
budget = { tokens = 100 }
verification = "none"
"#
    }

    #[test]
    fn parses_a_minimal_single_node_workflow() {
        let def = WorkflowDef::parse(minimal_toml()).expect("valid minimal workflow parses");
        assert_eq!(def.name, "one-step");
        assert_eq!(def.nodes.len(), 1);
        assert_eq!(def.nodes[0].role, Role::CoderFast);
        assert!(def.is_one_by_one());
    }

    #[test]
    fn rejects_empty_node_list() {
        let err = WorkflowDef::parse("name = \"empty\"\n").unwrap_err();
        assert!(err.to_string().contains("has no nodes"));
    }

    #[test]
    fn rejects_an_empty_static_for_each() {
        let source = r#"
name = "empty-fan-out"

[[node]]
id = "a"
role = "coder_fast"
objective = "x"
for_each = []
budget = { tokens = 1 }
verification = "none"
"#;
        let err = WorkflowDef::parse(source).unwrap_err();
        assert!(err.to_string().contains("empty list"));
    }

    #[test]
    fn rejects_a_node_id_with_a_reserved_character() {
        let source = r#"
name = "bad-id"

[[node]]
id = "a.b"
role = "coder_fast"
objective = "x"
budget = { tokens = 1 }
verification = "none"
"#;
        let err = WorkflowDef::parse(source).unwrap_err();
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn rejects_duplicate_node_ids() {
        let source = r#"
name = "dupe"

[[node]]
id = "a"
role = "coder_fast"
objective = "first"
budget = { tokens = 1 }
verification = "none"

[[node]]
id = "a"
role = "coder_fast"
objective = "second"
budget = { tokens = 1 }
verification = "none"
"#;
        let err = WorkflowDef::parse(source).unwrap_err();
        assert!(err.to_string().contains("duplicate node id"));
    }

    #[test]
    fn rejects_dangling_depends() {
        let source = r#"
name = "dangling"

[[node]]
id = "a"
role = "coder_fast"
objective = "a"
depends = ["ghost"]
budget = { tokens = 1 }
verification = "none"
"#;
        let err = WorkflowDef::parse(source).unwrap_err();
        assert!(err.to_string().contains("depends on unknown node"));
    }

    #[test]
    fn rejects_from_output_referencing_a_later_node() {
        let source = r#"
name = "forward-ref"

[[node]]
id = "a"
role = "coder_fast"
objective = "a"
for_each = "b.findings"
budget = { tokens = 1 }
verification = "none"

[[node]]
id = "b"
role = "coder_fast"
objective = "b"
produces = "findings"
budget = { tokens = 1 }
verification = "none"
"#;
        let err = WorkflowDef::parse(source).unwrap_err();
        assert!(err.to_string().contains("not declared earlier"));
    }

    #[test]
    fn rejects_from_output_field_mismatch() {
        let source = r#"
name = "field-mismatch"

[[node]]
id = "a"
role = "coder_fast"
objective = "a"
produces = "findings"
budget = { tokens = 1 }
verification = "none"

[[node]]
id = "b"
role = "coder_fast"
objective = "b"
for_each = "a.other_field"
budget = { tokens = 1 }
verification = "none"
"#;
        let err = WorkflowDef::parse(source).unwrap_err();
        assert!(err.to_string().contains("produces"));
    }

    #[test]
    fn rejects_a_cycle_missing_a_cycle_budget() {
        let source = r#"
name = "cyclic"

[[node]]
id = "a"
role = "coder_fast"
objective = "a"
depends = ["b"]
budget = { tokens = 1 }
verification = "none"

[[node]]
id = "b"
role = "coder_fast"
objective = "b"
depends = ["a"]
budget = { tokens = 1 }
verification = "none"
"#;
        let err = WorkflowDef::parse(source).unwrap_err();
        assert!(err.to_string().contains("has no `loop`"));
    }

    #[test]
    fn accepts_a_cycle_where_every_member_carries_a_cycle_budget() {
        let source = r#"
name = "cyclic-budgeted"

[[node]]
id = "a"
role = "coder_fast"
objective = "a"
depends = ["b"]
budget = { tokens = 1 }
verification = "none"
loop = { max_iterations = 3, iterations = 0 }

[[node]]
id = "b"
role = "coder_fast"
objective = "b"
depends = ["a"]
budget = { tokens = 1 }
verification = "none"
loop = { max_iterations = 3, iterations = 0 }
"#;
        let def = WorkflowDef::parse(source).expect("budgeted cycle is accepted");
        assert!(def.nodes.iter().all(|n| n.cycle.is_some()));
    }

    #[test]
    fn is_one_by_one_false_for_a_fanned_out_single_node() {
        let source = r#"
name = "fanned"

[[node]]
id = "solo"
role = "coder_fast"
objective = "{{item}}"
for_each = ["a", "b"]
budget = { tokens = 1 }
verification = "none"
"#;
        let def = WorkflowDef::parse(source).expect("valid");
        assert!(!def.is_one_by_one());
    }

    #[test]
    fn is_one_by_one_false_for_two_nodes() {
        let source = r#"
name = "two-step"

[[node]]
id = "a"
role = "coder_fast"
objective = "a"
budget = { tokens = 1 }
verification = "none"

[[node]]
id = "b"
role = "coder_fast"
objective = "b"
depends = ["a"]
budget = { tokens = 1 }
verification = "none"
"#;
        let def = WorkflowDef::parse(source).expect("valid");
        assert!(!def.is_one_by_one());
    }
}
