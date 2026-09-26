//! `tm bench run --live`'s [`tm_harness::SeededProvider`] adapter.
//!
//! The default `tm bench run` path (`ops.rs`'s `FixtureScriptProvider`) replays a fixed,
//! on-disk transcript, so the same epoch always scores the same way. `--live` trades that
//! determinism for a genuine result: [`LiveSeededProvider`] copies a task's fixture into a
//! fresh scratch directory, creates and activates a real worker ticket for it, and drives that
//! ticket to completion through [`crate::sched::run_ticket`] -- the same dispatcher/executor
//! wiring `tm run` itself uses, never a reimplementation of it -- then scores the task by
//! actually running its `test_command`. See
//! `docs/decisions/D-032-live-benchmark-mode.md` for the cost and determinism tradeoffs this
//! makes.
//!
//! [`tm_harness::SeededProvider::step`] is synchronous (`tm-harness` has no async runtime
//! dependency), but driving a ticket to completion is inherently async. Calling
//! `tokio::runtime::Handle::block_on` from inside `step` would try to block the very worker
//! thread that is already driving `tm bench run --live`'s own async task, which is exactly the
//! "cannot start a runtime from within a runtime" panic `sched.rs`'s `dispatch_sched` doc
//! comment describes for the analogous `tm sched run` case. [`block_on_isolated`] sidesteps it
//! by spawning a plain OS thread with no ambient Tokio context and building a fresh, throwaway
//! runtime there instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use tm_events::{Event, EventLog};
use tm_harness::metrics::ticket_metrics_from_events;
use tm_harness::{BenchTask, SeededProvider};
use tm_types::Clock as _;

use crate::args::RunArgs;
use crate::project::{self, Project};
use crate::render::Renderer;

/// Run `fut` to completion on a fresh, single-use Tokio runtime on its own OS thread. See this
/// module's own doc comment for why: it lets a caller already inside another Tokio runtime's
/// async task synchronously drive an inner async call without tripping the nested-runtime panic.
fn block_on_isolated<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("building a throwaway tokio runtime for one live bench task run");
                runtime.block_on(fut)
            })
            .join()
            .expect("the live bench task thread panicked instead of returning a result")
    })
}

/// One task's real, ticket-metrics-derived numbers, recorded by [`LiveSeededProvider::step`] and
/// applied to the report after the whole suite has run
/// ([`LiveSeededProvider::apply_live_metrics`]). [`tm_harness::BenchRunner::run`]'s own
/// cost/tool-call accounting is a proxy derived from the scripted provider's output byte length,
/// which has no meaning once a task is driven by a real ticket run.
#[derive(Debug, Clone, Copy, Default)]
struct LiveMetrics {
    /// Real spend, folded from the scratch project's own `usage.recorded` events.
    dollars_micros: u64,
    /// Real tool-call count, folded from `tool_call.completed` events.
    tool_calls: u32,
    /// Real aggregate token count, folded from `usage.recorded` events. `TaskResult` has no
    /// dedicated tokens field, so [`LiveSeededProvider::apply_live_metrics`] stores this in
    /// `context_bytes`, the closest "how much did this run move" number the report has.
    tokens: u64,
}

/// Drives a [`BenchTask`] as one real ticket run instead of replaying a fixed script.
///
/// Not `Sync`-free-for-all reuse: this is built fresh in `ops::bench_run` for one `tm bench run
/// --live` invocation and handed to [`tm_harness::BenchRunner::run_all`] as a
/// `&dyn SeededProvider`, matching every other `SeededProvider` in this crate.
pub struct LiveSeededProvider<'a> {
    /// Where `bench/tasks/*.toml` and `bench/fixtures/*` resolve from (`<project.root>/bench`).
    bench_root: PathBuf,
    /// The invoking project -- not the per-task scratch project a task actually runs against.
    /// Only its `state_dir` (where each task's cassette lands, alongside this run's own report)
    /// is used.
    project: &'a Project,
    /// Passed through to [`crate::sched::run_ticket`] so a live task's own progress output joins
    /// `tm bench run --live`'s, instead of building a second, silent renderer.
    renderer: &'a Renderer,
    /// Real metrics recorded per task id during [`LiveSeededProvider::step`], applied to the
    /// report by [`LiveSeededProvider::apply_live_metrics`] once the whole suite has run. A task
    /// whose ticket run errored before any event was recorded simply has no entry.
    metrics: Mutex<BTreeMap<String, LiveMetrics>>,
}

impl<'a> LiveSeededProvider<'a> {
    /// Build a provider over `project`'s own `bench/` directory. `renderer` is reused as-is for
    /// every live task's ticket run.
    pub fn new(project: &'a Project, renderer: &'a Renderer) -> Self {
        LiveSeededProvider {
            bench_root: project.root.join("bench"),
            project,
            renderer,
            metrics: Mutex::new(BTreeMap::new()),
        }
    }

