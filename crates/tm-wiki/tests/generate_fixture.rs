//! Lighter, always-run integration proof for `tm-wiki` (`SPEC.md` §26, B-14): a small,
//! hermetic fixture project (one crate, one real ticket, one real decision, one real commit)
//! rather than this actual workspace. `tests/e2e_real_repo.rs` (`#[ignore]`'d) is the heavier,
//! manually-run proof against the real repository this crate lives in; this test exists so CI
//! and every default `cargo test` run still exercise `tm_wiki::run`/`tm_wiki::dry_run` end to
//! end, without the cost of indexing ~120k lines of real Rust.
//!
//! What this proves that the crate's own unit tests (which mostly build a `WikiPage`/`ProjectView`
//! by hand) do not: that `dry_run` writes nothing while still reporting real content, that `run`
//! writes real files, and that a page's content is genuinely assembled from this fixture's own
//! ticket/decision/symbol data -- not a static template that would look the same for any project.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use tm_codeintel::CodeIntel;
use tm_core::{ExecutorRequirements, RetryPolicy, Store, TicketKind, VerificationPolicy};
use tm_types::{
    Authority, Budget, Clock, CounterIds, FixedClock, IdSource, ParticipantId, Role, Tolerance,
};

/// A minimal `ExecutorRequirements` -- the fixture doesn't exercise scheduling, so these values
/// only need to satisfy `Store::create_ticket`'s validation, not mean anything further.
fn executor() -> ExecutorRequirements {
    ExecutorRequirements {
        role: Role::CoderFast,
        human_required: false,
        min_capability: Tolerance::Any,
    }
}

fn retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay_seconds: 1,
        backoff_multiplier: 2.0,
        max_delay_seconds: 60,
    }
}

/// Write a one-crate fixture workspace under `root` and commit it: `crates/demo/src/lib.rs`
/// declares one distinctively-named public function, so a generated `architecture/demo.md` page
/// can only contain that name if it was genuinely assembled from this file, not a template.
fn write_fixture_crate(root: &Path) {
    let src_dir = root.join("crates/demo/src");
    fs::create_dir_all(&src_dir).expect("create crates/demo/src");
    fs::write(
        root.join("crates/demo/Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write demo Cargo.toml");
    fs::write(
        src_dir.join("lib.rs"),
        "//! Fixture demo crate for tm-wiki's generate_fixture integration test.\n\n\
         /// A distinctively-named public function this test greps the generated page for.\n\
         pub fn fixture_wiki_marker_fn(name: &str) -> String {\n    \
             format!(\"hello, {name}\")\n}\n",
    )
    .expect("write demo/src/lib.rs");
}

fn commit_all(root: &Path) {
    let repo = git2::Repository::init(root).expect("git init");
    let sig = git2::Signature::new("Fixture", "fixture@example.com", &git2::Time::new(0, 0))
        .expect("signature");
    let tree_id = {
        let mut index = repo.index().expect("repo index");
        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .expect("stage everything");
        index.write().expect("write index");
        index.write_tree().expect("write tree")
    };
    let tree = repo.find_tree(tree_id).expect("find tree");
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "fixture: add demo crate",
        &tree,
        &[],
    )
    .expect("commit fixture");
}

