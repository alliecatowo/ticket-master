//! `cargo xtask bench-cross` (`mise run bench:cross`) — run the `bench/tasks/*.toml` suite
//! identically through `tm bench run`'s own live path and through configured external
//! coding-CLI tools, then report pass/fail, score, cost, tool calls and wall time side by side.
//! See `docs/decisions/D-033-cross-tool-benchmark.md` and `docs/backlog.md`'s "A head-to-head
//! benchmark" section (sketched at `docs/backlog.md:498-517`) for the shape this follows.
//!
//! `run_comparison` is the whole mechanism, parameterized over `ToolAdapter` so the same loop
//! drives every tool -- `tm` is not special-cased. Every real-tool `ToolAdapter` impl below
//! (`TmAdapter`, `ClaudeAdapter`, `OpencodeAdapter`, `CodexAdapter`) shells out to a real binary;
//! `mod tests`'s `FakeAdapter` is what unit tests drive `run_comparison` with instead, per
//! `SPEC.md` §0. That still leaves tests spawning `git` (to give each scratch fixture a real
//! history), the real `sleep` binary (to prove the `--task-timeout` process-kill path actually
//! kills a real, slow child) and the fixture's own `test_command` (e.g. `true`/`false`) locally --
//! none of those is a coding tool or a network call, the two things this module's own real
//! adapters are opt-in about. Real runs are opt-in from the CLI (`--tools`) and are never part of
//! `mise run verify` or `mise run hygiene` -- see `run`'s own doc comment.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tm_harness::BenchTask;

/// What one tool's run against a copied fixture produced, before scoring. `None` in any field
/// means the adapter's tool never reports that number -- `run_comparison` treats an unreported
/// number as `0` for scoring rather than failing the run over it, since pass/fail itself always
/// comes from independently re-running `task.fixture.test_command`, never from the adapter.
#[derive(Debug, Clone, Default)]
pub struct AdapterOutcome {
    /// Tool calls the adapter's tool reported for this run, when it reports one at all.
    pub tool_calls: Option<u32>,
    /// `tokens_in + tokens_out`, when the tool reports it.
    pub tokens: Option<u64>,
    /// Spend in micro-dollars, when the tool reports it.
    pub cost_micros: Option<u64>,
    /// The model the run actually served under, when the tool reports it (e.g. Claude's
    /// `--output-format json` result object, or tm's own `tm stats`). Recorded so a reader of
    /// the comparison report can tell "same task, unpinned/differing models" apart from a fair,
    /// model-controlled comparison, per `docs/audits/2026-09-25-bench-plan.md`'s "Required
    /// fixes" item 1.
    pub model: Option<String>,
    /// Set when the adapter's own process was killed for exceeding the configured
    /// `--task-timeout` rather than completing (successfully or not).
    pub timed_out: bool,
}

/// One coding tool under comparison. Implemented once per real tool (`TmAdapter` and friends,
/// below) and once for tests (`FakeAdapter`) -- `run_comparison` never distinguishes `tm`
/// from any other adapter.
pub trait ToolAdapter {
    /// Drive `task.task` against the fixture already copied into `workdir` (a fresh, git-backed
    /// scratch directory -- see `prepare_workdir`). Returning `Err` means the tool itself
    /// failed to run at all (a crashed process, a missing binary); `run_comparison` always
    /// scores that as a failed task rather than propagating the error and aborting the whole
    /// suite, so one tool's outage does not stop every other tool's run.
    fn run(&self, task: &BenchTask, workdir: &Path) -> Result<AdapterOutcome>;

    /// Human-readable description of the permission/sandbox posture this adapter runs its tool
    /// under (e.g. Claude Code's `--permission-mode bypassPermissions`, Codex's `--sandbox
    /// workspace-write`). Reported in `CrossToolReport::header` so a reader can confirm every
    /// tool ran under a comparable posture, rather than `tm` "winning" every task simply because
    /// the others couldn't edit files or run commands at all --
    /// `docs/audits/2026-09-25-bench-plan.md`'s "Required fixes" item 0. `mod tests`'s
    /// `FakeAdapter` keeps this default.
    fn permission_posture(&self) -> &str {
        "unspecified"
    }

    /// Best-effort `<binary> --version` (or equivalent), for the report header. `None` when the
    /// tool's version could not be read (missing binary, non-zero exit, non-UTF8 output) --
    /// purely informational, never affects scoring. `mod tests`'s `FakeAdapter` keeps this
    /// default so unit tests never spawn a real binary.
    fn version(&self) -> Option<String> {
        None
    }
}

/// One tool's result for one task, mirroring `tm_harness::TaskResult`'s externally visible
/// fields (the same shape `tm bench report` renders) plus a `tool` column, since this report
/// always compares more than one tool for the same task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossTaskResult {
    pub tool: String,
    pub task_id: String,
    pub passed: bool,
    pub score: f64,
    pub cost_micros: u64,
    pub tool_calls: u32,
    pub wall_seconds: u32,
    /// `None` when no adapter run for this (tool, task) pair reported a token count.
    pub tokens: Option<u64>,
    /// `None` when no adapter run for this (tool, task) pair reported which model served it.
    pub model: Option<String>,
    /// Set when this pair's adapter process was killed for exceeding `--task-timeout`, rather
    /// than completing. `passed` is always `false` in that case, and `render_markdown` shows
    /// `TIMEOUT` rather than `fail` in the `Result` column so the two aren't conflated.
    pub timed_out: bool,
}

/// The full comparison: every `(tool, task)` pair that ran, in run order, plus the run-level
/// metadata (permission posture, tool versions, the pinned model and any caps) needed to read
/// the numbers as a fair comparison rather than an unlabeled one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CrossToolReport {
    pub header: ReportHeader,
    pub results: Vec<CrossTaskResult>,
}

/// Run-level metadata rendered once, above the per-task table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportHeader {
    /// `(tool name, permission/sandbox posture)`, in adapter order -- see
    /// `ToolAdapter::permission_posture`.
    pub permission_postures: Vec<(String, String)>,
    /// `(tool name, `<binary> --version` output)`, in adapter order -- see
    /// `ToolAdapter::version`.
    pub tool_versions: Vec<(String, Option<String>)>,
    /// The model id passed via `--model`, when the caller pinned one.
    pub model: Option<String>,
    /// The per-task timeout, in seconds, when the caller set one via `--task-timeout`.
    pub task_timeout_seconds: Option<u64>,
    /// The total-spend cap, in micro-dollars, when the caller set one via `--max-cost-usd`.
    pub max_cost_micros: Option<u64>,
}

/// Caller-supplied caps/config for one `run_comparison` call. Pure data -- CLI flag parsing
/// happens in `run()`, so this can be constructed directly in tests.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// The model id the caller pinned via `--model`, recorded in the report header. Threading it
    /// into each adapter's own request is each adapter struct's own `model` field (set by
    /// `run()`'s construction), not this option -- this copy exists purely for the header.
    pub model: Option<String>,
    /// Per-task wall-clock timeout. Each real adapter enforces this itself (killing the whole
    /// process group -- see `run_with_timeout`); a test double that blocks in-process (like
    /// `mod tests`'s `SleepingAdapter`) has to opt into the same mechanism itself, since
    /// `run_comparison` has no way to interrupt an arbitrary blocking Rust call.
    pub task_timeout: Option<Duration>,
    /// Total spend cap, in micro-dollars. Once cumulative reported cost across every `(tool,
    /// task)` pair run so far reaches this, `run_comparison` stops scheduling further pairs
    /// (already-scheduled pairs still finish; this is checked between pairs, not mid-pair).
    pub max_cost_micros: Option<u64>,
}

/// Score of 1.0 while `actual` stays at or under `ceiling`, degrading toward 0.0 past it. A
/// `ceiling` of zero demands `actual == 0` for full credit. Duplicated from
/// `tm_harness::bench`'s private `ceiling_subscore` -- that function is not `pub`, and this
/// crate's own convention (`tm-cli/src/bench_live.rs`'s `copy_dir_recursive` doc comment) is that
/// each command module re-derives a small pure helper like this rather than reaching into
/// another module's private internals.
fn ceiling_subscore(actual: u64, ceiling: u64) -> f64 {
    if ceiling == 0 {
        return if actual == 0 { 1.0 } else { 0.0 };
    }
    (ceiling as f64 / actual.max(1) as f64).min(1.0)
}

/// Score `task`'s run the same way `tm_harness::bench::BenchRunner::run` weighs its dimensions,
/// minus `unnecessary_ops` (no adapter here can observe it, so its weight is treated as 0 credit
/// rather than guessed at).
fn score_task(
    task: &BenchTask,
    passed: bool,
    cost_micros: u64,
    wall_seconds: u32,
    tool_calls: u32,
) -> f64 {
    let scoring = &task.scoring;
    let expected = &task.expected;
    let success_score = if passed { 1.0 } else { 0.0 };
    let cost_score = ceiling_subscore(cost_micros, expected.max_cost_micros);
    let latency_score = ceiling_subscore(wall_seconds as u64, expected.max_wall_seconds as u64);
    let tool_count_score = ceiling_subscore(tool_calls as u64, expected.max_tool_calls as u64);

    scoring.success_weight * success_score
        + scoring.cost_weight * cost_score
        + scoring.latency_weight * latency_score
        + scoring.tool_count_weight * tool_count_score
}