    /// Overwrite `report`'s cost/tool-call/context-bytes accounting for every task this provider
    /// actually ran live, using the real numbers `step` recorded. `score` is left exactly as
    /// [`tm_harness::BenchRunner::run`] computed it: recomputing it here would mean duplicating
    /// `tm-harness`'s own (private) scoring weights, and `passed` -- decided purely by the
    /// task's predicate against `step`'s returned text -- is what a live run is actually
    /// proving.
    pub fn apply_live_metrics(&self, report: &mut tm_harness::BenchmarkReport) {
        let recorded = self
            .metrics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for task_result in &mut report.tasks {
            if let Some(m) = recorded.get(&task_result.task_id) {
                task_result.cost_micros = m.dollars_micros;
                task_result.tool_calls = m.tool_calls;
                task_result.context_bytes = m.tokens;
            }
        }
    }

    /// Run `task` for real: copy its fixture into a fresh scratch directory, give that directory
    /// a real git history (so the dispatcher's code-intelligence history ingest has a valid
    /// `HEAD`), run `task.fixture.setup_commands`, create and activate a worker ticket for
    /// `task.task`, drive it through [`crate::sched::run_ticket`] with a cassette recorded next
    /// to this run's own report, then run `task.fixture.test_command` and report
    /// `tests_pass:<suite>` in the returned text only when it exits zero -- the one thing
    /// [`tm_harness::bench`]'s scripted-run predicate evaluation looks for. Real cost/tool-call/
    /// token numbers are folded from the scratch project's own event log and stashed for
    /// [`LiveSeededProvider::apply_live_metrics`] to apply afterward, per
    /// [`SeededProvider::step`]'s "output text only" contract.
    fn run_live_task(&self, task: &BenchTask) -> tm_types::Result<String> {
        let scratch_root = scratch_dir_for(self.project, &task.id)?;
        copy_dir_recursive(&self.bench_root.join(&task.fixture.path), &scratch_root)?;
        init_git_repo_with_a_commit(&scratch_root)?;

        for command in &task.fixture.setup_commands {
            if !run_command(&scratch_root, command)? {
                return Err(tm_types::TmError::storage(format!(
                    "live bench task {}: setup command `{}` failed",
                    task.id,
                    command.join(" ")
                )));
            }
        }

        let scratch_project = project::open(&scratch_root)?;
        let ticket = crate::tickets::create_worker_ticket(&scratch_project, &task.task)?;
        scratch_project
            .store
            .activate(&ticket, scratch_project.actor.clone())?;

        let cassette_dir = self.project.state_dir.join("bench").join("live");
        std::fs::create_dir_all(&cassette_dir)?;
        let cassette_path = cassette_dir.join(format!("{}.cassette.jsonl", task.id));

        let run_args = RunArgs {
            ticket: ticket.to_string(),
            role: None,
            worktree: false,
            record: Some(cassette_path),
            replay: None,
            strict_replay: false,
        };

        let run_outcome = block_on_isolated(crate::sched::run_ticket(
            &run_args,
            &scratch_project,
            self.renderer,
        ));

        let metrics = ticket_metrics_from_events(&ticket, &read_all_events(&scratch_project)?);
        self.metrics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                task.id.clone(),
                LiveMetrics {
                    dollars_micros: metrics.dollars_micros,
                    tool_calls: metrics.tool_calls,
                    tokens: metrics
                        .tokens_in
                        .saturating_add(metrics.tokens_out)
                        .saturating_add(metrics.tokens_total),
                },
            );

        if let Err(e) = run_outcome {
            self.renderer.note(&format!(
                "live bench task {} did not reach a finished ticket state: {e}",
                task.id
            ));
            return Ok(format!(
                "live bench task {} did not reach a finished ticket state: {e}",
                task.id
            ));
        }

        let Some(test_command) = &task.fixture.test_command else {
            return Ok(format!(
                "live bench task {} ran to completion (no test command configured)",
                task.id
            ));
        };
        let passed = run_command(&scratch_root, test_command)?;
        let mut output = format!(
            "live bench task {} ran; test command {}",
            task.id,
            if passed { "passed" } else { "failed" }
        );
        if passed {
            output.push('\n');
            output.push_str("tests_pass:default");
        }
        Ok(output)
    }
}

impl SeededProvider for LiveSeededProvider<'_> {
    fn step(&self, task: &BenchTask, step_index: u32) -> tm_types::Result<String> {
        if step_index != 0 {
            return Err(tm_types::TmError::invariant(
                "a live bench task runs exactly one step; the runner asked for a second one",
            ));
        }
        self.run_live_task(task)
    }

    fn is_finished(&self, _task: &BenchTask, step_index: u32) -> bool {
        step_index >= 1
    }
}