#[test]
fn dry_run_and_run_produce_genuinely_derived_content_from_a_fixture_project() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();

    write_fixture_crate(root);
    commit_all(root);

    let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
    let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
    let store = Store::open_with(root, clock.clone(), ids.clone()).expect("open store");
    let actor = ParticipantId::system();

    // A real ticket, through the real command path -- not a hand-built `ProjectView`.
    let ticket_events = store
        .create_ticket(
            TicketKind::Work,
            "Prove the fixture wiki pages are real".to_string(),
            None,
            None,
            Authority::none(),
            vec![],
            executor(),
            vec![],
            vec![],
            VerificationPolicy::None,
            Budget::none(),
            retry(),
            0,
            actor.clone(),
        )
        .expect("create_ticket");
    assert!(!ticket_events.is_empty());

    // A real decision, likewise through `Store::record_decision`.
    store
        .record_decision(
            "Use a fixture crate for the wiki integration test".to_string(),
            "Generate one real crate, ticket, and decision rather than mocking ProjectView"
                .to_string(),
            "Proves tm_wiki::run/dry_run end to end without the cost of indexing this workspace"
                .to_string(),
            vec![],
            vec![],
            vec!["crates/demo/src/lib.rs".to_string()],
            actor.clone(),
        )
        .expect("record_decision");

    let ci = CodeIntel::open(root).expect("open code index");
    ci.update_incremental(clock.as_ref())
        .expect("index the fixture repository");

    let view = store.view().expect("view");
    assert_eq!(
        view.tickets.len(),
        1,
        "the fixture ticket must be materialized"
    );
    assert_eq!(
        view.decisions.len(),
        1,
        "the fixture decision must be materialized"
    );

    let history_paths = tm_wiki::default_history_paths(&view, root);
    assert!(history_paths.contains(&"crates/demo/src/lib.rs".to_string()));

    // --- dry-run: reports real content, writes nothing ---
    let preview = tm_wiki::dry_run(root, &store, &ci, &history_paths).expect("dry_run");
    assert!(
        preview.written().count() > 0,
        "dry_run should report pages it would write"
    );
    assert!(
        !root.join("docs/wiki").exists(),
        "dry_run must not create docs/wiki/"
    );
    let architecture_demo = preview
        .pages
        .iter()
        .find(|p| p.path == "docs/wiki/architecture/demo.md")
        .expect("architecture/demo.md previewed");
    assert_eq!(
        architecture_demo.outcome,
        tm_wiki::WriteOutcome::Written,
        "a brand new architecture/demo.md should preview as Written, not Skipped"
    );

    // --- real run: same pages, actually written this time, with genuinely derived content ---
    let report =
        tm_wiki::run(root, &store, &ci, clock.as_ref(), actor, &history_paths).expect("run");
    assert!(report.written().count() > 0);
    assert_eq!(
        report.pages.len(),
        preview.pages.len(),
        "dry_run and run must assemble the exact same set of pages"
    );

    let architecture = fs::read_to_string(root.join("docs/wiki/architecture/demo.md"))
        .expect("architecture/demo.md was written");
    assert!(
        architecture.contains("fixture_wiki_marker_fn"),
        "architecture page must contain this fixture's real public symbol, not boilerplate:\n{architecture}"
    );
    assert!(architecture.contains("crates/demo/src/lib.rs"));

    let tickets =
        fs::read_to_string(root.join("docs/wiki/tickets.md")).expect("tickets.md was written");
    assert!(
        tickets.contains("Prove the fixture wiki pages are real"),
        "tickets page must contain this fixture's real ticket objective, not boilerplate:\n{tickets}"
    );

    let decisions =
        fs::read_to_string(root.join("docs/wiki/decisions.md")).expect("decisions.md was written");
    assert!(
        decisions.contains("Use a fixture crate for the wiki integration test"),
        "decisions index must contain this fixture's real decision subject, not boilerplate:\n{decisions}"
    );

    let glossary =
        fs::read_to_string(root.join("docs/wiki/glossary.md")).expect("glossary.md was written");
    assert!(
        glossary.contains("Work"),
        "glossary must list the fixture ticket's real kind (Work), not boilerplate:\n{glossary}"
    );
    assert!(glossary.contains("Use a fixture crate for the wiki integration test"));

    let history = fs::read_to_string(root.join("docs/wiki/history/crates__demo__src__lib.rs.md"))
        .expect("history/crates__demo__src__lib.rs.md was written");
    assert!(
        history.contains("fixture: add demo crate"),
        "history page must contain this fixture's real commit message, not boilerplate:\n{history}"
    );

    // Regenerating over the same, still-`Generated`-mode output must be idempotent: every page
    // writes again cleanly rather than erroring or getting stuck as newly "Skipped".
    let second = tm_wiki::run(
        root,
        &store,
        &ci,
        clock.as_ref(),
        ParticipantId::system(),
        &history_paths,
    )
    .expect("second run");
    assert_eq!(second.written().count(), report.written().count());
    assert_eq!(second.skipped().count(), 0);
}