/// Recursively copy every file under `from` to `to`, creating directories as needed. A small,
/// deliberate duplicate of `tm-cli/src/bench_live.rs`'s private `copy_dir_recursive` -- see that
/// function's doc comment for why this crate re-derives its own IO helpers instead of reusing
/// another module's private ones.
fn copy_dir_recursive(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)
        .with_context(|| format!("reading fixture dir {}", from.display()))?
    {
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
/// there, so a tool that expects a real repo (every one of the four does) has one. Matches
/// `tm-cli/src/bench_live.rs`'s `init_git_repo_with_a_commit`, duplicated for the same reason as
/// `copy_dir_recursive`.
fn init_git_repo_with_a_commit(root: &Path) -> Result<()> {
    let run = |args: &[&str]| -> Result<()> {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .with_context(|| format!("git {args:?} failed to start in {}", root.display()))?;
        if !status.success() {
            bail!("git {args:?} exited non-zero in {}", root.display());
        }
        Ok(())
    };
    run(&["init", "--quiet", "--initial-branch=main"])?;
    run(&["config", "user.email", "bench-cross@ticketmaster.local"])?;
    run(&["config", "user.name", "tm bench cross"])?;
    run(&["add", "-A"])?;
    run(&[
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "bench-cross fixture",
    ])?;
    Ok(())
}

/// Run `argv` (never through a shell) in `cwd`, returning whether it exited zero. `Ok(true)` on
/// an empty `argv`: nothing to fail.
fn run_command(cwd: &Path, argv: &[String]) -> Result<bool> {
    let Some((program, args)) = argv.split_first() else {
        return Ok(true);
    };
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to run `{}` in {}", argv.join(" "), cwd.display()))?;
    Ok(status.success())
}

/// Copy `task`'s fixture (resolved under `bench_root`) into a fresh directory under
/// `workdir_root`, give it a real git history, and run its `setup_commands`. Fails the task
/// outright (matching `tm bench run --live`'s own behavior) if a setup command exits non-zero.
fn prepare_workdir(bench_root: &Path, task: &BenchTask, workdir_root: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(workdir_root)?;
    copy_dir_recursive(&bench_root.join(&task.fixture.path), workdir_root)?;
    init_git_repo_with_a_commit(workdir_root)?;
    for command in &task.fixture.setup_commands {
        if !run_command(workdir_root, command)? {
            bail!(
                "task {}: setup command `{}` failed",
                task.id,
                command.join(" ")
            );
        }
    }
    Ok(workdir_root.to_path_buf())
}

/// Run `task.fixture.test_command` in `workdir`. `Ok(true)` when there is no test command at all
/// (nothing to fail against).
fn run_test_command(workdir: &Path, task: &BenchTask) -> Result<bool> {
    match &task.fixture.test_command {
        Some(cmd) => run_command(workdir, cmd),
        None => Ok(true),
    }
}

/// Outcome of `run_with_timeout`: either the command finished (with its captured output) or it
/// was killed for running past the configured timeout.
enum TimedOutput {
    Finished(std::process::Output),
    TimedOut,
}

/// Spawn `cmd` (already configured with program, args, cwd and stdin -- stdout/stderr are always
/// overridden to piped here, matching what `Command::output()` would have done) in its own
/// process group and wait for it, killing the *whole group* with `SIGKILL` if it runs past
/// `timeout`. `None` waits unboundedly, matching every adapter's behavior before `--task-timeout`
/// existed. The whole group, not just the direct child, because several of these tools (Codex,
/// OpenCode) spawn their own subprocesses, and killing only the direct child would leave those
/// running.
///
/// Used by every real adapter (`TmAdapter`, `ClaudeAdapter`, `OpencodeAdapter`, `CodexAdapter`)
/// and, in tests, by a `SleepingAdapter` that spawns the real `sleep` binary -- so the timeout
/// tests exercise this actual process-kill path, not a simulation of it.
fn run_with_timeout(mut cmd: Command, timeout: Option<Duration>) -> Result<TimedOutput> {
    prepare_process_group(&mut cmd);
    // `Command::spawn` alone inherits the parent's stdout/stderr; every caller here wants the
    // captured-`Output` behavior `Command::output()` gives, just with a bounded wait instead of
    // an unconditional one -- without this, every adapter would get empty stdout back from a
    // real run (caught by `claude_adapter_completes_when_the_tool_blocks_on_stdin` below).
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().context("failed to spawn command")?;
    let Some(timeout) = timeout else {
        let output = child.wait_with_output().context("waiting for command")?;
        return Ok(TimedOutput::Finished(output));
    };

    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    // Detached, not scoped: `std::thread::scope` joins every spawned thread before returning,
    // which would defeat the point of a timeout (we'd still block until the child exits). This
    // thread outlives a timed-out call; it simply drops its result into a channel nobody is
    // listening to anymore once the killed child's `wait_with_output` finally returns.
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => Ok(TimedOutput::Finished(output)),
        Ok(Err(e)) => Err(e).context("waiting for command"),
        Err(RecvTimeoutError::Timeout) => {
            kill_process_group(pid);
            Ok(TimedOutput::TimedOut)
        }
        Err(RecvTimeoutError::Disconnected) => {
            bail!("command wait thread ended without sending a result")
        }
    }
}

#[cfg(unix)]
fn prepare_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // New process group whose pgid equals the child's own pid, so `kill_process_group` can
    // target the whole tree via `-pid` rather than only the direct child.
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn prepare_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // SAFETY: `pid` is a child this same call put in its own new process group via
    // `process_group(0)` (pgid == pid), so `-pid` addresses exactly that group. Sending SIGKILL
    // to an already-exited group is a harmless ESRCH, not a hazard, and this never touches any
    // process this module didn't spawn itself.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Run every task in `tasks` through every `(tool name, adapter)` pair in `adapters`, scoring
/// each with that task's own `ScoringSpec`/`ExpectedOutcome`. `bench_root` resolves
/// `task.fixture.path`; `workdir_root` is where each per-tool, per-task scratch copy is made
/// (`<workdir_root>/<tool>/<task id>`) -- a caller-supplied `mktemp` directory in production, a
/// `tempfile::TempDir` in tests.
///
/// One `(tool, task)` pair's adapter failure (`ToolAdapter::run` returning `Err`, or its own
/// `Command` failing to start) is scored as a failed run for that pair and does not stop the rest
/// of the suite -- a missing or misconfigured external CLI should not hide every other tool's
/// result. `options.max_cost_micros`, once reached by cumulative reported spend, stops scheduling
/// further pairs outright (see `RunOptions::max_cost_micros`).
pub fn run_comparison(
    tasks: &[BenchTask],
    bench_root: &Path,
    adapters: &[(&str, &dyn ToolAdapter)],
    workdir_root: &Path,
    options: &RunOptions,
) -> Result<CrossToolReport> {
    let header = ReportHeader {
        permission_postures: adapters
            .iter()
            .map(|&(name, adapter)| (name.to_string(), adapter.permission_posture().to_string()))
            .collect(),
        tool_versions: adapters
            .iter()
            .map(|&(name, adapter)| (name.to_string(), adapter.version()))
            .collect(),
        model: options.model.clone(),
        task_timeout_seconds: options.task_timeout.map(|d| d.as_secs()),
        max_cost_micros: options.max_cost_micros,
    };

    let mut results = Vec::with_capacity(tasks.len() * adapters.len());
    let mut cumulative_cost_micros: u64 = 0;
    'tasks: for task in tasks {
        for &(tool_name, adapter) in adapters {
            let task_workdir = workdir_root.join(tool_name).join(&task.id);
            let prepared = prepare_workdir(bench_root, task, &task_workdir);
            let workdir = match prepared {
                Ok(dir) => dir,
                Err(e) => {
                    results.push(CrossTaskResult {
                        tool: tool_name.to_string(),
                        task_id: task.id.clone(),
                        passed: false,
                        score: 0.0,
                        cost_micros: 0,
                        tool_calls: 0,
                        wall_seconds: 0,
                        tokens: None,
                        model: None,
                        timed_out: false,
                    });
                    eprintln!("bench-cross: {tool_name}/{}: {e:?}", task.id);
                    continue;
                }
            };

            let start = Instant::now();
            let outcome = adapter.run(task, &workdir);
            let wall_seconds = start.elapsed().as_secs() as u32;

            let (tool_calls, tokens, model, cost_micros, adapter_ran, timed_out) = match &outcome {
                Ok(o) if o.timed_out => (0, None, o.model.clone(), 0, false, true),
                Ok(o) => (
                    o.tool_calls.unwrap_or(0),
                    o.tokens,
                    o.model.clone(),
                    o.cost_micros.unwrap_or(0),
                    true,
                    false,
                ),
                Err(e) => {
                    eprintln!("bench-cross: {tool_name}/{} adapter failed: {e:?}", task.id);
                    (0, None, None, 0, false, false)
                }
            };

            let passed =
                adapter_ran && !timed_out && run_test_command(&workdir, task).unwrap_or(false);
            let score = score_task(task, passed, cost_micros, wall_seconds, tool_calls);

            results.push(CrossTaskResult {
                tool: tool_name.to_string(),
                task_id: task.id.clone(),
                passed,
                score,
                cost_micros,
                tool_calls,
                wall_seconds,
                tokens,
                model,
                timed_out,
            });

            cumulative_cost_micros = cumulative_cost_micros.saturating_add(cost_micros);
            if let Some(cap) = options.max_cost_micros {
                if cumulative_cost_micros >= cap {
                    eprintln!(
                        "bench-cross: cumulative spend ${:.4} reached the --max-cost-usd cap \
                         ${:.4}; stopping",
                        cumulative_cost_micros as f64 / 1_000_000.0,
                        cap as f64 / 1_000_000.0,
                    );
                    break 'tasks;
                }
            }
        }
    }
    Ok(CrossToolReport { header, results })
}

