//! Offline e2e coverage for `tm-codeintel`'s index and `tm-context`'s pack compiler, in one
//! pass, over a real tempdir project — no network, no live model call.
//!
//! Three probes were previously verified only by hand (see
//! `docs/tasks/TASKS.md`'s `p1-e2e-navigation-freshness-search-prefetch`): that the index picks
//! up an edit made after the first index build (freshness), that exact and hybrid search return
//! the expected top hit for a known-content query, and that a `ContextPack` compiled for a
//! ticket comes back with at least one non-empty section. This file turns all three into a
//! regression test instead of leaving them as CLI probes nobody reruns.

use std::fs;

use tempfile::TempDir;
use tm_codeintel::{CodeIntel, Query, RetrievalContext, SignalWeights};
use tm_context::{compile, SectionKind, TokenBudget};
use tm_core::{
    ExecutorRequirements, ProjectView, RetryPolicy, Ticket, TicketKind, TicketState,
    VerificationPolicy,
};
use tm_provider::RoleTable;
use tm_types::{Authority, Budget, FixedClock, Role, TicketId, Timestamp, Tolerance};

/// A fresh temp project with a real (empty) git repo, so `update_incremental`'s history
/// ingestion has a valid `HEAD` to walk instead of failing on a repo with no commits — same
/// setup `tm-codeintel::api`'s own tests use.
fn new_project() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let repo = git2::Repository::init(dir.path()).expect("git init");
    let sig = git2::Signature::new("Test", "test@example.com", &git2::Time::new(0, 0))
        .expect("signature");
    let tree_id = {
        let mut index = repo.index().expect("repo index");
        index.write_tree().expect("write tree")
    };
    let tree = repo.find_tree(tree_id).expect("find tree");
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .expect("initial commit");
    dir
}

/// A minimal `Ready` ticket for [`compile`], mirroring `tm-context`'s own `pack.rs` test fixture.
fn base_ticket(objective: &str) -> Ticket {
    Ticket {
        id: TicketId::new("T-1").expect("literal id matches T-n"),
        kind: TicketKind::Work,
        objective: objective.to_string(),
        state: TicketState::Ready,
        parent: None,
        children: vec![],
        dependencies: vec![],
        milestone: None,
        due: None,
        authority: Authority::none(),
        resources: vec![],
        executor: ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Any,
        },
        context_refs: vec![],
        success: vec![],
        verification: VerificationPolicy::None,
        budget: Budget::none(),
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 60,
        },
        cycle: None,
        attempts: 0,
        failures: vec![],
        priority: 0,
        created: Timestamp::EPOCH,
        updated: Timestamp::EPOCH,
    }
}

