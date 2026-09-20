//! Proves the actual point of SPEC.md §26.1/§9: a generated wiki page is a real `DocRecord`
//! routed through `tm-docs`'s own staleness state machine (`ProvenanceIndex` + `Assessor`), not
//! a parallel mechanism this crate invents. Pure in-memory, no filesystem or `CodeIntel` index
//! involved — this is specifically about whether the `derived_from` strings [`tm_wiki`]'s page
//! builders emit actually get picked up by `tm-docs`'s real glob/decision matching.

use tm_docs::{Assessor, ChangeSet, DocMode, DocRecord, DocRegistry, DocState, ProvenanceIndex};

use tm_wiki::WikiPage;

/// Build the `DocRecord` `tm-wiki::page::write_page` would build for a freshly-written page
/// (mode = generated), already `Fresh` (as if a prior generation run had verified it), so the
/// only thing under test is whether `changes` moves it to `Stale`.
fn fresh_doc_record(page: &WikiPage) -> DocRecord {
    let mut record = DocRecord::new(
        page.id.clone(),
        page.project_path(),
        DocMode::Generated,
        page.derived_from.clone(),
    );
    record.state = DocState::Fresh;
    record
}

fn assessor_for(pages: &[WikiPage]) -> Assessor {
    let mut registry = DocRegistry::new();
    for page in pages {
        registry.insert(fresh_doc_record(page));
    }
    let docs: Vec<DocRecord> = registry.list().into_iter().cloned().collect();
    let provenance =
        ProvenanceIndex::build(&docs).expect("compile provenance for well-formed pages");
    Assessor::new(registry, provenance)
}

#[test]
fn architecture_page_goes_stale_when_its_crate_changes() {
    // Exactly the derived_from shape `architecture::pages` emits: a single `crates/<name>/src/**`
    // glob (see crates/tm-wiki/src/architecture.rs).
    let arch_page = WikiPage::new(
        "architecture/tm-core",
        "architecture/tm-core.md",
        "# Architecture: tm-core\n",
        vec!["crates/tm-core/src/**".to_string()],
    );

    let mut assessor = assessor_for(std::slice::from_ref(&arch_page));

    // A change to a real file inside that crate must touch the page...
    let touches = ChangeSet {
        changed_paths: vec!["crates/tm-core/src/store.rs".to_string()],
        superseded_decisions: Vec::new(),
    };
    let assessments = assessor.assess(&touches);
    assert_eq!(assessments.len(), 1);
    assert_eq!(assessments[0].doc_id, arch_page.id);
    assert_eq!(assessments[0].new_state, DocState::Stale);

    // ...and an unrelated crate's change must not.
    let mut other_assessor = assessor_for(std::slice::from_ref(&arch_page));
    let unrelated = ChangeSet {
        changed_paths: vec!["crates/tm-server/src/routes.rs".to_string()],
        superseded_decisions: Vec::new(),
    };
    assert!(other_assessor.assess(&unrelated).is_empty());
}