/// Render `report` as markdown: one table, grouped by task, each row a `(tool, task)` pair --
/// the same pass/fail-score-cost-tool_calls-wall_time columns `tm-cli/src/bench_report.rs`'s
/// `render_report_markdown` renders for a single-tool `BenchmarkReport`, with a leading `Tool`
/// column since this report always compares more than one.
pub fn render_markdown(report: &CrossToolReport) -> String {
    let mut out = String::from("# Cross-tool benchmark report\n\n");
    out.push_str(&render_header(&report.header));
    if report.results.is_empty() {
        out.push_str("No tasks ran.\n");
        return out;
    }
    out.push_str(
        "| Tool | Task | Model | Result | Score | Cost | Tokens | Tool calls | Wall time |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
    for r in &report.results {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {:.2} | ${:.4} | {} | {} | {}s |\n",
            r.tool,
            r.task_id,
            r.model.as_deref().unwrap_or("-"),
            if r.timed_out {
                "TIMEOUT"
            } else if r.passed {
                "pass"
            } else {
                "fail"
            },
            r.score,
            r.cost_micros as f64 / 1_000_000.0,
            r.tokens
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string()),
            r.tool_calls,
            r.wall_seconds,
        ));
    }
    out
}

/// Render the run-level metadata block (model/timeout/cost cap, tool versions, permission
/// postures) that precedes the per-task table, so a reader sees *how* each tool ran and under
/// what constraints before the numbers, not just the numbers. Empty when there is nothing to
/// report (e.g. `CrossToolReport::default()`).
fn render_header(header: &ReportHeader) -> String {
    let mut out = String::new();

    if header.model.is_some()
        || header.task_timeout_seconds.is_some()
        || header.max_cost_micros.is_some()
    {
        if let Some(model) = &header.model {
            out.push_str(&format!("Model: {model}\n"));
        }
        if let Some(secs) = header.task_timeout_seconds {
            out.push_str(&format!("Task timeout: {secs}s\n"));
        }
        if let Some(cap) = header.max_cost_micros {
            out.push_str(&format!("Max cost cap: ${:.4}\n", cap as f64 / 1_000_000.0));
        }
        out.push('\n');
    }

    if !header.tool_versions.is_empty() {
        out.push_str("| Tool | Version |\n| --- | --- |\n");
        for (tool, version) in &header.tool_versions {
            out.push_str(&format!(
                "| {tool} | {} |\n",
                version.as_deref().unwrap_or("unknown")
            ));
        }
        out.push('\n');
    }

    if !header.permission_postures.is_empty() {
        out.push_str("| Tool | Permission posture |\n| --- | --- |\n");
        for (tool, posture) in &header.permission_postures {
            out.push_str(&format!("| {tool} | {posture} |\n"));
        }
        out.push('\n');
    }

    out
}

/// Shells the real `tm` binary against `workdir` (a fresh, git-backed scratch directory): `tm
/// init` first -- `TicketCommand::Dispatch` (unlike `TicketCommand::New`) requires an *existing*
/// project (`main.rs`'s `dispatch` only bootstraps one via `open_bare` for `ticket new`, and that
/// bootstrap is global-scope, not rooted at `workdir`, which would leave the ticket unable to see
/// the fixture at all), so this adapter explicitly creates a repo-scoped project at `workdir`
/// first, matching what `tm bench run --live`'s own scratch directory gets internally (D-032).
/// Then `tm ticket dispatch <task>` for a real, real-event-log-backed ticket (draft -> ready in
/// one step), `tm run <ticket>` to drive it through the same dispatcher every other `tm run`
/// invocation uses, then `tm stats --by ticket --ticket <ticket> --json` for the real
/// `tm_harness::metrics::TicketMetrics` numbers `tm` itself already folds from that project's
/// event log (`docs/decisions/D-030-local-telemetry.md`) -- no separate bookkeeping invented
/// here. Credentials: whatever `workdir`'s own environment and `providers.toml` resolve (DevPass
/// when `DEVPASS_API_KEY`/`DEVPASS_BASE_URL` are exported in that environment, per
/// `docs/decisions/D-005-devpass-default-provider.md`'s `coder.fast`-role pattern) -- this
/// adapter does not itself pin a provider or copy any credential into `workdir`.
pub struct TmAdapter {
    /// Path to (or bare name of) the `tm` binary to shell out to.
    pub binary: String,
    /// `provider/model` id to pin `workdir`'s scratch project to, when the caller set one via
    /// `--model` -- written into `.tm/providers.toml`'s `coder.fast` role candidate right after
    /// `tm init` (see `pin_model`); `Role::CoderFast` is the role `crates/tm-cli/src/agent.rs`'s
    /// `AGENT_ROLE` actually uses for a worker ticket's attempts, per `docs/providers.md`.
    pub model: Option<String>,
    /// Per-`tm` subprocess-call timeout. Applied to each individual `tm` invocation inside
    /// `run()` (init/dispatch/run/stats), not as one budget across all four -- a real deadline
    /// across the whole sequence would need tracking remaining budget between calls, which this
    /// keeps out of scope; what this does guarantee is that no single `tm` call can hang forever.
    pub timeout: Option<Duration>,
}

/// Marks a `tm_json` failure as "the subprocess was killed for exceeding `--task-timeout`"
/// rather than an ordinary tool failure, so `TmAdapter::run` can report `timed_out` distinctly
/// instead of a generic adapter error.
#[derive(Debug)]
struct AdapterTimedOut;

impl std::fmt::Display for AdapterTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "command exceeded the configured --task-timeout and was killed"
        )
    }
}

impl std::error::Error for AdapterTimedOut {}

fn is_adapter_timeout(e: &anyhow::Error) -> bool {
    e.downcast_ref::<AdapterTimedOut>().is_some()
}