/// (1) Freshness: editing a file after the first index build changes what a subsequent search
/// returns, both for the word that disappeared and the one that replaced it.
/// (2) Search modes: a known-content query via `search_exact` and `search_hybrid` returns the
/// expected top file over a small multi-file project.
/// (3) Prefetch: a `ContextPack` compiled for a ticket over that same indexed project comes back
/// with at least one non-empty admitted section.
///
/// All three run against the same tempdir project in one pass, matching
/// `p1-e2e-navigation-freshness-search-prefetch`'s acceptance check.
#[test]
fn navigation_freshness_search_and_prefetch() {
    let dir = new_project();
    let clock = FixedClock::epoch();

    // --- Seed a small, distinguishable project. `pantry.py` starts out about "giraffe treats"
    // and is later edited to be about "wombat parade" instead -- two words that share no
    // subtokens, so a lexical/BM25 signal over the *indexed* content (not a raw tree walk) can
    // only prefer `pantry.py` for one query at a time. ---
    fs::write(
        dir.path().join("math.rs"),
        "fn add_two_numbers(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .expect("write math.rs");
    fs::write(
        dir.path().join("unrelated.rs"),
        "fn totally_unrelated_thing() -> bool {\n    false\n}\n",
    )
    .expect("write unrelated.rs");
    fs::write(
        dir.path().join("pantry.py"),
        "def stock_the_pantry_with_giraffe_treats():\n    pass\n",
    )
    .expect("write pantry.py");

    let intel = CodeIntel::open(dir.path()).expect("open codeintel");
    let first = intel
        .update_incremental(&clock)
        .expect("first update_incremental");
    assert_eq!(first.files_added, 3, "all three seed files get indexed");

    // --- (1) Freshness: an edit made after the first index build changes what a subsequent
    // *index-backed* search returns, not just what a literal tree-walk search sees. ---
    let giraffe_query = Query {
        text: "giraffe treats".to_string(),
        seed_symbols: vec![],
        seed_paths: vec![],
    };
    let before = intel
        .search_hybrid(
            &giraffe_query,
            &RetrievalContext::default(),
            SignalWeights::default(),
        )
        .expect("search_hybrid before edit");
    assert!(
        !before.is_empty() && before[0].path == "pantry.py",
        "the original content ranks pantry.py first for a matching query, before the edit"
    );

    fs::write(
        dir.path().join("pantry.py"),
        "def summon_the_wombat_parade():\n    pass\n",
    )
    .expect("rewrite pantry.py");
    let delta = intel
        .update_incremental(&clock)
        .expect("second update_incremental");
    assert_eq!(
        delta.files_modified, 1,
        "the edited file is detected as modified"
    );

    let wombat_query = Query {
        text: "wombat parade".to_string(),
        seed_symbols: vec![],
        seed_paths: vec![],
    };
    let after = intel
        .search_hybrid(
            &wombat_query,
            &RetrievalContext::default(),
            SignalWeights::default(),
        )
        .expect("search_hybrid after edit");
    assert!(
        !after.is_empty() && after[0].path == "pantry.py",
        "the index has picked up the edit: a query matching only the new content now ranks \
         pantry.py first, which was impossible before the edit landed"
    );

    // A plain literal search over the tree corroborates the same freshness: the old marker
    // text is gone and the new one is present, in the same file.
    let old_literal = intel
        .search_exact("giraffe")
        .expect("search_exact after edit, old word");
    assert!(
        old_literal.hits.is_empty(),
        "the old word is no longer present anywhere in the tree after the edit"
    );
    let new_literal = intel
        .search_exact("wombat")
        .expect("search_exact after edit, new word");
    assert_eq!(new_literal.hits.len(), 1);
    assert_eq!(new_literal.hits[0].path, "pantry.py");

    // --- (2) Search modes: exact and hybrid queries return the expected top hit. ---
    let exact = intel
        .search_exact("add_two_numbers")
        .expect("search_exact for a known symbol");
    assert_eq!(exact.hits.len(), 1);
    assert_eq!(exact.hits[0].path, "math.rs");

    let query = Query {
        text: "add two numbers together".to_string(),
        seed_symbols: vec![],
        seed_paths: vec![],
    };
    let ctx = RetrievalContext::default();
    let hybrid_hits = intel
        .search_hybrid(&query, &ctx, SignalWeights::default())
        .expect("search_hybrid for a known-content query");
    assert!(
        !hybrid_hits.is_empty(),
        "hybrid search returns at least one hit for a query matching indexed content"
    );
    assert_eq!(
        hybrid_hits[0].path, "math.rs",
        "the file that actually adds two numbers ranks first, not the unrelated one"
    );

    // --- (3) Prefetch: compiling a ContextPack for a ticket produces at least one non-empty
    // admitted section. This goes through the same `tm_context::compile` the real attempt path
    // (`tm-cli`'s dispatch, outside this crate's dependency graph) calls per attempt, rather
    // than a pack pulled back off a finished `AgentLoop` run. ---
    let ticket = base_ticket("Add a function that adds two numbers together");
    let view = ProjectView::empty();
    let pack = compile(
        &ticket,
        &view,
        &intel,
        TokenBudget::even(100_000),
        SignalWeights::default(),
        &[],
        &RoleTable::default_table(),
    )
    .expect("compile a context pack for the ticket");

    assert!(
        !pack.sections.is_empty(),
        "the pack admits at least one section"
    );
    assert!(
        pack.sections.iter().any(|s| !s.body.is_empty()),
        "at least one admitted section has a non-empty body"
    );
    let objective_section = pack
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Objective)
        .expect("the objective section is always admitted for a generous budget");
    assert!(
        objective_section.body.contains("adds two numbers"),
        "the objective section reflects the ticket's own objective"
    );

    // The retrieval section is the one whose content actually depends on the index built
    // above, not just on the ticket's own fields -- so its presence and content prove the pack
    // is really prefetching from `intel`, not just echoing the ticket.
    let retrieval_section = pack
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Retrieval)
        .expect("a hybrid-retrieval section is admitted for a generous budget");
    assert!(
        !retrieval_section.body.is_empty(),
        "the retrieval section has real content, not an empty placeholder"
    );
    assert!(
        retrieval_section.body.contains("math.rs"),
        "the retrieval section surfaces the file that actually answers the ticket's objective, \
         confirming the pack drew on the index built earlier in this test"
    );
}
