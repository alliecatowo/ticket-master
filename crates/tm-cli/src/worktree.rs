//! `tm run <ticket> --worktree`: a real, per-run `git worktree add` isolation boundary for a
//! delegated executor, so a worker's own file edits/commits land on a fresh branch in a
//! throwaway checkout instead of the main working tree (`docs/audit-2026-09-18-fable.md` M-04,
//! `docs/decisions/D-012-run-worktree-isolation.md`).
//!
//! [`create`] is the only entry point; [`TicketWorktree::cleanup`] is the only way back out.
//! Both shell out to real `git` subprocesses — the same convention `tm_scheduler::snapshot`
//! (D-008's `git stash create`) and `tm-agent`'s own agent-invocable `git.worktree` tool already
//! use, rather than reimplementing worktree plumbing against `git2` (already a dependency here,
//! used read-only elsewhere in this crate for repo discovery).

use std::path::{Path, PathBuf};
use std::process::Command;

use tm_types::{IdSource, TicketId, TmError};

use crate::project::{Project, Scope};

/// One `git worktree add`-created checkout dedicated to a single `tm run --worktree` invocation.
#[derive(Debug)]
pub struct TicketWorktree {
    /// Absolute path to the worktree's checkout — what a caller should point an executor's
    /// working directory at.
    pub path: PathBuf,
    /// The fresh branch `git worktree add -b` created off `HEAD`, checked out at `path`.
    pub branch: String,
    /// The main repository this worktree belongs to; `git worktree remove` must run from a
    /// checkout that knows about it, and the main checkout always does.
    repo_root: PathBuf,
}

/// `--worktree` requires a real, non-bare git repository with at least one commit at
/// `project.root` — checked with `git2` (already a dependency; `crate::project::workspace_root_for`
/// resolves the same way) rather than by parsing a `git` subprocess's stderr, so each precondition
/// is a distinct, named [`TmError`] instead of one confusing failure (task requirement: a project
/// with no git repo, or a bare one, or a repo with no commits yet, must each say so plainly).
fn require_usable_git_repo(root: &Path) -> tm_types::Result<()> {
    let repo = git2::Repository::open(root).map_err(|_| {
        TmError::parse(format!(
            "--worktree needs a real git repository at {}, but none was found. Run `git init` \
             first, or drop --worktree to run against the main checkout.",
            root.display()
        ))
    })?;
    if repo.is_bare() {
        return Err(TmError::parse(format!(
            "--worktree needs a non-bare git repository at {}, but found a bare one.",
            root.display()
        )));
    }
    // `git worktree add -b <branch> <path> HEAD` needs `HEAD` to resolve to a real commit;
    // `Repository::head` errors on an "unborn" HEAD (a repo with `git init` but no commits yet),
    // which is exactly the case this turns into a named error instead of a raw `git` subprocess's
    // "invalid reference: HEAD" stderr.
    if repo.head().is_err() {
        return Err(TmError::parse(format!(
            "--worktree needs at least one commit at {} (HEAD doesn't resolve yet). Commit \
             something first, or drop --worktree.",
            root.display()
        )));
    }
    Ok(())
}