impl TmAdapter {
    /// Run `tm <args>` in `workdir` with `--json`, returning stdout on success. Returns
    /// `Err(AdapterTimedOut)` (checkable with `is_adapter_timeout`) rather than an ordinary error
    /// when the call itself was killed for exceeding `self.timeout`.
    fn tm_json(&self, workdir: &Path, args: &[&str]) -> Result<String> {
        let mut full_args = vec!["--json"];
        full_args.extend_from_slice(args);
        let mut cmd = Command::new(&self.binary);
        cmd.args(&full_args)
            .current_dir(workdir)
            // `tm` never expects to read from stdin here (every one of these calls is
            // non-interactive), and an inherited *open* stdin pipe (e.g. this xtask itself
            // running under a CI runner or another tool's pipe) can make a child that probes
            // stdin block indefinitely rather than treating "nothing there" as EOF -- the same
            // hang this closes off for the other three adapters below.
            .stdin(Stdio::null());
        match run_with_timeout(cmd, self.timeout)? {
            TimedOutput::TimedOut => bail!(AdapterTimedOut),
            TimedOutput::Finished(output) => {
                if !output.status.success() {
                    bail!(
                        "tm {} exited non-zero: {}",
                        full_args.join(" "),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            }
        }
    }

    /// Overwrite `workdir/.tm/providers.toml`'s `coder.fast` role candidate to pin `model` (a
    /// `provider/model`-shaped id, e.g. `anthropic/claude-sonnet-5`), preserving every other role
    /// `tm init` already wrote (`reviewer`, `decider`, etc. would otherwise be left with zero
    /// candidates and become unroutable for the rest of this run -- `RoleTable::parse` accepts a
    /// partial table with no error, so a naive from-scratch rewrite would silently break the
    /// ticket's other role lookups instead of failing loudly). Deliberately drops the `[meta]`
    /// table so the file reads as a hand edit (`generated` reads `false`), which the role-table
    /// loader never overwrites from `DEVPASS_*`/other env-detected credentials -- see
    /// `docs/providers.md`'s "`providers.toml` follows the environment, not just `tm init` time".
    ///
    /// The pinned candidate has no `price` (unlike `docs/providers.md`'s documented candidates),
    /// so real spend against it is not attributed for `--max-cost-usd` purposes -- see
    /// `docs/decisions/D-033-cross-tool-benchmark.md`'s costs section.
    fn pin_model(&self, workdir: &Path, model: &str) -> Result<()> {
        let (provider, model_id) = model.split_once('/').with_context(|| {
            format!("--model `{model}` must be `provider/model`-shaped for tm, e.g. `anthropic/claude-sonnet-5`")
        })?;

        let providers_path = workdir.join(".tm").join("providers.toml");
        let existing = std::fs::read_to_string(&providers_path).unwrap_or_default();
        let mut root: toml::Value = if existing.trim().is_empty() {
            toml::Value::Table(Default::default())
        } else {
            toml::from_str(&existing)
                .with_context(|| format!("parsing existing {}", providers_path.display()))?
        };
        let top = root
            .as_table_mut()
            .context("providers.toml root must be a table")?;
        top.remove("meta");

        let mut candidate = toml::value::Table::new();
        candidate.insert(
            "provider".to_string(),
            toml::Value::String(provider.to_string()),
        );
        candidate.insert(
            "model".to_string(),
            toml::Value::String(model_id.to_string()),
        );
        candidate.insert("max_concurrency".to_string(), toml::Value::Integer(5));
        let mut fast = toml::value::Table::new();
        fast.insert(
            "candidates".to_string(),
            toml::Value::Array(vec![toml::Value::Table(candidate)]),
        );

        let coder = top
            .entry("coder")
            .or_insert_with(|| toml::Value::Table(Default::default()));
        coder
            .as_table_mut()
            .context("providers.toml's `coder` entry must be a table")?
            .insert("fast".to_string(), toml::Value::Table(fast));

        let serialized =
            toml::to_string_pretty(&root).context("serializing providers.toml model override")?;
        std::fs::write(&providers_path, serialized).context("writing providers.toml model override")
    }
}

impl ToolAdapter for TmAdapter {
    fn run(&self, task: &BenchTask, workdir: &Path) -> Result<AdapterOutcome> {
        // `ticket dispatch` needs an existing project; bootstrap a repo-scoped one at `workdir`
        // itself first (see this struct's own doc comment for why `ticket new`'s auto-bootstrap
        // doesn't help here).
        if let Err(e) = self.tm_json(workdir, &["init"]) {
            if is_adapter_timeout(&e) {
                return Ok(AdapterOutcome {
                    timed_out: true,
                    ..Default::default()
                });
            }
            return Err(e);
        }

        if let Some(model) = &self.model {
            self.pin_model(workdir, model)?;
        }

        let dispatch_out = match self.tm_json(workdir, &["ticket", "dispatch", &task.task]) {
            Ok(out) => out,
            Err(e) if is_adapter_timeout(&e) => {
                return Ok(AdapterOutcome {
                    timed_out: true,
                    ..Default::default()
                })
            }
            Err(e) => return Err(e),
        };
        // `TicketId` serializes as a bare JSON string, e.g. `"T-12"`.
        let ticket_id = dispatch_out.trim_matches('"').to_string();
        if ticket_id.is_empty() {
            bail!("`tm ticket dispatch` printed no ticket id");
        }

        // `tm run <ticket>` reports a failed attempt as a non-zero exit; still worth reading
        // whatever stats got recorded, so don't bail out before the `tm stats` call below --
        // unless it was killed for the timeout, in which case there is nothing left to read.
        let run_result = self.tm_json(workdir, &["run", &ticket_id]);
        if let Err(e) = &run_result {
            if is_adapter_timeout(e) {
                return Ok(AdapterOutcome {
                    timed_out: true,
                    ..Default::default()
                });
            }
            eprintln!("bench-cross: tm run {ticket_id} did not finish cleanly: {e:?}");
        }

        let stats_out = match self.tm_json(
            workdir,
            &["stats", "--by", "ticket", "--ticket", &ticket_id],
        ) {
            Ok(out) => out,
            Err(e) if is_adapter_timeout(&e) => {
                return Ok(AdapterOutcome {
                    timed_out: true,
                    ..Default::default()
                })
            }
            Err(e) => return Err(e),
        };
        let rows: Vec<tm_harness::metrics::TicketMetrics> = serde_json::from_str(&stats_out)
            .with_context(|| format!("parsing `tm stats` output: {stats_out}"))?;

        match rows.into_iter().next() {
            Some(m) => Ok(AdapterOutcome {
                tool_calls: Some(m.tool_calls),
                tokens: Some(m.tokens_in.saturating_add(m.tokens_out)),
                cost_micros: Some(m.dollars_micros),
                // `TicketMetrics` (the pure fold `tm stats` renders from) doesn't carry a model
                // column -- it attributes cost/tokens per (provider, model) pair only under
                // `tm stats --by model`, not per-ticket, so this reports back the model this
                // adapter was told to pin (if any) rather than something read from `tm` itself.
                model: self.model.clone(),
                timed_out: false,
            }),
            None => Ok(AdapterOutcome::default()),
        }
    }

    fn permission_posture(&self) -> &str {
        "n/a -- runs entirely inside the scratch project's own ticket/attempt loop; no external \
         sandbox flag applies"
    }

    fn version(&self) -> Option<String> {
        binary_version(&self.binary, &["--version"])
    }
}

/// Shells Claude Code's own scriptable one-shot turn: `claude -p <task> --output-format json
/// --permission-mode bypassPermissions`. `bypassPermissions` (not `dontAsk`, which denies rather
/// than approves every prompt -- confirmed against Claude Code's own `--help`, see
/// `docs/audits/2026-09-25-bench-plan.md`'s "Required fixes" item 0) auto-approves Edit/Write/Bash
/// so a headless run can actually touch the fixture; it is only ever pointed at `workdir`, a
/// disposable per-task scratch copy (see `prepare_workdir`), never the primary checkout. Real-
/// Claude auth (the metered Anthropic API, or an interactive subscription session) is never the
/// accidental default -- per `docs/backlog.md`'s "A head-to-head benchmark" section, this adapter
/// is only ever constructed by `run` when `--real-claude-auth` was passed explicitly; see that
/// function's doc comment for the gate itself.
pub struct ClaudeAdapter {
    pub binary: String,
    /// Model id passed through to `--model` verbatim, when the caller pinned one.
    pub model: Option<String>,
    /// Per-call wall-clock timeout, enforced by killing the whole process group -- see
    /// `run_with_timeout`.
    pub timeout: Option<Duration>,
}

impl ClaudeAdapter {
    /// Build the `Command` this adapter would run for `task` in `workdir`, without spawning it --
    /// factored out so a unit test can inspect the argv (`Command::get_args`) directly instead of
    /// needing a real `claude` binary on `$PATH`.
    fn build_command(&self, task: &BenchTask, workdir: &Path) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p")
            .arg(&task.task)
            .arg("--output-format")
            .arg("json")
            .arg("--permission-mode")
            .arg("bypassPermissions");
        if let Some(model) = &self.model {
            // Claude Code's `--model` takes a bare alias or model id (`claude --help`: "Provide
            // an alias for the latest model ... or a model's full name"), not a `provider/model`
            // pair -- strip any `provider/` prefix the caller's `--model` carried.
            cmd.arg("--model").arg(bare_model_id(model));
        }
        cmd.current_dir(workdir);
        // Closed, not just unpiped: `claude -p` probes stdin for piped input and prints "no
        // stdin data received in 3s" then hangs when it's an open pipe with nothing written to
        // it (the real failure this fixes -- see this module's own doc comment).
        cmd.stdin(Stdio::null());
        cmd
    }
}

impl ToolAdapter for ClaudeAdapter {
    fn run(&self, task: &BenchTask, workdir: &Path) -> Result<AdapterOutcome> {
        match run_with_timeout(self.build_command(task, workdir), self.timeout)
            .context("failed to run `claude -p`")?
        {
            TimedOutput::TimedOut => Ok(AdapterOutcome {
                timed_out: true,
                ..Default::default()
            }),
            // Best-effort: `claude -p --output-format json` prints one JSON result object with a
            // `usage`/`cost_usd`/`num_turns`-shaped payload. Field names are not pinned by this
            // crate (no dependency on Claude Code's own output schema), so a shape it doesn't
            // recognize degrades to "ran, but no metrics reported" rather than a hard error --
            // pass/fail always comes from re-running the task's own `test_command`, never from
            // this.
            TimedOutput::Finished(output) => Ok(parse_best_effort_usage(&String::from_utf8_lossy(
                &output.stdout,
            ))),
        }
    }

    fn permission_posture(&self) -> &str {
        "--permission-mode bypassPermissions (auto-approves Edit/Write/Bash except rm/rmdir on a \
         handful of critical paths)"
    }