#[test]
fn decision_page_goes_stale_when_its_source_file_changes() {
    // `decisions::pages` is filesystem-driven now (see `crates/tm-wiki/src/decisions.rs`'s module
    // doc): a real `docs/decisions/D-NNN-*.md` fixture file, not a hand-built `ProjectView`, and
    // `derived_from` is that file's real path — a `DecisionId`-shaped `ChangeSet.superseded_decisions`
    // entry can never touch it again, exactly like `architecture::pages`'s glob-path basis above.
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("docs/decisions")).unwrap();
    std::fs::write(
        dir.path().join("docs/decisions/D-001-use-sqlite.md"),
        "# D-001 — Use SQLite\n\n**Status:** accepted · **Date:** 2026-01-01 · **Supersedes:** \
         nothing\n\n## Context\n\nBody.\n",
    )
    .unwrap();

    let pages = tm_wiki::decisions::pages(dir.path()).expect("pages built from the fixture");
    let decision_page = pages
        .iter()
        .find(|p| p.rel_path == "decisions/D-001-use-sqlite.md")
        .expect("decisions::pages built a page for D-001")
        .clone();
    assert_eq!(
        decision_page.derived_from,
        vec!["docs/decisions/D-001-use-sqlite.md".to_string()]
    );

    let mut assessor = assessor_for(std::slice::from_ref(&decision_page));

    // A change to this decision's own source file must touch the page...
    let touches = ChangeSet {
        changed_paths: vec!["docs/decisions/D-001-use-sqlite.md".to_string()],
        superseded_decisions: Vec::new(),
    };
    let assessments = assessor.assess(&touches);
    assert_eq!(assessments.len(), 1);
    assert_eq!(assessments[0].doc_id, decision_page.id);
    assert_eq!(assessments[0].new_state, DocState::Stale);

    // ...and an unrelated decision's file must not.
    let mut other_assessor = assessor_for(std::slice::from_ref(&decision_page));
    let unrelated = ChangeSet {
        changed_paths: vec!["docs/decisions/D-002-something-else.md".to_string()],
        superseded_decisions: Vec::new(),
    };
    assert!(other_assessor.assess(&unrelated).is_empty());
}

#[test]
fn decisions_index_page_goes_stale_when_any_decision_file_changes_or_is_added() {
    // `decisions.md`'s own `derived_from` is a directory glob (`docs/decisions/**`, see
    // `decisions::pages`), not a single path — this is the page that was visibly broken (it
    // rendered "No decisions recorded yet" despite 14 real docs on disk) and is the headline
    // deliverable of this change, so its own staleness basis needs the same real proof as a
    // single decision's does above.
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("docs/decisions")).unwrap();
    std::fs::write(
        dir.path().join("docs/decisions/D-001-use-sqlite.md"),
        "# D-001 — Use SQLite\n\n**Status:** accepted · **Date:** 2026-01-01 · **Supersedes:** \
         nothing\n\n## Context\n\nBody.\n",
    )
    .unwrap();

    let pages = tm_wiki::decisions::pages(dir.path()).expect("pages built from the fixture");
    let index_page = pages
        .iter()
        .find(|p| p.rel_path == "decisions.md")
        .expect("decisions::pages built the index page")
        .clone();
    assert_eq!(
        index_page.derived_from,
        vec!["docs/decisions/**".to_string()]
    );

    // A change to an existing decision file must touch the index...
    let mut assessor = assessor_for(std::slice::from_ref(&index_page));
    let touches = ChangeSet {
        changed_paths: vec!["docs/decisions/D-001-use-sqlite.md".to_string()],
        superseded_decisions: Vec::new(),
    };
    let assessments = assessor.assess(&touches);
    assert_eq!(assessments.len(), 1);
    assert_eq!(assessments[0].doc_id, index_page.id);
    assert_eq!(assessments[0].new_state, DocState::Stale);

    // ...and so must a brand new decision file under the same directory, not just an edit to an
    // existing one -- the index has to notice a new D-NNN doc was added, not only that one changed.
    let mut other_assessor = assessor_for(std::slice::from_ref(&index_page));
    let new_file = ChangeSet {
        changed_paths: vec!["docs/decisions/D-002-use-postgres.md".to_string()],
        superseded_decisions: Vec::new(),
    };
    let assessments = other_assessor.assess(&new_file);
    assert_eq!(assessments.len(), 1);
    assert_eq!(assessments[0].new_state, DocState::Stale);

    // ...but an unrelated path outside docs/decisions/ must not.
    let mut unrelated_assessor = assessor_for(std::slice::from_ref(&index_page));
    let unrelated = ChangeSet {
        changed_paths: vec!["crates/tm-core/src/store.rs".to_string()],
        superseded_decisions: Vec::new(),
    };
    assert!(unrelated_assessor.assess(&unrelated).is_empty());
}
