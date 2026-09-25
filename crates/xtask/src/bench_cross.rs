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
//! history) and the fixture's own `test_command` (e.g. `true`/`false`) locally -- neither a coding
//! tool nor a network call, the two things this module's own real adapters are opt-in about. Real
//! runs are opt-in from the CLI (`--tools`) and are never part of `mise run verify` or `mise run
//! hygiene` -- see `run`'s own doc comment.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

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
}

/// The full comparison: every `(tool, task)` pair that ran, in run order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CrossToolReport {
    pub results: Vec<CrossTaskResult>,
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

/// Run every task in `tasks` through every `(tool name, adapter)` pair in `adapters`, scoring
/// each with that task's own `ScoringSpec`/`ExpectedOutcome`. `bench_root` resolves
/// `task.fixture.path`; `workdir_root` is where each per-tool, per-task scratch copy is made
/// (`<workdir_root>/<tool>/<task id>`) -- a caller-supplied `mktemp` directory in production, a
/// `tempfile::TempDir` in tests.
///
/// One `(tool, task)` pair's adapter failure (`ToolAdapter::run` returning `Err`, or its own
/// `Command` failing to start) is scored as a failed run for that pair and does not stop the rest
/// of the suite -- a missing or misconfigured external CLI should not hide every other tool's
/// result.
pub fn run_comparison(
    tasks: &[BenchTask],
    bench_root: &Path,
    adapters: &[(&str, &dyn ToolAdapter)],
    workdir_root: &Path,
) -> Result<CrossToolReport> {
    let mut results = Vec::with_capacity(tasks.len() * adapters.len());
    for task in tasks {
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
                    });
                    eprintln!("bench-cross: {tool_name}/{}: {e:?}", task.id);
                    continue;
                }
            };

            let start = Instant::now();
            let outcome = adapter.run(task, &workdir);
            let wall_seconds = start.elapsed().as_secs() as u32;

            let (tool_calls, tokens, model, cost_micros, adapter_ran) = match &outcome {
                Ok(o) => (
                    o.tool_calls.unwrap_or(0),
                    o.tokens,
                    o.model.clone(),
                    o.cost_micros.unwrap_or(0),
                    true,
                ),
                Err(e) => {
                    eprintln!("bench-cross: {tool_name}/{} adapter failed: {e:?}", task.id);
                    (0, None, None, 0, false)
                }
            };

            let passed = adapter_ran && run_test_command(&workdir, task).unwrap_or(false);
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
            });
        }
    }
    Ok(CrossToolReport { results })
}

/// Render `report` as markdown: one table, grouped by task, each row a `(tool, task)` pair --
/// the same pass/fail-score-cost-tool_calls-wall_time columns `tm-cli/src/bench_report.rs`'s
/// `render_report_markdown` renders for a single-tool `BenchmarkReport`, with a leading `Tool`
/// column since this report always compares more than one.
pub fn render_markdown(report: &CrossToolReport) -> String {
    if report.results.is_empty() {
        return "# Cross-tool benchmark report\n\nNo tasks ran.\n".to_string();
    }
    let mut out = String::from("# Cross-tool benchmark report\n\n");
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
            if r.passed { "pass" } else { "fail" },
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
}

