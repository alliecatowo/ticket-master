//! Snapshot tests of the two starter fixture workflows' *expanded* graphs.
//!
//! `expand` is pure and ref-addressed (no real ids, no clock), so its output is stable for a
//! given `WorkflowDef` + params -- exactly what makes it safe to snapshot. These tests exist so a
//! future change to `crate::expand`'s expansion logic that silently changes what graph a
//! known-good workflow produces gets caught, per this ticket's own requirement.

use std::collections::BTreeMap;

use tm_core::view::ProjectView;
use tm_workflow::{expand, WorkflowDef};

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn review_change_expansion_snapshot() {
    let source = include_str!("../fixtures/review-change.toml");
    let def = WorkflowDef::parse(source).expect("review-change.toml is a valid definition");
    let proposal = expand(
        &def,
        &params(&[("target", "crates/tm-workflow/src/expand.rs")]),
        &ProjectView::empty(),
    )
    .expect("review-change expands");

    insta::assert_json_snapshot!("review_change_expansion", &proposal);
}

#[test]
fn harness_benchmark_expansion_snapshot() {
    let source = include_str!("../fixtures/harness-benchmark.toml");
    let def = WorkflowDef::parse(source).expect("harness-benchmark.toml is a valid definition");
    let proposal = expand(&def, &params(&[("epoch", "7")]), &ProjectView::empty())
        .expect("harness-benchmark expands");

    insta::assert_json_snapshot!("harness_benchmark_expansion", &proposal);
}

#[test]
fn harness_benchmark_expansion_uses_its_declared_default_param() {
    let source = include_str!("../fixtures/harness-benchmark.toml");
    let def = WorkflowDef::parse(source).expect("harness-benchmark.toml is a valid definition");
    // No `epoch` supplied: the definition's own `default = "current"` should be used.
    let proposal = expand(&def, &params(&[]), &ProjectView::empty())
        .expect("harness-benchmark expands using its default param");
    assert!(proposal
        .tickets
        .iter()
        .any(|t| t.objective.contains("epoch current")));
}

#[test]
fn neither_starter_fixture_is_a_1x1_workflow() {
    for source in [
        include_str!("../fixtures/review-change.toml"),
        include_str!("../fixtures/harness-benchmark.toml"),
    ] {
        let def = WorkflowDef::parse(source).expect("fixture is a valid definition");
        assert!(
            !def.is_one_by_one(),
            "starter fixture {:?} should not be a 1x1 workflow",
            def.name
        );
    }
}
