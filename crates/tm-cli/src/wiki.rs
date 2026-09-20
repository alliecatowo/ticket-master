//! `tm wiki generate`: the project wiki (`SPEC.md` §26, B-14) — assembles generated
//! documentation pages (`architecture/<crate>`, `decisions/`, `history/<path>`, `tickets`,
//! `glossary`) from live project state (`Store::view()`, `tm-codeintel`'s symbol index, git
//! history) and writes them under `docs/wiki/*.md` at the project's workspace root, not its
//! state directory (D-003: wiki docs are workspace documentation, the same bucket as
//! `templates.toml`/`bench/tasks`/the web client build — see
//! `docs/decisions/D-003-project-scope.md` and `crate::project::Project::root`'s own doc
//! comment). All assembly and write-protection logic lives in `tm-wiki`; this module only wires
//! the CLI surface to it and renders the result.

use crate::args::{WikiCommand, WikiGenerateArgs};
use crate::project::Project;
use crate::render::{Renderer, Table};
use tm_wiki::{GenerationReport, WriteOutcome};

/// Dispatch one [`WikiCommand`].
pub fn dispatch_wiki(
    cmd: &WikiCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        WikiCommand::Generate(args) => wiki_generate(args, project, renderer),
    }
}

/// `tm wiki generate [--dry-run]`
///
/// # IMPL
/// Bring the code index up to date first ([`tm_codeintel::CodeIntel::update_incremental`]) —
/// `tm_wiki::generate::run`/`dry_run` only *read* from the index, they never (re)index anything
/// themselves, so a stale index would silently generate stale `architecture`/`history` pages.
/// `history_paths` comes from `tm_wiki::default_history_paths`, the same default the crate's own
/// e2e proof (`crates/tm-wiki/tests/e2e_real_repo.rs`) uses when the caller has no stronger
/// opinion. `--dry-run` calls [`tm_wiki::dry_run`] instead of [`tm_wiki::run`]: same assembly,
/// same human/maintained protection decision, nothing written to disk or the store.
pub fn wiki_generate(
    args: &WikiGenerateArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ci = project.code_intel()?;
    ci.update_incremental(project.clock.as_ref())?;

    let view = project.store.view()?;
    let history_paths = tm_wiki::default_history_paths(&view, &project.root);

    let report = if args.dry_run {
        tm_wiki::dry_run(&project.root, &project.store, &ci, &history_paths)?
    } else {
        tm_wiki::run(
            &project.root,
            &project.store,
            &ci,
            project.clock.as_ref(),
            project.actor.clone(),
            &history_paths,
        )?
    };

    render_report(&report, args.dry_run, renderer)
}

/// Render a [`GenerationReport`] as a table (one row per page considered) or JSON, plus a
/// one-line written/skipped summary in human mode.
fn render_report(
    report: &GenerationReport,
    dry_run: bool,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let written_count = report.written().count();
    let skipped_count = report.skipped().count();

    if renderer.is_json() {
        let pages: Vec<_> = report
            .pages
            .iter()
            .map(|p| {
                let (outcome, reason) = match &p.outcome {
                    WriteOutcome::Written => ("written", None),
                    WriteOutcome::Skipped { reason } => ("skipped", Some(reason.as_str())),
                };
                serde_json::json!({
                    "id": p.id,
                    "path": p.path,
                    "outcome": outcome,
                    "reason": reason,
                })
            })
            .collect();
        renderer.emit(
            &serde_json::json!({
                "dry_run": dry_run,
                "written": written_count,
                "skipped": skipped_count,
                "pages": pages,
            }),
            "",
        )?;
        return Ok(());
    }

    let verb = if dry_run { "would write" } else { "wrote" };
    renderer.note(&format!(
        "{verb} {written_count} wiki page(s); skipped {skipped_count} (human/maintained-owned or \
         undeclared)"
    ));

    let rows: Vec<Vec<String>> = report
        .pages
        .iter()
        .map(|p| match &p.outcome {
            WriteOutcome::Written => vec![
                p.path.clone(),
                if dry_run { "WOULD WRITE" } else { "WRITTEN" }.to_string(),
                String::new(),
            ],
            WriteOutcome::Skipped { reason } => {
                vec![p.path.clone(), "SKIPPED".to_string(), reason.clone()]
            }
        })
        .collect();
    let table = Table::new(
        vec![
            "PATH".to_string(),
            "OUTCOME".to_string(),
            "DETAIL".to_string(),
        ],
        rows,
    );
    renderer.emit(&(), &table.render())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tm_types::{CounterIds, FixedClock};

    /// Initialize a git repo with one empty commit at `path`, so `CodeIntel::update_incremental`
    /// (called by [`wiki_generate`]) has a valid `HEAD` to walk instead of failing on a repo with
    /// no commits — same fixture shape as `tm_codeintel::api::tests::init_git_repo`.
    fn init_git_repo(path: &std::path::Path) {
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

    fn test_project(root: &std::path::Path) -> Project {
        let clock: Arc<dyn tm_types::Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn tm_types::IdSource> = Arc::new(CounterIds::new());
        let store = Arc::new(
            tm_core::Store::open_with(root, clock.clone(), ids.clone()).expect("open store"),
        );
        Project::for_test(root, store, clock, ids)
    }

    /// `tm wiki generate --dry-run` against a project with no crates/tickets/decisions must not
    /// create `docs/wiki/` at all -- a preview that leaves a stray empty directory behind is not
    /// a real preview.
    #[test]
    fn dry_run_creates_no_docs_wiki_directory() {
        let dir = TempDir::new().unwrap();
        // A real git repo with a commit, not just a bare directory: `CodeIntel::update_incremental`
        // (via `Project::code_intel`) walks git history, which requires a valid `HEAD`.
        init_git_repo(dir.path());
        let project = test_project(dir.path());
        let renderer = Renderer::from_flags(false, true, true);

        let args = WikiGenerateArgs { dry_run: true };
        wiki_generate(&args, &project, &renderer).unwrap();

        assert!(
            !dir.path().join("docs/wiki").exists(),
            "dry-run must not create docs/wiki/"
        );
    }
}
