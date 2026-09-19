//! Manual, real-repository end-to-end proof for `tm-wiki` (`SPEC.md` §26).
//!
//! Not part of the default test run: it indexes and reads *this actual workspace* (a real
//! `CodeIntel::update_incremental` over ~120k lines of Rust plus a real git history walk), which
//! is slow, and it writes into the real repository's `.tm/` (gitignored) and `docs/wiki/`
//! (cleaned up at the end of this test). Run explicitly:
//!
//! ```sh
//! cargo test -p tm-wiki --test e2e_real_repo -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use tm_codeintel::CodeIntel;
use tm_core::Store;
use tm_types::{Clock, CounterIds, FixedClock, IdSource, ParticipantId};

/// The workspace root: this crate lives at `<root>/crates/tm-wiki`.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/tm-wiki has two parent directories under the workspace root")
        .to_path_buf()
}

#[test]
#[ignore = "indexes and reads the real repository; run explicitly with --ignored, not part of the default suite"]
fn generate_wiki_against_this_repository() {
    let root = workspace_root();
    assert!(
        root.join("SPEC.md").is_file(),
        "resolved workspace root looks wrong: {}",
        root.display()
    );

    let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
    let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
    let store = Store::open_with(&root, clock.clone(), ids).expect("open the real project store");

    let ci = CodeIntel::open(&root).expect("open the real code index");
    let delta = ci
        .update_incremental(clock.as_ref())
        .expect("index the real repository");
    eprintln!("indexed: {delta:?}");

    let view = store.view().expect("read the real project view");
    let history_paths = tm_wiki::default_history_paths(&view, &root);
    eprintln!("history_paths: {history_paths:?}");

    let report = tm_wiki::run(
        &root,
        &store,
        &ci,
        clock.as_ref(),
        ParticipantId::system(),
        &history_paths,
    )
    .expect("generate the wiki against real project state");

    let written: Vec<_> = report.written().collect();
    eprintln!("wrote {} page(s):", written.len());
    for page in &written {
        eprintln!("  {} ({})", page.path, page.id);
    }
    for skipped in report.skipped() {
        eprintln!("  SKIPPED {} ({})", skipped.path, skipped.id);
    }

    assert!(
        !written.is_empty(),
        "expected at least one real wiki page to be generated"
    );

    // A real architecture page for this very crate should exist and name a real public type
    // this crate actually defines -- not boilerplate, genuine assembled content.
    let arch_path = root.join("docs/wiki/architecture/tm-wiki.md");
    let arch_content =
        std::fs::read_to_string(&arch_path).expect("architecture/tm-wiki.md was written");
    assert!(
        arch_content.contains("WikiPage"),
        "expected a real public symbol name in the generated architecture page:\n{arch_content}"
    );
    assert!(arch_content.contains("## Module tree"));
    assert!(arch_content.contains("crates/tm-wiki/src/"));

    // The tickets and glossary pages must exist too, even if this particular worktree's fresh
    // `.tm/project.db` has no recorded tickets/decisions yet.
    assert!(root.join("docs/wiki/tickets.md").is_file());
    assert!(root.join("docs/wiki/glossary.md").is_file());
    assert!(root.join("docs/wiki/decisions.md").is_file());

    // Clean up: remove only what this test itself wrote, so a manual run leaves no stray files
    // in the real repository. `.tm/` is already gitignored and left in place (harmless, and
    // removing a live SQLite WAL/index file out from under nothing is not worth the risk).
    let _ = std::fs::remove_dir_all(root.join("docs/wiki"));
}
