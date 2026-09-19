//! Expanding a [`crate::def::WorkflowDef`] plus concrete parameter values into a proposed ticket
//! graph.
//!
//! [`expand`] returns a [`tm_genesis::compile::GraphCompilation`] -- the *same* type
//! `tm-genesis`'s spec-to-graph compilation produces -- so `tm_genesis::compile::validate_graph`
//! validates a workflow expansion exactly the way it validates a genesis compilation, with no
//! second graph validator for this crate to maintain. `expand` is pure and clock-free: every
//! `ProposedTicket` is addressed by [`Ref`] (a local string, not a real [`tm_types::TicketId`],
//! which doesn't exist until [`crate::commit::commit`] mints one), so the same `WorkflowDef` +
//! params always produces byte-identical output -- what makes it safe to snapshot-test (see
//! `tests/`) and safe to validate before anything is written to the store.
//!
//! # `FromOutput` fan-out: staged in two passes, not one
//!
//! [`crate::def::ForEach::FromOutput`] asks to fan out over an upstream node's *realized*
//! output -- but at `expand` time the upstream node has not run: its ticket doesn't exist yet
//! (this call is what creates it), so neither does its result, and the fan-out's cardinality is
//! unknown. `expand` therefore emits exactly **one** ticket for a `FromOutput` node: a
//! coordinator, whose objective documents the pending fan-out (source node, field, and the
//! per-item template it will apply) rather than attempting to render `{{item...}}` tokens against
//! nothing. Downstream nodes that `depends` on it depend on that one coordinator ticket.
//!
//! The actual per-item expansion is [`expand_fan_out`], a second, separate pure function: given
//! the node and the upstream output's realized items (as JSON-or-plain-text strings), it returns
//! the same `Vec<ProposedTicket>` shape `expand` itself builds for a `Static` fan-out. Driving
//! `expand_fan_out` automatically once the coordinator's upstream dependency actually closes --
//! reading its `produces` field back out of evidence, calling `expand_fan_out`, committing the
//! result, and rewiring whatever depended on the coordinator to depend on the new tickets instead
//! -- is scheduler-triggered work (a `ticket.verified`-shaped subscriber) that belongs with
//! `tm-scheduler`, out of this ticket's scope; this crate provides the pure expansion half only.

use std::collections::BTreeMap;

use tm_core::ticket::{
    ContextRef, DependencyKind, ExecutorRequirements, ResourceClaim, RetryPolicy, TicketKind,
};
use tm_core::view::ProjectView;
use tm_genesis::compile::{GraphCompilation, ProposedDependency, ProposedTicket, Ref};
use tm_types::{Authority, Predicate, Result as TmResult, TmError};

use crate::def::{ForEach, NodeDef, WorkflowDef};
use crate::template;

/// Resolve `def.params` against caller-supplied `params`: an explicit value wins, otherwise the
/// declared default, otherwise an error naming the missing required parameter. Keys in `params`
/// not declared by `def.params` are ignored (a template referencing them still errors, via
/// [`template::render`]'s own unknown-parameter check).
fn resolve_params(
    def: &WorkflowDef,
    params: &BTreeMap<String, String>,
) -> TmResult<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    for (name, param_def) in &def.params {
        let value = params
            .get(name)
            .cloned()
            .or_else(|| param_def.default.clone())
            .ok_or_else(|| {
                TmError::invariant(format!(
                    "workflow {:?}: missing required param {name:?} (no default declared)",
                    def.name
                ))
            })?;
        resolved.insert(name.clone(), value);
    }
    Ok(resolved)
}

/// The fixed retry policy every expanded ticket gets. The DSL has no per-node retry
/// configuration (not in this ticket's field list); a moderate, shared default is preferable to
/// silently defaulting to `RetryPolicy`'s zero value (which has no `Default` impl for exactly
/// that reason -- see `tm-core::ticket`).
fn default_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay_seconds: 30,
        backoff_multiplier: 2.0,
        max_delay_seconds: 600,
    }
}