    fn version(&self) -> Option<String> {
        binary_version(&self.binary, &["--version"])
    }
}

/// Shells `opencode run <task> --format json --auto` (OpenCode's non-interactive mode; `--auto`
/// auto-approves any permission request not explicitly denied, confirmed against `opencode run
/// --help`).
pub struct OpencodeAdapter {
    pub binary: String,
    /// `provider/model`-shaped id passed to `-m`, when the caller pinned one.
    pub model: Option<String>,
    /// Per-call wall-clock timeout, enforced by killing the whole process group -- see
    /// `run_with_timeout`.
    pub timeout: Option<Duration>,
}

impl OpencodeAdapter {
    /// Build the `Command` this adapter would run, without spawning it -- see
    /// `ClaudeAdapter::build_command`'s doc comment for why.
    fn build_command(&self, task: &BenchTask, workdir: &Path) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("run")
            .arg(&task.task)
            .arg("--format")
            .arg("json")
            .arg("--auto");
        if let Some(model) = &self.model {
            cmd.arg("-m").arg(model);
        }
        cmd.current_dir(workdir);
        // `opencode run` hung for the full timeout against an open stdin pipe and finished in
        // 14s against `/dev/null` (docs/audits/2026-09-25-bench-plan.md's evidence) -- close it
        // explicitly rather than inheriting whatever the caller's stdin is.
        cmd.stdin(Stdio::null());
        cmd
    }
}

impl ToolAdapter for OpencodeAdapter {
    fn run(&self, task: &BenchTask, workdir: &Path) -> Result<AdapterOutcome> {
        match run_with_timeout(self.build_command(task, workdir), self.timeout)
            .context("failed to run `opencode run`")?
        {
            TimedOutput::TimedOut => Ok(AdapterOutcome {
                timed_out: true,
                ..Default::default()
            }),
            TimedOutput::Finished(output) => Ok(parse_best_effort_usage(&String::from_utf8_lossy(
                &output.stdout,
            ))),
        }
    }

    fn permission_posture(&self) -> &str {
        "--auto (auto-approves any permission request not explicitly denied)"
    }

    fn version(&self) -> Option<String> {
        binary_version(&self.binary, &["--version"])
    }
}

/// Shells `codex exec <task> --json --sandbox workspace-write` (Codex CLI's non-interactive
/// mode; streams newline-delimited JSON events to stdout, one per state change).
/// `workspace-write` is the minimum sandbox posture that lets Codex actually edit files and run
/// commands inside `workdir` headlessly -- Codex's default "never approve" policy otherwise
/// leaves it unable to touch the fixture at all (docs/audits/2026-09-25-bench-plan.md's
/// "Required fixes" item 0); the fully-open `--dangerously-bypass-approvals-and-sandbox` is
/// deliberately not used here since this adapter runs against a real scratch directory on this
/// machine, not a disposable container.
pub struct CodexAdapter {
    pub binary: String,
    /// Model id, when the caller pinned one -- passed to `-m` with any `provider/` prefix
    /// stripped (Codex's `-m` takes a bare model id, not a `provider/model` pair).
    pub model: Option<String>,
    /// Per-call wall-clock timeout, enforced by killing the whole process group -- see
    /// `run_with_timeout`.
    pub timeout: Option<Duration>,
}

impl CodexAdapter {
    /// Build the `Command` this adapter would run, without spawning it -- see
    /// `ClaudeAdapter::build_command`'s doc comment for why.
    fn build_command(&self, task: &BenchTask, workdir: &Path) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("exec")
            .arg(&task.task)
            .arg("--json")
            .arg("--sandbox")
            .arg("workspace-write");
        if let Some(model) = &self.model {
            cmd.arg("-m").arg(bare_model_id(model));
        }
        cmd.current_dir(workdir);
        cmd.stdin(Stdio::null());
        cmd
    }
}

impl ToolAdapter for CodexAdapter {
    fn run(&self, task: &BenchTask, workdir: &Path) -> Result<AdapterOutcome> {
        match run_with_timeout(self.build_command(task, workdir), self.timeout)
            .context("failed to run `codex exec`")?
        {
            TimedOutput::TimedOut => Ok(AdapterOutcome {
                timed_out: true,
                ..Default::default()
            }),
            TimedOutput::Finished(output) => Ok(parse_best_effort_usage(&String::from_utf8_lossy(
                &output.stdout,
            ))),
        }
    }

    fn permission_posture(&self) -> &str {
        "--sandbox workspace-write (never-approve policy, sandboxed to the scratch working dir)"
    }

    fn version(&self) -> Option<String> {
        binary_version(&self.binary, &["--version"])
    }
}

/// Scrape whatever `usage`/`tokens`/`tool_calls`/`cost`-shaped numbers a tool's JSON output
/// happens to contain, without depending on any one tool's exact schema (each of the three
/// external CLIs' JSON output shape is sourced from that tool's own docs, not from this repo, and
/// can drift without notice). Looks for the last JSON object or JSON-lines record in `text` and
/// reads a handful of commonly-named fields; returns an all-`None` `AdapterOutcome` rather than
/// an error when nothing recognizable is found, since this is inherently best-effort -- see each
/// adapter's own doc comment.
fn parse_best_effort_usage(text: &str) -> AdapterOutcome {
    let mut outcome = AdapterOutcome::default();
    for line in text.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        outcome.tool_calls = find_u32(&value, &["tool_calls", "num_tool_calls", "toolCalls"]);
        outcome.tokens =
            find_u64(&value, &["tokens", "total_tokens", "totalTokens"]).or_else(|| {
                let usage = value.get("usage")?;
                let input = find_u64(usage, &["input_tokens", "prompt_tokens"])?;
                let output = find_u64(usage, &["output_tokens", "completion_tokens"]).unwrap_or(0);
                Some(input + output)
            });
        outcome.cost_micros = find_u64(&value, &["cost_micros"]).or_else(|| {
            value
                .get("cost_usd")
                .or_else(|| value.get("total_cost_usd"))
                .and_then(|v| v.as_f64())
                .map(|dollars| (dollars * 1_000_000.0).round() as u64)
        });
        outcome.model = find_str(&value, &["model", "model_id", "modelId"]);
        break;
    }
    outcome
}

fn find_u32(value: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|k| value.get(*k))
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
}

fn find_u64(value: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|k| value.get(*k))
        .and_then(|v| v.as_u64())
}

fn find_str(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| value.get(*k))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Strip any `provider/` prefix from a `--model` value, for tools whose own `--model`/`-m` flag
/// takes a bare model id rather than a `provider/model` pair (Claude Code, Codex -- OpenCode's
/// own `-m` explicitly documents `provider/model` as its expected format, so it keeps the full
/// string; `TmAdapter::pin_model` needs the provider half too, so it does its own `split_once`
/// rather than calling this).
fn bare_model_id(model: &str) -> &str {
    model.split_once('/').map_or(model, |(_, rest)| rest)
}