impl TmAdapter {
    /// Run `tm <args>` in `workdir` with `--json`, returning stdout on success.
    fn tm_json(&self, workdir: &Path, args: &[&str]) -> Result<String> {
        let mut full_args = vec!["--json"];
        full_args.extend_from_slice(args);
        let output = Command::new(&self.binary)
            .args(&full_args)
            .current_dir(workdir)
            // `tm` never expects to read from stdin here (every one of these calls is
            // non-interactive), and an inherited *open* stdin pipe (e.g. this xtask itself
            // running under a CI runner or another tool's pipe) can make a child that probes
            // stdin block indefinitely rather than treating "nothing there" as EOF -- the same
            // hang this closes off for the other three adapters below.
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run `tm {}`", full_args.join(" ")))?;
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

impl ToolAdapter for TmAdapter {
    fn run(&self, task: &BenchTask, workdir: &Path) -> Result<AdapterOutcome> {
        // `ticket dispatch` needs an existing project; bootstrap a repo-scoped one at `workdir`
        // itself first (see this struct's own doc comment for why `ticket new`'s auto-bootstrap
        // doesn't help here).
        self.tm_json(workdir, &["init"])?;

        let dispatch_out = self.tm_json(workdir, &["ticket", "dispatch", &task.task])?;
        // `TicketId` serializes as a bare JSON string, e.g. `"T-12"`.
        let ticket_id = dispatch_out.trim_matches('"').to_string();
        if ticket_id.is_empty() {
            bail!("`tm ticket dispatch` printed no ticket id");
        }

        // `tm run <ticket>` reports a failed attempt as a non-zero exit; still worth reading
        // whatever stats got recorded, so don't bail out before the `tm stats` call below.
        let run_result = self.tm_json(workdir, &["run", &ticket_id]);
        if let Err(e) = &run_result {
            eprintln!("bench-cross: tm run {ticket_id} did not finish cleanly: {e:?}");
        }

        let stats_out = self.tm_json(
            workdir,
            &["stats", "--by", "ticket", "--ticket", &ticket_id],
        )?;
        let rows: Vec<tm_harness::metrics::TicketMetrics> = serde_json::from_str(&stats_out)
            .with_context(|| format!("parsing `tm stats` output: {stats_out}"))?;

        match rows.into_iter().next() {
            Some(m) => Ok(AdapterOutcome {
                tool_calls: Some(m.tool_calls),
                tokens: Some(m.tokens_in.saturating_add(m.tokens_out)),
                cost_micros: Some(m.dollars_micros),
                // `TicketMetrics` (the pure fold `tm stats` renders from) doesn't carry a model
                // column -- it attributes cost/tokens per (provider, model) pair only under
                // `tm stats --by model`, not per-ticket, so this adapter has nothing to report
                // here yet.
                model: None,
            }),
            None => Ok(AdapterOutcome::default()),
        }
    }
}

/// Shells Claude Code's own scriptable one-shot turn: `claude -p <task> --output-format json`.
/// Real-Claude auth (the metered Anthropic API, or an interactive subscription session) is never
/// the accidental default -- per `docs/backlog.md`'s "A head-to-head benchmark" section, this
/// adapter is only ever constructed by `run` when `--real-claude-auth` was passed explicitly;
/// see that function's doc comment for the gate itself.
pub struct ClaudeAdapter {
    pub binary: String,
}

impl ToolAdapter for ClaudeAdapter {
    fn run(&self, task: &BenchTask, workdir: &Path) -> Result<AdapterOutcome> {
        let output = Command::new(&self.binary)
            .args(["-p", &task.task, "--output-format", "json"])
            .current_dir(workdir)
            // Closed, not just unpiped: `claude -p` probes stdin for piped input and prints
            // "no stdin data received in 3s" then hangs when it's an open pipe with nothing
            // written to it (the real failure this fixes -- see this module's own doc comment).
            .stdin(Stdio::null())
            .output()
            .context("failed to run `claude -p`")?;
        // Best-effort: `claude -p --output-format json` prints one JSON result object with a
        // `usage`/`cost_usd`/`num_turns`-shaped payload. Field names are not pinned by this
        // crate (no dependency on Claude Code's own output schema), so a shape it doesn't
        // recognize degrades to "ran, but no metrics reported" rather than a hard error --
        // pass/fail always comes from re-running the task's own `test_command`, never from this.
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_best_effort_usage(&stdout))
    }
}

/// Shells `opencode run <task> --format json` (OpenCode's non-interactive mode).
pub struct OpencodeAdapter {
    pub binary: String,
}

impl ToolAdapter for OpencodeAdapter {
    fn run(&self, task: &BenchTask, workdir: &Path) -> Result<AdapterOutcome> {
        let output = Command::new(&self.binary)
            .args(["run", &task.task, "--format", "json"])
            .current_dir(workdir)
            // `opencode run` hung for the full timeout against an open stdin pipe and finished
            // in 14s against `/dev/null` (docs/audits/2026-09-25-bench-plan.md's evidence) --
            // close it explicitly rather than inheriting whatever the caller's stdin is.
            .stdin(Stdio::null())
            .output()
            .context("failed to run `opencode run`")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_best_effort_usage(&stdout))
    }
}

/// Shells `codex exec <task> --json` (Codex CLI's non-interactive mode; streams newline-delimited
/// JSON events to stdout, one per state change).
pub struct CodexAdapter {
    pub binary: String,
}

impl ToolAdapter for CodexAdapter {
    fn run(&self, task: &BenchTask, workdir: &Path) -> Result<AdapterOutcome> {
        let output = Command::new(&self.binary)
            .args(["exec", &task.task, "--json"])
            .current_dir(workdir)
            .stdin(Stdio::null())
            .output()
            .context("failed to run `codex exec`")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_best_effort_usage(&stdout))
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

/// `cargo xtask bench-cross [--tools tm,opencode,...] [--task <filter>] [--out <dir>]
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

    let requested: Vec<String> = tools_arg.split(',').map(|s| s.trim().to_string()).collect();
    require_real_claude_auth_opt_in(&requested, real_claude_auth)?;

    let tasks = discover_bench_tasks(&bench_root, task_filter.as_deref())?;
    if tasks.is_empty() {
        bail!("no bench/tasks/*.toml matched (filter: {:?})", task_filter);
    }

    let tm_adapter = TmAdapter {
        binary: "tm".to_string(),
    };
    let claude_adapter = ClaudeAdapter {
        binary: "claude".to_string(),
    };
    let opencode_adapter = OpencodeAdapter {
        binary: "opencode".to_string(),
    };
    let codex_adapter = CodexAdapter {
        binary: "codex".to_string(),
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
    let report = run_comparison(&tasks, &bench_root, &adapters, &out_dir)?;
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
                },
            }
        }
    }

    impl ToolAdapter for FakeAdapter {
        fn run(&self, _task: &BenchTask, _workdir: &Path) -> Result<AdapterOutcome> {
            Ok(self.outcome.clone())
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

        let report = run_comparison(&[task], &bench_root, &adapters, workdir_root.path())
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

        let report = run_comparison(&[task], &bench_root, &adapters, workdir_root.path())
            .expect("run_comparison should still succeed overall");

        assert_eq!(report.results.len(), 1);
        assert!(!report.results[0].passed);
    }

    #[test]
    fn render_markdown_includes_a_tool_column_and_every_result() {
        let report = CrossToolReport {
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
                },
            ],
        };
        let markdown = render_markdown(&report);
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
}
