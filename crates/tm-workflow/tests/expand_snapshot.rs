//! Snapshot tests of the two starter fixture workflows' *expanded* graphs.
//!
//! `expand` is pure and ref-addressed (no real ids, no clock), so its output is stable for a
//! given `WorkflowDef` + params -- exactly what makes it safe to snapshot. These tests exist so a
//! future change to `crate::expand`'s expansion logic that silently changes what graph a
//! known-good workflow produces gets caught, per this ticket's own requirement.

use std::collections::BTreeMap;

use tm_core::view::ProjectView;
use tm_workflow::{expand, WorkflowDef};

/// One starter fixture: its name, its raw TOML source, and the concrete params `expand` needs.
type StarterFixture = (
    &'static str,
    &'static str,
    Vec<(&'static str, &'static str)>,
);

/// The four starter fixtures, each paired with concrete params `expand` needs -- shared by the
/// acceptance test below.
fn starter_fixtures() -> Vec<StarterFixture> {
    vec![
        (
            "review-change",
            include_str!("../fixtures/review-change.toml"),
            vec![("target", "crates/tm-workflow/src/expand.rs")],
        ),
        (
            "harness-benchmark",
            include_str!("../fixtures/harness-benchmark.toml"),
            vec![("epoch", "7")],
        ),
        (
            "migrate-sites",
            include_str!("../fixtures/migrate-sites.toml"),
            vec![("target_schema", "v9")],
        ),
        (
            "research-and-synthesize",
            include_str!("../fixtures/research-and-synthesize.toml"),
            vec![("topic", "context pack budgets")],
        ),
    ]
}

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
fn migrate_sites_expansion_snapshot() {
    let source = include_str!("../fixtures/migrate-sites.toml");
    let def = WorkflowDef::parse(source).expect("migrate-sites.toml is a valid definition");
    let proposal = expand(
        &def,
        &params(&[("target_schema", "v9")]),
        &ProjectView::empty(),
    )
    .expect("migrate-sites expands");

    insta::assert_json_snapshot!("migrate_sites_expansion", &proposal);
}

#[test]
fn research_and_synthesize_expansion_snapshot() {
    let source = include_str!("../fixtures/research-and-synthesize.toml");
    let def =
        WorkflowDef::parse(source).expect("research-and-synthesize.toml is a valid definition");
    let proposal = expand(
        &def,
        &params(&[("topic", "context pack budgets")]),
        &ProjectView::empty(),
    )
    .expect("research-and-synthesize expands");

    insta::assert_json_snapshot!("research_and_synthesize_expansion", &proposal);
}

#[test]
fn every_starter_fixture_expansion_passes_tm_core_invariant_checks() {
    for (name, source, param_pairs) in starter_fixtures() {
        let def = WorkflowDef::parse(source)
            .unwrap_or_else(|e| panic!("starter fixture {name:?} is a valid definition: {e}"));
        let proposal = expand(&def, &params(&param_pairs), &ProjectView::empty())
            .unwrap_or_else(|e| panic!("starter fixture {name:?} expands: {e}"));
        let violations = tm_genesis::compile::validate_graph(&proposal, &ProjectView::empty());
        assert!(
            violations.is_empty(),
            "starter fixture {name:?}'s expansion should pass tm-core's invariant checks, got {violations:?}"
        );
    }
}

#[test]
fn neither_starter_fixture_is_a_1x1_workflow() {
    for source in [
        include_str!("../fixtures/review-change.toml"),
        include_str!("../fixtures/harness-benchmark.toml"),
        include_str!("../fixtures/migrate-sites.toml"),
        include_str!("../fixtures/research-and-synthesize.toml"),
    ] {
        let def = WorkflowDef::parse(source).expect("fixture is a valid definition");
        assert!(
            !def.is_one_by_one(),
            "starter fixture {:?} should not be a 1x1 workflow",
            def.name
        );
    }
}