/// Bound on how long `binary_version` will wait for a `--version`-shaped call before giving up.
/// This exists because at least one real tool in this harness (`codex`, confirmed empirically
/// against the installed binary while writing this module) hangs past two minutes on `--version`
/// with no arguments and no stdin -- without a cap here, building the report header (which calls
/// `ToolAdapter::version` for every requested tool before any task runs) could hang the entire
/// run before `--task-timeout` ever gets a chance to matter.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Best-effort `<binary> <args...>` (typically `--version`), trimmed; `None` on any failure
/// (missing binary, non-zero exit, non-UTF8 output, or exceeding `VERSION_PROBE_TIMEOUT`).
/// Purely informational -- see `ToolAdapter::version`'s own doc comment.
fn binary_version(binary: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(binary);
    cmd.args(args).stdin(Stdio::null());
    let output = match run_with_timeout(cmd, Some(VERSION_PROBE_TIMEOUT)).ok()? {
        TimedOutput::TimedOut => return None,
        TimedOutput::Finished(output) => output,
    };
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// `cargo xtask bench-cross [--tools tm,opencode,...] [--task <filter>] [--out <dir>]
/// [--model <provider/model>] [--task-timeout <secs>] [--max-cost-usd <amount>]
/// [--real-claude-auth]` -- real runs only, opt-in, never part of `mise run verify`/`hygiene`
/// (shelling to a real tool against a real, possibly-metered provider is exactly the
/// non-deterministic, network-touching call `SPEC.md` §0 keeps out of the deterministic gate).
///
/// `--tools` defaults to `tm` alone when omitted: the only adapter with a genuinely
/// zero-additional-cost default credential already wired end to end
/// (`docs/decisions/D-005-devpass-default-provider.md`). `opencode`/`codex` read their own
/// `OPENAI_API_KEY`-shaped credential from the environment the same way they already do outside
/// this harness, so requesting them costs nothing extra to wire here. `claude` is refused unless
/// `--real-claude-auth` is also passed -- the explicit opt-in `docs/backlog.md`'s "A head-to-head
/// benchmark" section asks for, so a real, metered Claude/Anthropic API call is never the
/// accidental default of running this command.
///
/// `--model` is passed through to every requested tool (`claude --model`, `codex -m`, `opencode
/// -m`, and for `tm` a scratch `providers.toml` role candidate -- see `TmAdapter::pin_model`),
/// so a comparison isn't silently confounded by each tool's own default/last-configured model.
/// `--task-timeout` bounds each adapter's own subprocess work (killing the whole process group on
/// expiry -- see `run_with_timeout`) rather than leaving a hung tool to block the suite
/// indefinitely. `--max-cost-usd` stops scheduling further `(tool, task)` pairs once cumulative
/// reported spend reaches it (checked between pairs, never mid-pair -- see
/// `RunOptions::max_cost_micros`).
pub fn run(args: &[String]) -> Result<()> {
    let root = crate::workspace_root()?;
    let bench_root = root.join("bench");

    let tools_arg = flag_value(args, "--tools").unwrap_or_else(|| "tm".to_string());
    let task_filter = flag_value(args, "--task");
    let out_dir = flag_value(args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("tm-bench-cross-{}", std::process::id()))
        });
    let real_claude_auth = args.iter().any(|a| a == "--real-claude-auth");
    let model = flag_value(args, "--model");
    let task_timeout = flag_value(args, "--task-timeout")
        .map(|s| {
            s.parse::<u64>()
                .with_context(|| format!("--task-timeout `{s}` must be a whole number of seconds"))
        })
        .transpose()?
        .map(Duration::from_secs);
    let max_cost_micros = flag_value(args, "--max-cost-usd")
        .map(|s| {
            s.parse::<f64>()
                .with_context(|| format!("--max-cost-usd `{s}` must be a decimal dollar amount"))
        })
        .transpose()?
        .map(|dollars| (dollars * 1_000_000.0).round() as u64);

    let requested: Vec<String> = tools_arg.split(',').map(|s| s.trim().to_string()).collect();
    require_real_claude_auth_opt_in(&requested, real_claude_auth)?;

    let tasks = discover_bench_tasks(&bench_root, task_filter.as_deref())?;
    if tasks.is_empty() {
        bail!("no bench/tasks/*.toml matched (filter: {:?})", task_filter);
    }

    let tm_adapter = TmAdapter {
        binary: "tm".to_string(),
        model: model.clone(),
        timeout: task_timeout,
    };
    let claude_adapter = ClaudeAdapter {
        binary: "claude".to_string(),
        model: model.clone(),
        timeout: task_timeout,
    };
    let opencode_adapter = OpencodeAdapter {
        binary: "opencode".to_string(),
        model: model.clone(),
        timeout: task_timeout,
    };
    let codex_adapter = CodexAdapter {
        binary: "codex".to_string(),
        model: model.clone(),
        timeout: task_timeout,
    };

    let mut adapters: Vec<(&str, &dyn ToolAdapter)> = Vec::new();
    for name in &requested {
        match name.as_str() {
            "tm" => adapters.push(("tm", &tm_adapter)),
            "claude" => adapters.push(("claude", &claude_adapter)),
            "opencode" => adapters.push(("opencode", &opencode_adapter)),
            "codex" => adapters.push(("codex", &codex_adapter)),
            other => {
                bail!("unknown --tools entry `{other}` (expected tm, opencode, codex, claude)")
            }
        }
    }

    println!(
        "==> bench-cross: {} task(s) x {} tool(s), workdir {}",
        tasks.len(),
        adapters.len(),
        out_dir.display()
    );
    let options = RunOptions {
        model,
        task_timeout,
        max_cost_micros,
    };
    let report = run_comparison(&tasks, &bench_root, &adapters, &out_dir, &options)?;
    let markdown = render_markdown(&report);
    print!("{markdown}");

    std::fs::create_dir_all(&out_dir)?;
    let report_path = out_dir.join("cross-report.json");
    std::fs::write(&report_path, serde_json::to_string_pretty(&report)?)?;
    println!("==> bench-cross: wrote {}", report_path.display());
    Ok(())
}