/// Build one [`ProposedTicket`] for `node`, addressed by `ticket_ref`, with its objective already
/// rendered. Every field the DSL does not expose (`authority`, `resources`, `success`, `retry`,
/// `priority`) gets a fixed, conservative default -- `Authority::read_only()` in particular, since
/// the DSL has no per-node authority declaration yet; a node whose work needs more must be given
/// it some other way today (a known, documented limitation, not an oversight).
fn build_ticket(node: &NodeDef, ticket_ref: Ref, objective: String) -> ProposedTicket {
    ProposedTicket {
        ticket_ref,
        kind: TicketKind::Work,
        objective: objective.clone(),
        parent_ref: None,
        milestone_ref: None,
        authority: Authority::read_only(),
        resources: Vec::<ResourceClaim>::new(),
        executor: ExecutorRequirements {
            role: node.role,
            human_required: false,
            min_capability: node.role.default_tolerance(),
        },
        context_refs: Vec::<ContextRef>::new(),
        success: vec![Predicate::Judgment { claim: objective }],
        verification: node.verification.clone(),
        budget: node.budget.to_budget(),
        retry: default_retry_policy(),
        priority: 0,
    }
}

/// Expand one node into its `(Ref, rendered objective)` pairs, per its [`ForEach`].
fn expand_node(node: &NodeDef, params: &BTreeMap<String, String>) -> TmResult<Vec<(Ref, String)>> {
    match &node.for_each {
        None => {
            let objective = template::render(&node.objective, params, None)?;
            Ok(vec![(node.id.clone(), objective)])
        }
        Some(ForEach::Static(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let ticket_ref = format!("{}[{i}]", node.id);
                let objective = template::render(&node.objective, params, Some(item))?;
                Ok((ticket_ref, objective))
            })
            .collect(),
        Some(for_each @ ForEach::FromOutput(_)) => {
            // `expect`: `WorkflowDef::validate` (already run by `expand`, see its call site)
            // rejects a malformed `for_each: FromOutput` string before this ever runs.
            let (source_id, field) = for_each
                .from_output_parts()
                .expect("validated FromOutput always splits on '.'");
            let objective = format!(
                "Fan out over {source_id}.{field} once available; for each item, run: {}",
                node.objective
            );
            Ok(vec![(node.id.clone(), objective)])
        }
    }
}

/// The node id a `ticket_ref` `expand`/`expand_fan_out` produced was expanded from: the part
/// before a `[` (fan-out index suffix), or the whole ref for a non-fanned node. Valid because
/// `WorkflowDef::validate` rejects a node `id` containing `[`, `]` or `.` -- the only characters
/// this crate's own ref/`for_each` encodings use as separators.
pub fn node_id_of_ticket_ref(ticket_ref: &str) -> &str {
    ticket_ref.split('[').next().unwrap_or(ticket_ref)
}

/// Turn `def` + concrete `params` into a proposed ticket graph, addressed by [`Ref`] rather than
/// real ids. `view` is accepted (per this crate's task contract) for parity with
/// `tm_genesis::compile::validate_graph`'s own signature and so a future version of `expand` can
/// consult existing project state (e.g. to avoid re-proposing an already-running instance)
/// without a signature change; today's expansion does not read it, which is also why `expand`
/// itself never touches a clock or an id source -- both properties `tests/` relies on to
/// snapshot-test its output byte-for-byte.
///
/// # Errors
/// Whatever [`WorkflowDef::validate`] returns, a missing required parameter, or a
/// [`template::render`] failure (unknown parameter, malformed `{{...}}` token, or an `{{item...}}`
/// token used where no item is in scope).
pub fn expand(
    def: &WorkflowDef,
    params: &BTreeMap<String, String>,
    _view: &ProjectView,
) -> TmResult<GraphCompilation> {
    def.validate()?;
    let resolved = resolve_params(def, params)?;

    let mut tickets: Vec<ProposedTicket> = Vec::new();
    let mut refs_by_node: BTreeMap<&str, Vec<Ref>> = BTreeMap::new();

    for node in &def.nodes {
        let expanded = expand_node(node, &resolved)?;
        let mut refs = Vec::with_capacity(expanded.len());
        for (ticket_ref, objective) in expanded {
            refs.push(ticket_ref.clone());
            tickets.push(build_ticket(node, ticket_ref, objective));
        }
        refs_by_node.insert(node.id.as_str(), refs);
    }

    let mut dependencies: Vec<ProposedDependency> = Vec::new();
    for node in &def.nodes {
        if node.depends.is_empty() {
            continue;
        }
        let my_refs = &refs_by_node[node.id.as_str()];
        let kind = if node.cycle.is_some() {
            DependencyKind::Loop
        } else {
            DependencyKind::Hard
        };
        for dep_id in &node.depends {
            let dep_refs = &refs_by_node[dep_id.as_str()];
            for from_ref in my_refs {
                for to_ref in dep_refs {
                    dependencies.push(ProposedDependency {
                        from_ref: from_ref.clone(),
                        to_ref: to_ref.clone(),
                        kind,
                    });
                }
            }
        }
    }

    Ok(GraphCompilation {
        source_spec: None,
        tickets,
        dependencies,
        milestones: vec![],
        authority_domains: vec![],
        attempt: 1,
        selected_template: None,
    })
}