/// A fresh, unique scratch directory under the OS temp dir for one live task run -- never the
/// primary checkout or any worktree. `tm-bench-live-*` matches `disk-guard.sh`'s own `tm-*`
/// scratch-dir convention, so a directory left behind by an interrupted `tm bench run --live`
/// still gets reclaimed. Uniqueness comes from `project`'s own injected clock (never a raw
/// `SystemTime::now()`, which the hygiene check forbids outside `tm-types`'s clock substrate)
/// plus the process id, since two tasks in the same run never share a task id but a retried
/// invocation could otherwise collide on the same second.
fn scratch_dir_for(project: &Project, task_id: &str) -> tm_types::Result<PathBuf> {
    let unique = project.clock.now().to_rfc3339().replace([':', '.'], "-");
    let dir = std::env::temp_dir().join(format!(
        "tm-bench-live-{task_id}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Recursively copy every file under `from` to `to`, creating directories as needed. A small,
/// deliberate duplicate of `project.rs`'s private `copy_dir_recursive` rather than a shared
/// helper -- this crate's own convention (see `stats.rs`'s `read_all_events` doc comment) is
/// that each command module owns its own thin IO helpers rather than reaching into another
/// module's private ones.
fn copy_dir_recursive(from: &Path, to: &Path) -> tm_types::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Give `root` a real git history: `git init` plus one commit of whatever the fixture copy put
/// there, so the dispatcher's code-intelligence history ingest has a valid `HEAD` to read.
/// Matches the test suite's own `init_git_repo_with_a_commit` helper (`tests/worktree_run.rs`
/// and its siblings), duplicated here because this one runs from production code, outside any
/// test harness.
fn init_git_repo_with_a_commit(root: &Path) -> tm_types::Result<()> {
    let run = |args: &[&str]| -> tm_types::Result<()> {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .map_err(|e| {
                tm_types::TmError::storage(format!("git {args:?} failed to start: {e}"))
            })?;
        if !status.success() {
            return Err(tm_types::TmError::storage(format!(
                "git {args:?} exited non-zero in {}",
                root.display()
            )));
        }
        Ok(())
    };
    run(&["init", "--quiet", "--initial-branch=main"])?;
    run(&["config", "user.email", "bench-live@ticketmaster.local"])?;
    run(&["config", "user.name", "tm bench run --live"])?;
    run(&["add", "-A"])?;
    run(&[
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "live bench fixture",
    ])?;
    Ok(())
}

/// Run `argv` (never through a shell) in `cwd`, returning whether it exited zero. Used for both
/// `task.fixture.setup_commands` (the caller treats any failure as fatal to the task) and
/// `task.fixture.test_command` (the caller's own pass/fail signal).
fn run_command(cwd: &Path, argv: &[String]) -> tm_types::Result<bool> {
    let Some((program, args)) = argv.split_first() else {
        return Ok(true);
    };
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| {
            tm_types::TmError::storage(format!(
                "failed to run `{}` in {}: {e}",
                argv.join(" "),
                cwd.display()
            ))
        })?;
    Ok(status.success())
}

/// Read every event in `project`'s own log, oldest first. Mirrors `stats.rs`'s private
/// `read_all_events` -- not reusable from here for the same reason that module's own doc comment
/// gives: each command module reads the log through its own thin, independent handle.
fn read_all_events(project: &Project) -> tm_types::Result<Vec<Event>> {
    let db_path = project.state_dir.join("project.db");
    let log = EventLog::open_with_clock(&db_path, project.clock.clone())?;
    const BATCH: usize = 1024;
    let mut out = Vec::new();
    let mut seq = 1u64;
    loop {
        let batch = log.read_from(seq, BATCH)?;
        if batch.is_empty() {
            break;
        }
        seq += batch.len() as u64;
        out.extend(batch);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_dir_recursive_copies_nested_files() {
        let src_root =
            std::env::temp_dir().join(format!("tm-bench-live-test-src-{}", std::process::id()));
        let dst_root =
            std::env::temp_dir().join(format!("tm-bench-live-test-dst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src_root);
        let _ = std::fs::remove_dir_all(&dst_root);
        std::fs::create_dir_all(src_root.join("nested")).expect("create nested source dir");
        std::fs::write(src_root.join("top.txt"), b"top").expect("write top-level file");
        std::fs::write(src_root.join("nested/deep.txt"), b"deep").expect("write nested file");

        copy_dir_recursive(&src_root, &dst_root).expect("copy should succeed");

        assert_eq!(
            std::fs::read_to_string(dst_root.join("top.txt")).expect("read copied top file"),
            "top"
        );
        assert_eq!(
            std::fs::read_to_string(dst_root.join("nested/deep.txt"))
                .expect("read copied nested file"),
            "deep"
        );

        let _ = std::fs::remove_dir_all(&src_root);
        let _ = std::fs::remove_dir_all(&dst_root);
    }

    #[test]
    fn run_command_reports_exit_status() {
        let cwd = std::env::temp_dir();
        assert!(run_command(&cwd, &["true".to_string()]).expect("true should run"));
        assert!(!run_command(&cwd, &["false".to_string()]).expect("false should run"));
    }

    #[test]
    fn run_command_on_empty_argv_is_vacuously_true() {
        let cwd = std::env::temp_dir();
        assert!(run_command(&cwd, &[]).expect("empty argv never spawns a process"));
    }
}