/// Find `--flag value` in `args` (space-separated, not `--flag=value`).
/// The `--real-claude-auth` gate itself, factored out of `run()` so it's testable without
/// spawning `run()`'s own filesystem/process work: refuses to proceed when `claude` was
/// requested without the explicit opt-in, per this module's own doc comment on `ClaudeAdapter`.
fn require_real_claude_auth_opt_in(requested: &[String], real_claude_auth: bool) -> Result<()> {
    if requested.iter().any(|t| t == "claude") && !real_claude_auth {
        bail!(
            "refusing to run the `claude` adapter without --real-claude-auth: this would call a \
             real, metered Claude/Anthropic API by default. Pass --real-claude-auth to opt in \
             deliberately, per docs/decisions/D-033-cross-tool-benchmark.md."
        );
    }
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Parse every `bench/tasks/*.toml` file, optionally restricted to ids containing `filter`.
fn discover_bench_tasks(bench_root: &Path, filter: Option<&str>) -> Result<Vec<BenchTask>> {
    let tasks_dir = bench_root.join("tasks");
    let mut tasks = Vec::new();
    for entry in
        std::fs::read_dir(&tasks_dir).with_context(|| format!("reading {}", tasks_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let task =
            BenchTask::parse(&source).with_context(|| format!("parsing {}", path.display()))?;
        if filter.is_none_or(|f| task.id.contains(f)) {
            tasks.push(task);
        }
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted, deterministic adapter for tests: always returns the same `AdapterOutcome` and
    /// leaves the fixture's files exactly as copied, so pass/fail comes entirely from whether the
    /// task's own `test_command` already passes against the unmodified fixture (matching
    /// `bench/tasks/live-smoke.toml`'s own `test_command = ["true"]` design, so
    /// `FakeAdapter::default()` "solves" `live-smoke` without editing anything). Test-only: never
    /// spawns a coding tool, so `run_comparison`'s own tests never touch a real binary.
    struct FakeAdapter {
        outcome: AdapterOutcome,
    }

    impl Default for FakeAdapter {
        fn default() -> Self {
            FakeAdapter {
                outcome: AdapterOutcome {
                    tool_calls: Some(1),
                    tokens: Some(100),
                    cost_micros: Some(1_000),
                    model: Some("fake-model".to_string()),
                    timed_out: false,
                },
            }
        }
    }

    impl ToolAdapter for FakeAdapter {
        fn run(&self, _task: &BenchTask, _workdir: &Path) -> Result<AdapterOutcome> {
            Ok(self.outcome.clone())
        }
    }

    /// A test-only adapter that spawns the real `sleep` binary for `sleep_seconds` -- used to
    /// prove `run_with_timeout` actually kills a real, slow child process (not a simulation of
    /// one), the same mechanism every real per-tool adapter uses for `--task-timeout`. Neither a
    /// coding tool nor a network call -- see this module's own top-level doc comment.
    struct SleepingAdapter {
        sleep_seconds: u64,
        timeout: Option<Duration>,
    }

    impl ToolAdapter for SleepingAdapter {
        fn run(&self, _task: &BenchTask, _workdir: &Path) -> Result<AdapterOutcome> {
            let mut cmd = Command::new("sleep");
            cmd.arg(self.sleep_seconds.to_string());
            match run_with_timeout(cmd, self.timeout)? {
                TimedOutput::TimedOut => Ok(AdapterOutcome {
                    timed_out: true,
                    ..Default::default()
                }),
                TimedOutput::Finished(_) => Ok(AdapterOutcome::default()),
            }
        }
    }

    fn live_smoke_task() -> BenchTask {
        let root = crate::workspace_root().expect("workspace root");
        let source = std::fs::read_to_string(root.join("bench/tasks/live-smoke.toml"))
            .expect("read live-smoke.toml");
        BenchTask::parse(&source).expect("parse live-smoke.toml")
    }

    #[test]
    fn run_comparison_produces_a_result_per_tool_per_task_with_a_fake_adapter() {
        let task = live_smoke_task();
        let root = crate::workspace_root().expect("workspace root");
        let bench_root = root.join("bench");
        let workdir_root = tempfile::tempdir().expect("tempdir");

        let tm_fake = FakeAdapter::default();
        let other_fake = FakeAdapter::default();
        let adapters: Vec<(&str, &dyn ToolAdapter)> =
            vec![("tm", &tm_fake), ("opencode", &other_fake)];

        let report = run_comparison(
            &[task],
            &bench_root,
            &adapters,
            workdir_root.path(),
            &RunOptions::default(),
        )
        .expect("run_comparison should succeed against a fake adapter");

        assert_eq!(report.results.len(), 2, "{report:?}");
        let tools: Vec<&str> = report.results.iter().map(|r| r.tool.as_str()).collect();
        assert!(tools.contains(&"tm"));
        assert!(tools.contains(&"opencode"));
        for r in &report.results {
            assert_eq!(r.task_id, "live-smoke");
            // live-smoke's test_command is `["true"]`, which always exits zero -- both fake
            // adapters "pass" it without touching the fixture.
            assert!(r.passed, "{r:?}");
            assert!(r.score > 0.0, "{r:?}");
        }
    }

    #[test]
    fn run_comparison_scores_a_failing_test_command_as_failed_not_an_error() {
        let root = crate::workspace_root().expect("workspace root");
        let bench_root = root.join("bench");
        let mut task = live_smoke_task();
        task.fixture.test_command = Some(vec!["false".to_string()]);
        let workdir_root = tempfile::tempdir().expect("tempdir");
        let fake = FakeAdapter::default();
        let adapters: Vec<(&str, &dyn ToolAdapter)> = vec![("tm", &fake)];

        let report = run_comparison(
            &[task],
            &bench_root,
            &adapters,
            workdir_root.path(),
            &RunOptions::default(),
        )
        .expect("run_comparison should still succeed overall");

        assert_eq!(report.results.len(), 1);
        assert!(!report.results[0].passed);
    }

    #[test]
    fn run_comparison_marks_a_real_timed_out_process_as_timeout_not_pass_or_fail() {
        let task = live_smoke_task();
        let root = crate::workspace_root().expect("workspace root");
        let bench_root = root.join("bench");
        let workdir_root = tempfile::tempdir().expect("tempdir");
        let sleeping = SleepingAdapter {
            sleep_seconds: 5,
            timeout: Some(Duration::from_millis(200)),
        };
        let adapters: Vec<(&str, &dyn ToolAdapter)> = vec![("tm", &sleeping)];
        let options = RunOptions {
            task_timeout: Some(Duration::from_millis(200)),
            ..Default::default()
        };

        let start = Instant::now();
        let report = run_comparison(
            &[task],
            &bench_root,
            &adapters,
            workdir_root.path(),
            &options,
        )
        .expect("run_comparison should still succeed overall");

        assert_eq!(report.results.len(), 1, "{report:?}");
        assert!(report.results[0].timed_out, "{:?}", report.results[0]);
        assert!(!report.results[0].passed, "{:?}", report.results[0]);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "should return promptly after killing the sleeping process, not wait out its full \
             5s sleep"
        );
    }

    #[test]
    fn run_comparison_stops_scheduling_once_the_cost_cap_is_reached() {
        let mut task_a = live_smoke_task();
        task_a.id = "live-smoke-a".to_string();
        let mut task_b = live_smoke_task();
        task_b.id = "live-smoke-b".to_string();
        let root = crate::workspace_root().expect("workspace root");
        let bench_root = root.join("bench");
        let workdir_root = tempfile::tempdir().expect("tempdir");
        let fake = FakeAdapter::default(); // reports cost_micros: Some(1_000) every run
        let adapters: Vec<(&str, &dyn ToolAdapter)> = vec![("tm", &fake)];
        let options = RunOptions {
            max_cost_micros: Some(1_000),
            ..Default::default()
        };

        let report = run_comparison(
            &[task_a, task_b],
            &bench_root,
            &adapters,
            workdir_root.path(),
            &options,
        )
        .expect("run_comparison should still succeed overall");

        assert_eq!(
            report.results.len(),
            1,
            "should stop scheduling once the first pair's cost reaches the cap: {report:?}"
        );
    }

    #[test]
    fn run_comparison_header_carries_the_requested_model_timeout_and_cost_cap() {
        let task = live_smoke_task();
        let root = crate::workspace_root().expect("workspace root");
        let bench_root = root.join("bench");
        let workdir_root = tempfile::tempdir().expect("tempdir");
        let fake = FakeAdapter::default();
        let adapters: Vec<(&str, &dyn ToolAdapter)> = vec![("tm", &fake)];
        let options = RunOptions {
            model: Some("anthropic/claude-sonnet-5".to_string()),
            task_timeout: Some(Duration::from_secs(120)),
            max_cost_micros: Some(5_000_000),
        };

        let report = run_comparison(
            &[task],
            &bench_root,
            &adapters,
            workdir_root.path(),
            &options,
        )
        .expect("run_comparison should still succeed overall");

        assert_eq!(
            report.header.model.as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
        assert_eq!(report.header.task_timeout_seconds, Some(120));
        assert_eq!(report.header.max_cost_micros, Some(5_000_000));
        // `FakeAdapter` keeps `ToolAdapter::version`'s default (`None`).
        assert_eq!(report.header.tool_versions, vec![("tm".to_string(), None)]);
    }

    #[test]
    fn render_markdown_includes_a_tool_column_and_every_result() {
        let report = CrossToolReport {
            header: ReportHeader {
                permission_postures: vec![
                    ("tm".to_string(), "n/a (local ticket run)".to_string()),
                    ("opencode".to_string(), "--auto".to_string()),
                ],
                tool_versions: vec![
                    ("tm".to_string(), Some("tm 0.1.0".to_string())),
                    ("opencode".to_string(), None),
                ],
                model: Some("anthropic/claude-sonnet-5".to_string()),
                task_timeout_seconds: Some(120),
                max_cost_micros: Some(5_000_000),
            },
            results: vec![
                CrossTaskResult {
                    tool: "tm".to_string(),
                    task_id: "live-smoke".to_string(),
                    passed: true,
                    score: 1.0,
                    cost_micros: 500,
                    tool_calls: 2,
                    wall_seconds: 3,
                    tokens: Some(42),
                    model: Some("claude-sonnet-5".to_string()),
                    timed_out: false,
                },
                CrossTaskResult {
                    tool: "opencode".to_string(),
                    task_id: "live-smoke".to_string(),
                    passed: false,
                    score: 0.0,
                    cost_micros: 0,
                    tool_calls: 0,
                    wall_seconds: 1,
                    tokens: None,
                    model: None,
                    timed_out: false,
                },
            ],
        };
        let markdown = render_markdown(&report);
        assert!(markdown.contains("Model: anthropic/claude-sonnet-5"));
        assert!(markdown.contains("Task timeout: 120s"));
        assert!(markdown.contains("Max cost cap: $5.0000"));
        assert!(markdown.contains("| Tool | Version |"));
        assert!(markdown.contains("| tm | tm 0.1.0 |"));
        assert!(markdown.contains("| opencode | unknown |"));
        assert!(markdown.contains("| Tool | Permission posture |"));
        assert!(markdown.contains("| opencode | --auto |"));
        assert!(markdown.contains("| Tool | Task | Model |"));
        assert!(markdown.contains("Tokens"));
        assert!(markdown.contains("| tm | live-smoke | claude-sonnet-5 | pass"));
        assert!(markdown.contains("| opencode | live-smoke | - | fail"));
    }

    #[test]
    fn render_markdown_on_an_empty_report_says_so() {
        let markdown = render_markdown(&CrossToolReport::default());
        assert!(markdown.contains("No tasks ran."));
    }

    #[test]
    fn claude_adapter_without_opt_in_is_refused_by_the_same_gate_run_calls() {
        let requested = ["tm".to_string(), "claude".to_string()];
        assert!(require_real_claude_auth_opt_in(&requested, false).is_err());
        assert!(require_real_claude_auth_opt_in(&requested, true).is_ok());
        assert!(require_real_claude_auth_opt_in(&["tm".to_string()], false).is_ok());
    }

    #[test]
    fn parse_best_effort_usage_reads_claude_style_usage_object() {
        let text = r#"{"usage":{"input_tokens":10,"output_tokens":5},"cost_usd":0.002,"model":"claude-sonnet-5"}"#;
        let outcome = parse_best_effort_usage(text);
        assert_eq!(outcome.tokens, Some(15));
        assert_eq!(outcome.cost_micros, Some(2_000));
        assert_eq!(outcome.model.as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn parse_best_effort_usage_on_unrecognized_text_returns_all_none() {
        let outcome = parse_best_effort_usage("not json at all");
        assert!(outcome.tool_calls.is_none());
        assert!(outcome.tokens.is_none());
        assert!(outcome.cost_micros.is_none());
        assert!(outcome.model.is_none());
    }

    #[test]
    fn discover_bench_tasks_filters_by_id_substring() {
        let root = crate::workspace_root().expect("workspace root");
        let bench_root = root.join("bench");
        let filtered = discover_bench_tasks(&bench_root, Some("live-smoke")).expect("discover");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "live-smoke");
    }

    #[test]
    fn tm_adapter_pin_model_writes_a_hand_edited_providers_toml() {
        let adapter = TmAdapter {
            binary: "tm".to_string(),
            model: None,
            timeout: None,
        };
        let workdir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(workdir.path().join(".tm")).expect("mkdir .tm");
        // Simulate `tm init`'s already-generated file: a `[meta]` marker, `coder.fast`'s own
        // default candidate, and an unrelated role that must survive the override untouched.
        std::fs::write(
            workdir.path().join(".tm/providers.toml"),
            "[meta]\ngenerated = true\n\n[coder.fast]\ncandidates = [\n    { provider = \"devpass\", model = \"devpass-default\", max_concurrency = 10 },\n]\n\n[reviewer.semantic]\ncandidates = [\n    { provider = \"devpass\", model = \"devpass-default\", max_concurrency = 5 },\n]\n",
        )
        .expect("seed providers.toml");

        adapter
            .pin_model(workdir.path(), "anthropic/claude-sonnet-5")
            .expect("pin_model should succeed");

        let written = std::fs::read_to_string(workdir.path().join(".tm/providers.toml"))
            .expect("read providers.toml");
        let table: tm_provider::RoleTable =
            tm_provider::RoleTable::parse(&written).expect("pin_model must write valid TOML");
        let coder_fast = table.candidates_for(tm_types::Role::CoderFast);
        assert_eq!(coder_fast.len(), 1, "{written}");
        assert_eq!(coder_fast[0].provider, "anthropic");
        assert_eq!(coder_fast[0].model, "claude-sonnet-5");
        // The unrelated role from the seeded file must survive the override untouched, not be
        // silently dropped (see `pin_model`'s own doc comment on why a from-scratch rewrite would
        // be a real bug).
        assert!(
            !table
                .candidates_for(tm_types::Role::ReviewerSemantic)
                .is_empty(),
            "{written}"
        );
        // No `[meta]`/`generated = true` -- otherwise the role-table loader would treat this as
        // a generated file and overwrite `coder.fast`'s primary from `DEVPASS_*` env vars on the
        // next load, silently undoing the pin (see `pin_model`'s own doc comment).
        assert!(!written.contains("[meta]"), "{written}");
    }

    #[test]
    fn tm_adapter_pin_model_rejects_a_model_without_a_provider_prefix() {
        let adapter = TmAdapter {
            binary: "tm".to_string(),
            model: None,
            timeout: None,
        };
        let workdir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(workdir.path().join(".tm")).expect("mkdir .tm");

        assert!(adapter
            .pin_model(workdir.path(), "claude-sonnet-5")
            .is_err());
    }

    #[test]
    fn run_with_timeout_kills_a_real_slow_process_and_reports_timed_out() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let start = Instant::now();

        let outcome = run_with_timeout(cmd, Some(Duration::from_millis(200)))
            .expect("run_with_timeout should not error on a timeout");

        assert!(matches!(outcome, TimedOutput::TimedOut));
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "should return promptly after killing the process, not wait out its full 5s sleep"
        );
    }

    #[test]
    fn run_with_timeout_returns_finished_output_when_the_command_completes_first() {
        let mut cmd = Command::new("true");
        cmd.stdin(Stdio::null());
        let outcome = run_with_timeout(cmd, None).expect("run_with_timeout should succeed");
        match outcome {
            TimedOutput::Finished(output) => assert!(output.status.success()),
            TimedOutput::TimedOut => panic!("`true` should not time out with no timeout set"),
        }
    }

    /// `[program, args...]` from a not-yet-spawned `Command`, for asserting on the built argv
    /// without a real binary on `$PATH` -- see each `*Adapter::build_command`'s own doc comment.
    fn argv(cmd: &Command) -> Vec<String> {
        std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|s| s.to_string_lossy().to_string())
            .collect()
    }

    fn contains_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn claude_adapter_command_includes_bypass_permissions_and_the_scratch_workdir() {
        let task = live_smoke_task();
        let adapter = ClaudeAdapter {
            binary: "claude".to_string(),
            model: None,
            timeout: None,
        };
        let workdir = std::path::Path::new("/tmp/bench-cross-test-workdir");
        let cmd = adapter.build_command(&task, workdir);
        let args = argv(&cmd);

        assert_eq!(args[0], "claude");
        assert!(
            contains_pair(&args, "--permission-mode", "bypassPermissions"),
            "{args:?}"
        );
        assert!(contains_pair(&args, "--output-format", "json"), "{args:?}");
        assert_eq!(cmd.get_current_dir(), Some(workdir));
        assert!(!args.iter().any(|a| a == "--model"), "{args:?}");
    }

    #[test]
    fn claude_adapter_command_passes_a_pinned_model() {
        let task = live_smoke_task();
        let adapter = ClaudeAdapter {
            binary: "claude".to_string(),
            // A full `provider/model` id, as `--model` on the xtask CLI takes -- the adapter
            // must strip the `anthropic/` prefix before handing it to `claude --model`, which
            // takes a bare alias or model name.
            model: Some("anthropic/claude-sonnet-5".to_string()),
            timeout: None,
        };
        let workdir = std::path::Path::new("/tmp/bench-cross-test-workdir");
        let args = argv(&adapter.build_command(&task, workdir));
        assert!(
            contains_pair(&args, "--model", "claude-sonnet-5"),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "anthropic/claude-sonnet-5"),
            "{args:?}"
        );
    }

    #[test]
    fn opencode_adapter_command_includes_auto_and_the_scratch_workdir() {
        let task = live_smoke_task();
        let adapter = OpencodeAdapter {
            binary: "opencode".to_string(),
            model: None,
            timeout: None,
        };
        let workdir = std::path::Path::new("/tmp/bench-cross-test-workdir");
        let cmd = adapter.build_command(&task, workdir);
        let args = argv(&cmd);

        assert_eq!(args[0], "opencode");
        assert!(args.iter().any(|a| a == "--auto"), "{args:?}");
        assert!(contains_pair(&args, "--format", "json"), "{args:?}");
        assert_eq!(cmd.get_current_dir(), Some(workdir));
    }

    #[test]
    fn opencode_adapter_command_passes_a_pinned_model() {
        let task = live_smoke_task();
        let adapter = OpencodeAdapter {
            binary: "opencode".to_string(),
            model: Some("anthropic/claude-sonnet-5".to_string()),
            timeout: None,
        };
        let workdir = std::path::Path::new("/tmp/bench-cross-test-workdir");
        let args = argv(&adapter.build_command(&task, workdir));
        assert!(
            contains_pair(&args, "-m", "anthropic/claude-sonnet-5"),
            "{args:?}"
        );
    }

    #[test]
    fn codex_adapter_command_includes_workspace_write_sandbox_and_the_scratch_workdir() {
        let task = live_smoke_task();
        let adapter = CodexAdapter {
            binary: "codex".to_string(),
            model: None,
            timeout: None,
        };
        let workdir = std::path::Path::new("/tmp/bench-cross-test-workdir");
        let cmd = adapter.build_command(&task, workdir);
        let args = argv(&cmd);

        assert_eq!(args[0], "codex");
        assert!(
            contains_pair(&args, "--sandbox", "workspace-write"),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "--json"), "{args:?}");
        assert_eq!(cmd.get_current_dir(), Some(workdir));
    }

    #[test]
    fn codex_adapter_command_passes_a_pinned_model() {
        let task = live_smoke_task();
        let adapter = CodexAdapter {
            binary: "codex".to_string(),
            model: Some("openai/gpt-5-codex".to_string()),
            timeout: None,
        };
        let workdir = std::path::Path::new("/tmp/bench-cross-test-workdir");
        let args = argv(&adapter.build_command(&task, workdir));
        assert!(contains_pair(&args, "-m", "gpt-5-codex"), "{args:?}");
        assert!(!args.iter().any(|a| a == "openai/gpt-5-codex"), "{args:?}");
    }

    #[test]
    fn bare_model_id_strips_a_provider_prefix_but_leaves_a_bare_id_alone() {
        assert_eq!(
            bare_model_id("anthropic/claude-sonnet-5"),
            "claude-sonnet-5"
        );
        assert_eq!(bare_model_id("claude-sonnet-5"), "claude-sonnet-5");
    }

    /// Regression test for a real bug this module had briefly: `run_with_timeout` spawned the
    /// child without piping stdout/stderr, so every adapter silently got back empty output on a
    /// real run (`TmAdapter::tm_json` would bail "printed no ticket id" on every single call).
    /// Exercises the actual `ToolAdapter::run` path (not just `run_with_timeout` directly) against
    /// a real, tiny shell script standing in for `claude`, which also closes the loop on task
    /// u1-bench-cross-stdin-closed's own acceptance criterion: a tool that blocks reading stdin
    /// must still complete, because `build_command` closes it with `Stdio::null()`.
    #[test]
    fn claude_adapter_completes_and_captures_output_when_the_tool_blocks_on_stdin() {
        let task = live_smoke_task();
        let workdir = tempfile::tempdir().expect("tempdir");
        let script_path = workdir.path().join("fake-claude.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\ncat >/dev/null\necho '{\"model\":\"m\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2},\"cost_usd\":0.001}'\n",
        )
        .expect("write fake claude script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path)
                .expect("stat fake claude script")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).expect("chmod +x fake claude script");
        }

        let adapter = ClaudeAdapter {
            binary: script_path.to_string_lossy().to_string(),
            model: None,
            timeout: Some(Duration::from_secs(5)),
        };

        let outcome = adapter
            .run(&task, workdir.path())
            .expect("should complete rather than hang on stdin, or error on empty output");

        assert!(!outcome.timed_out, "{outcome:?}");
        assert_eq!(outcome.model.as_deref(), Some("m"));
        assert_eq!(outcome.tokens, Some(3));
    }
}