/// Create a fresh worktree for `ticket`: a new branch off the current `HEAD`, checked out under
/// `<project.state_dir>/worktrees/<ticket>-<suffix>/`.
///
/// Requires [`Scope::Repo`] (a repo-local `.tm/` must already exist, i.e. `project.state_dir` is
/// literally `<project.root>/.tm`) in addition to [`require_usable_git_repo`] — the audit's own
/// "done looks like" line spells the path as `<workspace_root>/.tm/worktrees/`, and going through
/// the already-resolved `state_dir` field rather than re-joining `.tm` onto `project.root` by
/// hand is what keeps this call site honoring `docs/decisions/D-003-project-scope.md` (a
/// global-scope project's workspace has no repo-local `.tm/` to nest a worktree under, and this
/// must never silently create one — see xtask's hygiene "no stray `.tm` literal" check, which
/// this satisfies by construction rather than by allowlist).
pub fn create(project: &Project, ticket: &TicketId) -> tm_types::Result<TicketWorktree> {
    if project.scope != Scope::Repo {
        return Err(TmError::parse(
            "--worktree needs a repo-scoped project. Run `tm init` here first; a global-scope \
             project has no repo-local .tm/ to nest a worktree under."
                .to_string(),
        ));
    }
    require_usable_git_repo(&project.root)?;

    // `std::process::id()`, not the hygiene-forbidden `rand::thread_rng`/`SystemTime::now`: a
    // fresh `tm` process re-seeds `project.ids`' RNG deterministically from persisted counters
    // (`crate::project::open_at`), so two separate `tm run --worktree` invocations that are each
    // the first caller of `IdSource::random_hex` in their process would otherwise draw the exact
    // same hex string — pairing it with the OS pid (already this codebase's convention for a
    // scratch path unique across processes, e.g. `tm-computer`'s `linux.rs`) makes a collision
    // require both a repeated pid *and* an identical `random_hex` draw at once.
    let suffix = format!("{}-{}", std::process::id(), project.ids.random_hex(6));
    let dir_name = format!("{ticket}-{suffix}");
    let worktrees_root = project.state_dir.join("worktrees");
    std::fs::create_dir_all(&worktrees_root)
        .map_err(|e| TmError::Io(format!("creating {}: {e}", worktrees_root.display())))?;
    let path = worktrees_root.join(&dir_name);
    let branch = format!("tm/run/{dir_name}");

    let output = Command::new("git")
        .args(["worktree", "add", "-b", &branch])
        .arg(&path)
        .arg("HEAD")
        .current_dir(&project.root)
        .output()
        .map_err(|e| TmError::Io(format!("running `git worktree add`: {e}")))?;
    if !output.status.success() {
        return Err(TmError::parse(format!(
            "git worktree add failed for ticket {ticket}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(TicketWorktree {
        path,
        branch,
        repo_root: project.root.clone(),
    })
}

impl TicketWorktree {
    /// Remove this worktree (`git worktree remove --force`, since the executor may have left
    /// untracked files a plain remove would otherwise refuse to delete). Best-effort: logs and
    /// swallows a failure rather than propagating one, matching
    /// `tm_scheduler::snapshot::capture_workspace_snapshot`'s "auxiliary, never load-bearing"
    /// shape — a caller that already decided to clean up should not have that decision undone by
    /// `git` itself failing partway through.
    pub fn cleanup(self) {
        let output = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .current_dir(&self.repo_root)
            .output();
        match output {
            Ok(out) if out.status.success() => {}
            Ok(out) => tracing::warn!(
                path = %self.path.display(),
                stderr = %String::from_utf8_lossy(&out.stderr),
                "git worktree remove did not succeed; leaving it on disk"
            ),
            Err(e) => tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "could not run `git worktree remove`; leaving it on disk"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tm_core::store::Store;
    use tm_core::{ExecutorRequirements, RetryPolicy, TicketKind, VerificationPolicy};
    use tm_types::{
        Authority, Budget, Clock, CounterIds, FixedClock, IdSource, ParticipantId, Role, Tolerance,
    };

    /// A real tempdir git repo with one commit and a configured identity, plus the repo-scoped
    /// `tm` project state `Scope::Repo`/[`create`] require.
    fn init_repo_project(root: &Path) -> Project {
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(root.join("README.md"), "hello\n").expect("write file");
        // Matching a real `tm init` repo (which a future change should make write this itself —
        // it does not today): without it, `.tm/`'s own database/artifacts show up as untracked
        // in `git status --porcelain` regardless of anything the worktree lifecycle does, which
        // would make the "main checkout untouched" assertion below pass or fail for the wrong
        // reason.
        std::fs::write(root.join(".gitignore"), ".tm/\n").expect("write .gitignore");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);

        let state_dir = root.join(".tm");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Arc::new(
            Store::open_with_at(&state_dir, clock.clone(), ids.clone()).expect("open store"),
        );
        Project::for_test(root, store, clock, ids)
    }

    fn seed_ticket(project: &Project) -> TicketId {
        let actor = ParticipantId::new("human:test").expect("actor");
        let events = project
            .store
            .create_ticket(
                TicketKind::Work,
                "a test ticket".to_string(),
                None,
                None,
                Authority::root(),
                vec![],
                ExecutorRequirements {
                    role: Role::CoderFast,
                    human_required: false,
                    min_capability: Tolerance::Any,
                },
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::unlimited(),
                RetryPolicy {
                    max_attempts: 3,
                    base_delay_seconds: 1,
                    backoff_multiplier: 2.0,
                    max_delay_seconds: 60,
                },
                0,
                actor,
            )
            .expect("create ticket");
        TicketId::new(events[0].subject.as_str()).expect("ticket id")
    }

    #[test]
    fn create_adds_a_real_worktree_on_a_fresh_branch_off_head() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = init_repo_project(dir.path());
        let ticket = seed_ticket(&project);

        let wt = create(&project, &ticket).expect("create worktree");

        assert!(wt.path.exists(), "worktree checkout must exist on disk");
        assert!(
            wt.path.starts_with(project.state_dir.join("worktrees")),
            "worktree must live under state_dir/worktrees, not project.root directly"
        );
        assert!(wt.path.join("README.md").exists(), "branched off real HEAD");

        let list = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&project.root)
            .output()
            .expect("git worktree list");
        let out = String::from_utf8_lossy(&list.stdout);
        assert!(
            out.contains(
                &wt.path
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ) || out.contains(&wt.path.to_string_lossy().into_owned()),
            "git itself must know about the new worktree: {out}"
        );

        let branch = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&wt.path)
            .output()
            .expect("git rev-parse");
        assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), wt.branch);
    }

    #[test]
    fn cleanup_removes_the_worktree_and_leaves_the_main_checkout_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = init_repo_project(dir.path());
        let ticket = seed_ticket(&project);

        let wt = create(&project, &ticket).expect("create worktree");
        let path = wt.path.clone();
        wt.cleanup();

        assert!(!path.exists(), "cleanup must remove the checkout");
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&project.root)
            .output()
            .expect("git status");
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "the main checkout's working tree must be unaffected by the worktree's lifecycle"
        );
    }

    #[test]
    fn changes_committed_in_the_worktree_land_on_its_own_branch_not_the_main_checkout() {
        // Task requirement (M-04): prove isolation with a real commit, not just an assertion
        // about paths — a file written and committed *inside* the worktree must be invisible
        // both to the main checkout's working tree on disk and to the main branch's own history,
        // exactly what a delegated worker actually doing real work would produce.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = init_repo_project(dir.path());
        let ticket = seed_ticket(&project);
        let main_branch = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&project.root)
            .output()
            .expect("git rev-parse");
        let main_branch = String::from_utf8_lossy(&main_branch.stdout)
            .trim()
            .to_string();

        let wt = create(&project, &ticket).expect("create worktree");

        std::fs::write(wt.path.join("worker-output.txt"), "delegated work\n")
            .expect("write in worktree");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&wt.path)
                .status()
                .expect("run git in worktree");
            assert!(status.success(), "git {args:?} failed in worktree");
        };
        run(&["add", "worker-output.txt"]);
        run(&["commit", "-q", "-m", "delegated worker commit"]);

        // On the worktree's own branch, the file is real.
        assert!(wt.path.join("worker-output.txt").exists());

        // On the main checkout's working tree, it never appeared.
        assert!(!project.root.join("worker-output.txt").exists());

        // And the main branch's own history was never touched by the worktree's commit.
        let log = Command::new("git")
            .args(["log", &main_branch, "--oneline", "--", "worker-output.txt"])
            .current_dir(&project.root)
            .output()
            .expect("git log main branch");
        assert!(
            String::from_utf8_lossy(&log.stdout).trim().is_empty(),
            "the delegated worker's commit must not reach the main branch's history"
        );

        let branch = wt.branch.clone();
        wt.cleanup();

        // `cleanup` (`git worktree remove`) deletes the checkout, not the branch — the branch is
        // the whole point: it is where the delegated worker's real output survives after the
        // scratch directory is gone. If cleanup ever started deleting the branch too, this is
        // the assertion that would catch it; without it, a regression here would still leave
        // "the file never appeared in the main checkout" true and pass unnoticed.
        let log_after_cleanup = Command::new("git")
            .args(["log", &branch, "--oneline", "--", "worker-output.txt"])
            .current_dir(&project.root)
            .output()
            .expect("git log the worktree's own branch after cleanup");
        assert!(
            !String::from_utf8_lossy(&log_after_cleanup.stdout)
                .trim()
                .is_empty(),
            "the worker's commit must still be reachable from its branch after cleanup removes \
             only the checkout"
        );
    }

    #[test]
    fn create_rejects_a_non_git_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Not a git repo at all: `Project::for_test` still opens a `.tm` store fine (the store
        // has no idea whether its workspace root is git-backed), so this exercises exactly the
        // precondition `create` itself must enforce.
        let state_dir = dir.path().join(".tm");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Arc::new(
            Store::open_with_at(&state_dir, clock.clone(), ids.clone()).expect("open store"),
        );
        let project = Project::for_test(dir.path(), store, clock, ids);
        let ticket = seed_ticket(&project);

        let err = create(&project, &ticket).expect_err("no git repo must be a clear error");
        assert!(matches!(err, TmError::Parse(_)));
    }

    #[test]
    fn create_rejects_a_repo_with_no_commits_yet() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());

        let state_dir = dir.path().join(".tm");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Arc::new(
            Store::open_with_at(&state_dir, clock.clone(), ids.clone()).expect("open store"),
        );
        let project = Project::for_test(dir.path(), store, clock, ids);
        let ticket = seed_ticket(&project);

        let err = create(&project, &ticket).expect_err("unborn HEAD must be a clear error");
        assert!(matches!(err, TmError::Parse(_)));
    }

    #[test]
    fn create_rejects_a_global_scope_project_even_inside_a_real_git_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write file");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);

        // Global-scope state, deliberately outside `dir` -- the same shape `resolve_scope`
        // produces when no repo-local `.tm/` exists yet. `Project::for_test` always hardcodes
        // `Scope::Repo`, so this builds the struct directly (every field is `pub`) rather than
        // adding a second test constructor for one test.
        let global_dir = tempfile::tempdir().expect("global tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Arc::new(
            Store::open_with_at(global_dir.path(), clock.clone(), ids.clone()).expect("open store"),
        );
        let project = Project {
            root: dir.path().to_path_buf(),
            state_dir: global_dir.path().to_path_buf(),
            scope: Scope::Global,
            store,
            clock,
            ids,
            actor: ParticipantId::new("human:test").expect("actor"),
        };
        let ticket = seed_ticket(&project);

        let err = create(&project, &ticket).expect_err("global scope must be rejected");
        assert!(matches!(err, TmError::Parse(_)));
    }
}
