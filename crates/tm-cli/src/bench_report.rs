//! `tm bench report <json> [--out]`: render a [`tm_harness::BenchmarkReport`] as markdown.
//!
//! [`render_report_markdown`] is a pure function of an already-parsed `BenchmarkReport`, unit
//! tested directly against a hand-built report per `SPEC.md` §0 — no network, no model calls.
//! [`run_bench_report`] is the only IO layered on top (reading the JSON, optionally writing the
//! markdown to a file), matching `crate::replay_diff`'s pure-render-then-write split.

use std::fs;

use tm_harness::BenchmarkReport;
use tm_types::{Result, TmError};

use crate::args::BenchReportArgs;
use crate::render::Renderer;

/// Render `report` as a markdown document: an aggregate-score heading, then a per-task table of
/// pass/fail, score, cost, tool calls and wall time.
pub fn render_report_markdown(report: &BenchmarkReport) -> String {
    let mut out = format!(
        "# Benchmark report — epoch {}\n\nAggregate score: {:.2} ({} task{})\n\n",
        report.epoch,
        report.aggregate_score,
        report.tasks.len(),
        if report.tasks.len() == 1 { "" } else { "s" }
    );

    if report.tasks.is_empty() {
        out.push_str("No tasks ran.\n");
        return out;
    }

    out.push_str("| Task | Result | Score | Cost | Tool calls | Wall time |\n");
    out.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for task in &report.tasks {
        out.push_str(&format!(
            "| {} | {} | {:.2} | ${:.4} | {} | {}s |\n",
            task.task_id,
            if task.passed { "pass" } else { "fail" },
            task.score,
            task.cost_micros as f64 / 1_000_000.0,
            task.tool_calls,
            task.wall_seconds,
        ));
    }
    out
}

/// `tm bench report`: read `args.path` as a `BenchmarkReport` JSON file, render it as markdown,
/// and print it (or write it to `args.out` when given).
pub fn run_bench_report(args: &BenchReportArgs, renderer: &Renderer) -> Result<()> {
    let content = fs::read_to_string(&args.path)
        .map_err(|e| TmError::storage(format!("Failed to read {}: {e}", args.path.display())))?;
    let report: BenchmarkReport = serde_json::from_str(&content).map_err(|e| {
        TmError::parse(format!(
            "{} doesn't look like a benchmark report: {e}",
            args.path.display()
        ))
    })?;

    let markdown = render_report_markdown(&report);

    if let Some(out_path) = &args.out {
        fs::write(out_path, &markdown).map_err(|e| {
            TmError::storage(format!("Failed to write {}: {e}", out_path.display()))
        })?;
    }

    if renderer.is_json() {
        renderer.emit(&report, "")?;
    } else if let Some(out_path) = &args.out {
        renderer.note(&format!(
            "Wrote benchmark report as markdown to {}.",
            out_path.display()
        ));
    } else {
        renderer.emit(&(), &markdown)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_harness::TaskResult;
    use tm_types::Timestamp;

    fn sample_report() -> BenchmarkReport {
        BenchmarkReport {
            epoch: 3,
            tasks: vec![
                TaskResult {
                    task_id: "fix-off-by-one".to_string(),
                    passed: true,
                    cost_micros: 1_250_000,
                    wall_seconds: 42,
                    tool_calls: 7,
                    context_bytes: 4096,
                    unnecessary_ops: 0,
                    score: 0.95,
                },
                TaskResult {
                    task_id: "add-missing-test".to_string(),
                    passed: false,
                    cost_micros: 500_000,
                    wall_seconds: 12,
                    tool_calls: 3,
                    context_bytes: 1024,
                    unnecessary_ops: 1,
                    score: 0.2,
                },
            ],
            aggregate_score: 0.575,
            generated_at: Timestamp::EPOCH,
        }
    }

    #[test]
    fn renders_expected_markdown() {
        let report = sample_report();
        let markdown = render_report_markdown(&report);
        let expected = "# Benchmark report — epoch 3\n\n\
Aggregate score: 0.57 (2 tasks)\n\n\
| Task | Result | Score | Cost | Tool calls | Wall time |\n\
| --- | --- | --- | --- | --- | --- |\n\
| fix-off-by-one | pass | 0.95 | $1.2500 | 7 | 42s |\n\
| add-missing-test | fail | 0.20 | $0.5000 | 3 | 12s |\n";
        assert_eq!(markdown, expected);
    }

    #[test]
    fn renders_no_tasks_message_when_empty() {
        let report = BenchmarkReport {
            epoch: 1,
            tasks: vec![],
            aggregate_score: 0.0,
            generated_at: Timestamp::EPOCH,
        };
        let markdown = render_report_markdown(&report);
        assert!(markdown.contains("No tasks ran."));
        assert!(!markdown.contains('|'));
    }

    #[test]
    fn singular_task_count_has_no_trailing_s() {
        let mut report = sample_report();
        report.tasks.truncate(1);
        let markdown = render_report_markdown(&report);
        assert!(markdown.contains("(1 task)"));
    }
}
