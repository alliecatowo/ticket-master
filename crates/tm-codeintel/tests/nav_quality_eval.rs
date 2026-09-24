//! A repeatable code-navigation search-quality eval (critic-nav-quality-eval): a fixed,
//! hand-authored corpus plus a set of (query, expected file) pairs, so a change to hybrid
//! retrieval's ranking is scored against a pinned baseline instead of eyeballed, per SPEC.md
//! section 10's "searches before first relevant hit" and section 32.2's rg-fallback-count ask.
//! This does not itself score an embedder switch (D-025): it always opens with the default
//! no-network `LocalHashEmbedder` (see below), which is the one thing that must stay fixed for
//! this eval to be reproducible without a real model or a cached download.
//!
//! `fixtures/nav_eval.toml` is the pinned corpus and query set; see that file's header comment
//! for why it is a small hand-authored fixture rather than a live index of this actual
//! repository (the recorded earlier attempt at this task set an 8/12 hit-rate@3 floor with no
//! prior CI run to calibrate it against, and failed a real run at 6/12; reverted).
//!
//! Uses [`tm_codeintel::CodeIntel::open`] (the default no-network [`LocalHashEmbedder`], not
//! `open_auto`), so this test is deterministic on any machine regardless of what embedder is
//! cached locally, matching every other test in this crate (SPEC 0's "deterministic machinery
//! stays pure/unit-testable with no network and no model calls").

use std::fs;
use std::path::Path;

use serde::Deserialize;
use tempfile::TempDir;
use tm_codeintel::hybrid::{Query, RetrievalContext, SignalWeights};
use tm_codeintel::CodeIntel;
use tm_types::FixedClock;

/// The pinned corpus and query set, frozen alongside this test (see the fixture's own header
/// comment for why it is hand-authored rather than a snapshot of this repository).
const FIXTURE_TOML: &str = include_str!("../fixtures/nav_eval.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NavFixture {
    file: Vec<FixtureFile>,
    case: Vec<FixtureCase>,
}

/// One frozen corpus file: `path` relative to the indexed project root, `content` written
/// verbatim.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureFile {
    path: String,
    content: String,
}

/// One query/expected-hit pair: `query` should surface `expected_file` in `search_hybrid`'s
/// results within the checked `k`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureCase {
    query: String,
    expected_file: String,
}

/// Initialize a git repo with one empty commit at `path`, so
/// `CodeIntel::update_incremental` (which walks git history via `HistoryIndex`) has a valid
/// `HEAD` instead of failing on a repo with no commits -- same pattern
/// `crates/tm-codeintel/src/api.rs`'s own `mod tests` uses.
fn init_git_repo(path: &Path) {
    let repo = git2::Repository::init(path).expect("git init");
    let sig = git2::Signature::new("Test", "test@example.com", &git2::Time::new(0, 0))
        .expect("signature");
    let tree_id = {
        let mut index = repo.index().expect("repo index");
        index.write_tree().expect("write tree")
    };
    let tree = repo.find_tree(tree_id).expect("find tree");
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .expect("initial commit");
}

/// Materialize the fixture's corpus files into a fresh temp project and build its index.
fn indexed_fixture_project(fixture: &NavFixture) -> (TempDir, CodeIntel) {
    let dir = TempDir::new().expect("temp dir");
    init_git_repo(dir.path());

    for file in &fixture.file {
        let full_path = dir.path().join(&file.path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).expect("create fixture file's parent dir");
        }
        fs::write(&full_path, &file.content).expect("write fixture file");
    }

    let intel = CodeIntel::open(dir.path()).expect("open code intel index");
    let clock = FixedClock::epoch();
    intel
        .update_incremental(&clock)
        .expect("index the fixture corpus");

    (dir, intel)
}

/// Runs every `[[case]]` in `fixtures/nav_eval.toml` through `search_hybrid`, reports
/// hit-rate@1/3/5, and asserts a floor on hit-rate@3 so a real retrieval regression fails this
/// test instead of only ever being eyeballed.
#[test]
fn nav_quality_eval_hit_rate() {
    let fixture: NavFixture = toml::from_str(FIXTURE_TOML).expect("parse fixtures/nav_eval.toml");
    assert!(
        fixture.case.len() >= 10,
        "acceptance requires at least 10 query/expected-file pairs, found {}",
        fixture.case.len()
    );

    let (_dir, intel) = indexed_fixture_project(&fixture);

    let mut hits_at_1 = 0usize;
    let mut hits_at_3 = 0usize;
    let mut hits_at_5 = 0usize;
    let mut misses: Vec<String> = Vec::new();

    for case in &fixture.case {
        let query = Query {
            text: case.query.clone(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let hits = intel
            .search_hybrid(
                &query,
                &RetrievalContext::default(),
                SignalWeights::default(),
            )
            .unwrap_or_else(|e| panic!("search_hybrid for {:?} failed: {e}", case.query));

        let rank = hits.iter().position(|h| h.path == case.expected_file);
        if let Some(idx) = rank {
            if idx < 1 {
                hits_at_1 += 1;
            }
            if idx < 3 {
                hits_at_3 += 1;
            }
            if idx < 5 {
                hits_at_5 += 1;
            }
        }
        let missed_top_3 = match rank {
            Some(idx) => idx >= 3,
            None => true,
        };
        if missed_top_3 {
            let top3: Vec<&str> = hits.iter().take(3).map(|h| h.path.as_str()).collect();
            misses.push(format!(
                "query {:?}: expected {:?} in top-3, got {:?} (rank {:?})",
                case.query, case.expected_file, top3, rank
            ));
        }
    }

    let total = fixture.case.len();
    println!(
        "nav_quality_eval: hit-rate@1={hits_at_1}/{total} hit-rate@3={hits_at_3}/{total} \
         hit-rate@5={hits_at_5}/{total}"
    );

    // A floor derived from the fixture's own case count, not a hardcoded number, so it tracks
    // the fixture instead of silently drifting if a case is added or removed later (the
    // convention this repo's CLAUDE.md asks for after the SectionKind/B-10/B-14 incident).
    // 75% leaves headroom below what a well-separated, hand-authored corpus like this one
    // should score (every case's query shares several exclusive, exact-token-form words with
    // only its expected file -- see the fixture's header comment) while still catching a real
    // regression in hybrid retrieval.
    let floor = (total * 3) / 4;
    assert!(
        hits_at_3 >= floor,
        "hit-rate@3 regressed: {hits_at_3}/{total} hits, need at least {floor}/{total}\n{}",
        misses.join("\n")
    );
}
