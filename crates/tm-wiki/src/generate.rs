//! Orchestration: assemble every page family and write them under `docs/wiki/`
//! (`SPEC.md` §26.2-26.4), routed through [`crate::page::write_page`]'s human-mode gate, and
//! register each written page via `Store::register_doc` — the real `tm-core` write path any
//! doc's registration goes through (see `crates/tm-core/src/store.rs`'s doc comment on
//! `register_doc`).

use std::collections::BTreeSet;
use std::path::Path;

use tm_codeintel::CodeIntel;
use tm_core::view::ProjectView;
use tm_core::Store;
use tm_types::{Clock, ParticipantId, Result};

use crate::page::{preview_write, write_page, WikiPage, WriteOutcome};
use crate::{architecture, decisions, glossary, history, tickets};

/// One page's outcome after a [`run`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageOutcome {
    /// The page's front-matter id (`wiki/<family>/<name>`).
    pub id: String,
    /// Path relative to the project root (`docs/wiki/...`).
    pub path: String,
    /// What [`crate::page::write_page`] actually did.
    pub outcome: WriteOutcome,
}

/// Every page's outcome from one generation pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenerationReport {
    /// One entry per page considered, in the order pages were assembled.
    pub pages: Vec<PageOutcome>,
}

impl GenerationReport {
    /// Pages that were written (created or regenerated).
    pub fn written(&self) -> impl Iterator<Item = &PageOutcome> {
        self.pages
            .iter()
            .filter(|p| matches!(p.outcome, WriteOutcome::Written))
    }

    /// Pages left untouched because they are human/maintained-owned (or undeclared).
    pub fn skipped(&self) -> impl Iterator<Item = &PageOutcome> {
        self.pages
            .iter()
            .filter(|p| matches!(p.outcome, WriteOutcome::Skipped { .. }))
    }
}

/// Assemble every page family from `store`/`ci`'s current state, in the fixed order [`run`] and
/// [`dry_run`] both write/report in. Shared by both so they can never disagree about which pages
/// exist for a given project state.
///
/// Requires `ci` to already be up to date ([`CodeIntel::update_incremental`]) — this function
/// only reads from the existing index (via `outline`/`history_why`), it never indexes anything
/// itself. `history_paths` seeds the `history/<path>` family; see [`default_history_paths`] for a
/// reasonable default when the caller has no stronger opinion.
fn assemble_pages(
    project_root: &Path,
    store: &Store,
    ci: &CodeIntel,
    history_paths: &[String],
) -> Result<Vec<WikiPage>> {
    let view = store.view()?;

    let mut all_pages: Vec<WikiPage> = Vec::new();
    all_pages.extend(architecture::pages(project_root, ci)?);
    all_pages.extend(decisions::pages(project_root)?);
    all_pages.extend(history::pages(project_root, ci, history_paths)?);
    all_pages.push(tickets::page(&view));
    all_pages.push(glossary::page(&view));
    Ok(all_pages)
}

/// Assemble every wiki page family from `store`/`ci`'s current state and write them under
/// `docs/wiki/` at `project_root`. See [`assemble_pages`] for the indexing precondition and
/// `history_paths`.
pub fn run(
    project_root: &Path,
    store: &Store,
    ci: &CodeIntel,
    clock: &dyn Clock,
    actor: ParticipantId,
    history_paths: &[String],
) -> Result<GenerationReport> {
    let all_pages = assemble_pages(project_root, store, ci, history_paths)?;

    let mut report = GenerationReport::default();
    for page in &all_pages {
        let outcome = write_page(project_root, page, clock)?;
        if matches!(outcome, WriteOutcome::Written) {
            store.register_doc(page.project_path(), None, actor.clone())?;
        }
        report.pages.push(PageOutcome {
            id: page.id.clone(),
            path: page.project_path(),
            outcome,
        });
    }

    Ok(report)
}

/// Preview [`run`] without writing anything to disk or registering anything in `store`: assemble
/// the exact same pages `run` would, then resolve each page's outcome via
/// [`crate::page::preview_write`] instead of [`crate::page::write_page`] — the same
/// human/maintained protection decision, without touching the filesystem. Backs
/// `tm wiki generate --dry-run`.
pub fn dry_run(
    project_root: &Path,
    store: &Store,
    ci: &CodeIntel,
    history_paths: &[String],
) -> Result<GenerationReport> {
    let all_pages = assemble_pages(project_root, store, ci, history_paths)?;

    let mut report = GenerationReport::default();
    for page in &all_pages {
        let outcome = preview_write(project_root, page)?;
        report.pages.push(PageOutcome {
            id: page.id.clone(),
            path: page.project_path(),
            outcome,
        });
    }

    Ok(report)
}

/// A reasonable default `history_paths` set for [`run`]: every path named in any decision's
/// `affected_paths`, deduplicated; falls back to each workspace crate's `src/lib.rs` when no
/// decision has recorded any yet (e.g. a brand-new project with no decisions recorded).
pub fn default_history_paths(view: &ProjectView, project_root: &Path) -> Vec<String> {
    let mut set: BTreeSet<String> = view
        .decisions
        .values()
        .flat_map(|d| d.affected_paths.iter().cloned())
        .collect();

    if set.is_empty() {
        if let Ok(entries) = std::fs::read_dir(project_root.join("crates")) {
            for entry in entries.flatten() {
                if entry.path().join("Cargo.toml").is_file() {
                    if let Some(name) = entry.file_name().to_str() {
                        let candidate = format!("crates/{name}/src/lib.rs");
                        if project_root.join(&candidate).is_file() {
                            set.insert(candidate);
                        }
                    }
                }
            }
        }
    }

    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::decision::Decision;
    use tm_types::{ArtifactId, DecisionId, Timestamp};

    #[test]
    fn default_history_paths_uses_decision_affected_paths() {
        let mut view = ProjectView::empty();
        let d = Decision::create(
            DecisionId::new("D-1").unwrap(),
            "subject".to_string(),
            "decision".to_string(),
            "reason".to_string(),
            Vec::<ArtifactId>::new(),
            Vec::new(),
            vec!["crates/tm-core/src/lib.rs".to_string()],
            ParticipantId::system(),
            Timestamp::EPOCH,
        );
        view.decisions.insert(d.id.clone(), d);

        let dir = tempfile::TempDir::new().unwrap();
        let paths = default_history_paths(&view, dir.path());
        assert_eq!(paths, vec!["crates/tm-core/src/lib.rs".to_string()]);
    }

    #[test]
    fn default_history_paths_falls_back_to_crate_lib_rs_files() {
        let view = ProjectView::empty();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/tm-a/src")).unwrap();
        std::fs::write(
            dir.path().join("crates/tm-a/Cargo.toml"),
            "[package]\nname=\"tm-a\"",
        )
        .unwrap();
        std::fs::write(dir.path().join("crates/tm-a/src/lib.rs"), "// empty\n").unwrap();

        let paths = default_history_paths(&view, dir.path());
        assert_eq!(paths, vec!["crates/tm-a/src/lib.rs".to_string()]);
    }
}