/// The second expansion pass for a [`ForEach::FromOutput`] node: given `items` (the upstream
/// node's realized output, one entry per fan-out element -- plain text or JSON object text, per
/// [`template::render`]'s `{{item}}`/`{{item.field}}` rules), return the `Vec<ProposedTicket>`
/// this node expands to, exactly as [`expand`] would have for a `Static` fan-out over the same
/// `items`. `node` must have `for_each = Some(ForEach::FromOutput(_))`.
///
/// This is pure and does not itself commit, rewire the coordinator ticket [`expand`] created for
/// `node`, or touch any dependent node -- see this module's doc comment for the scope boundary.
/// `params` is already-resolved (the same map an earlier [`expand`] call used, not raw
/// `tm workflow run --param` input) -- resolving defaults again here would need the whole
/// [`WorkflowDef`] this function deliberately does not take.
///
/// # Errors
/// `TmError::invariant` if `node.for_each` is not `FromOutput`; otherwise whatever
/// [`template::render`] returns.
pub fn expand_fan_out(
    node: &NodeDef,
    params: &BTreeMap<String, String>,
    items: &[String],
) -> TmResult<Vec<ProposedTicket>> {
    if !matches!(node.for_each, Some(ForEach::FromOutput(_))) {
        return Err(TmError::invariant(format!(
            "node {:?} is not a FromOutput node",
            node.id
        )));
    }
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let ticket_ref = format!("{}[{i}]", node.id);
        let objective = template::render(&node.objective, params, Some(item))?;
        out.push(build_ticket(node, ticket_ref, objective));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::view::ProjectView;

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn node_id_of_ticket_ref_strips_the_fan_out_suffix() {
        assert_eq!(node_id_of_ticket_ref("dimension[2]"), "dimension");
        assert_eq!(node_id_of_ticket_ref("solo"), "solo");
    }

    #[test]
    fn expands_a_single_node_workflow_to_one_ticket() {
        let def = WorkflowDef::parse(
            r#"
name = "one-step"

[[node]]
id = "solo"
role = "coder_fast"
objective = "Do the thing to {{target}}."
budget = { tokens = 100 }
verification = "none"

[params.target]
default = "src/lib.rs"
"#,
        )
        .expect("valid");
        let proposal = expand(&def, &params(&[]), &ProjectView::empty()).expect("expand succeeds");
        assert_eq!(proposal.tickets.len(), 1);
        assert_eq!(proposal.tickets[0].ticket_ref, "solo");
        assert_eq!(proposal.tickets[0].objective, "Do the thing to src/lib.rs.");
        assert!(proposal.dependencies.is_empty());
    }

    #[test]
    fn static_fan_out_produces_one_ticket_per_item_and_cross_product_dependencies() {
        let def = WorkflowDef::parse(
            r#"
name = "fan"

[[node]]
id = "dimension"
role = "reviewer_semantic"
objective = "Review {{target}} for {{item}} problems."
for_each = ["correctness", "security"]
produces = "findings"
budget = { tokens = 100 }
verification = "none"

[[node]]
id = "synthesize"
role = "synthesizer_long_context"
objective = "Summarize {{target}}."
depends = ["dimension"]
budget = { tokens = 100 }
verification = "single"

[params.target]
default = "src/lib.rs"
"#,
        )
        .expect("valid");
        let proposal = expand(&def, &params(&[]), &ProjectView::empty()).expect("expand succeeds");

        let dim_refs: Vec<&str> = proposal
            .tickets
            .iter()
            .filter(|t| t.ticket_ref.starts_with("dimension"))
            .map(|t| t.ticket_ref.as_str())
            .collect();
        assert_eq!(dim_refs, vec!["dimension[0]", "dimension[1]"]);
        assert!(proposal
            .tickets
            .iter()
            .any(|t| t.objective.contains("correctness")));
        assert!(proposal
            .tickets
            .iter()
            .any(|t| t.objective.contains("security")));

        // "synthesize" (one ticket) depends on every "dimension" ticket: a 1x2 cross product.
        assert_eq!(proposal.dependencies.len(), 2);
        for dep in &proposal.dependencies {
            assert_eq!(dep.from_ref, "synthesize");
            assert!(dep.to_ref.starts_with("dimension["));
        }
    }

    #[test]
    fn from_output_node_expands_to_a_single_coordinator_ticket() {
        let def = WorkflowDef::parse(
            r#"
name = "verify-findings"

[[node]]
id = "dimension"
role = "reviewer_semantic"
objective = "Review for {{item}}."
for_each = ["correctness"]
produces = "findings"
budget = { tokens = 100 }
verification = "none"

[[node]]
id = "verify"
role = "auditor_semantic"
objective = "Try to refute this finding: {{item.summary}}"
for_each = "dimension.findings"
depends = ["dimension"]
budget = { tokens = 100 }
verification = "single"
"#,
        )
        .expect("valid");
        let proposal = expand(&def, &params(&[]), &ProjectView::empty()).expect("expand succeeds");

        let verify_tickets: Vec<_> = proposal
            .tickets
            .iter()
            .filter(|t| t.ticket_ref.starts_with("verify"))
            .collect();
        assert_eq!(verify_tickets.len(), 1);
        assert_eq!(verify_tickets[0].ticket_ref, "verify");
        assert!(verify_tickets[0].objective.contains("dimension.findings"));

        assert_eq!(proposal.dependencies.len(), 1);
        assert_eq!(proposal.dependencies[0].from_ref, "verify");
        assert_eq!(proposal.dependencies[0].to_ref, "dimension[0]");
    }

    #[test]
    fn expand_fan_out_produces_one_ticket_per_realized_item() {
        let def = WorkflowDef::parse(
            r#"
name = "verify-findings"

[[node]]
id = "dimension"
role = "reviewer_semantic"
objective = "Review for {{item}}."
for_each = ["correctness"]
produces = "findings"
budget = { tokens = 100 }
verification = "none"

[[node]]
id = "verify"
role = "auditor_semantic"
objective = "Try to refute this finding: {{item.summary}}"
for_each = "dimension.findings"
depends = ["dimension"]
budget = { tokens = 100 }
verification = "single"
"#,
        )
        .expect("valid");
        let verify_node = def.nodes.iter().find(|n| n.id == "verify").expect("node");
        let items = vec![
            r#"{"summary": "unchecked index"}"#.to_string(),
            r#"{"summary": "missing auth check"}"#.to_string(),
        ];
        let tickets = expand_fan_out(verify_node, &params(&[]), &items).expect("fan out succeeds");
        assert_eq!(tickets.len(), 2);
        assert_eq!(tickets[0].ticket_ref, "verify[0]");
        assert!(tickets[0].objective.contains("unchecked index"));
        assert_eq!(tickets[1].ticket_ref, "verify[1]");
        assert!(tickets[1].objective.contains("missing auth check"));
    }

    #[test]
    fn expand_fan_out_rejects_a_non_from_output_node() {
        let def = WorkflowDef::parse(
            r#"
name = "one-step"

[[node]]
id = "solo"
role = "coder_fast"
objective = "x"
budget = { tokens = 1 }
verification = "none"
"#,
        )
        .expect("valid");
        let err = expand_fan_out(&def.nodes[0], &params(&[]), &[]).unwrap_err();
        assert!(err.to_string().contains("is not a FromOutput node"));
    }

    #[test]
    fn expand_errors_on_missing_required_param() {
        let def = WorkflowDef::parse(
            r#"
name = "needs-param"

[[node]]
id = "solo"
role = "coder_fast"
objective = "{{target}}"
budget = { tokens = 1 }
verification = "none"

[params.target]
"#,
        )
        .expect("valid");
        let err = expand(&def, &params(&[]), &ProjectView::empty()).unwrap_err();
        assert!(err.to_string().contains("missing required param"));
    }
}
