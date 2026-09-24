//! Repository-local benchmarks: `bench/tasks/*.toml`, a deterministic replay runner, and
//! `compare(a, b)` for promotion decisions.
//!
//! A benchmark task pins a fixture, a scripted interaction, and a scoring rule to a single TOML
//! file so that running the same task against the same epoch twice produces the same
//! [`TaskResult`]. The runner never touches the wall clock or a live model: it drives a
//! caller-supplied [`SeededProvider`] (an adapter the caller writes over e.g.
//! `tm_provider::MockProvider`, since this crate does not depend on `tm-provider`) and an
//! injected `&dyn Clock`/`&dyn IdSource`, per `SPEC.md` §10's determinism requirement.
//!
//! A fixture can be marked as "live-only" by setting [`BenchFixture::test_command`] without
//! providing any scripted steps (an empty sequence from the provider). The scripted runner skips
//! such tasks, allowing live-only fixtures to coexist with scripted tasks in the same repository
//! without breaking deterministic replay mode. This separation is particularly useful for
//! fixtures that require real-world resources only available in live mode.

use std::collections::HashSet;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tm_types::{Clock, IdSource, Predicate, PredicateOutcome, Result, Timestamp};

/// Where a benchmark task's fixture lives and what it sets up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchFixture {
    /// Path to the fixture, relative to the `bench/` directory.
    pub path: String,
    /// Human-readable description of the fixture's starting state.
    pub description: String,
    /// Optional command to run the test. When set, this fixture is marked for live mode (a mode
    /// that is not implemented in tm-harness). When the seeded provider has no scripted steps for
    /// this task, the scripted runner skips it, allowing live-only fixtures to coexist with
    /// scripted ones without breaking deterministic replay.
    #[serde(default)]
    pub test_command: Option<Vec<String>>,
    /// Optional setup commands to execute before running the test. Defaults to an empty list.
    /// Intentionally unused in the scripted runner; this field exists for live mode.
    #[serde(default)]
    pub setup_commands: Vec<Vec<String>>,
}

/// Weights combining a [`TaskResult`] into one scalar score. Higher is better; each weight
/// applies to a `[0.0, 1.0]`-normalized sub-score, so weights need not sum to 1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoringSpec {
    /// Weight on whether [`ExpectedOutcome::predicate`] was satisfied.
    pub success_weight: f64,
    /// Weight on staying under [`ExpectedOutcome::max_cost_micros`].
    pub cost_weight: f64,
    /// Weight on staying under [`ExpectedOutcome::max_wall_seconds`].
    pub latency_weight: f64,
    /// Weight on staying under [`ExpectedOutcome::max_tool_calls`].
    pub tool_count_weight: f64,
    /// Weight on staying under [`ExpectedOutcome::max_context_bytes`].
    pub context_weight: f64,
    /// Weight on avoiding unnecessary reads/edits (penalizes [`TaskResult::unnecessary_ops`]).
    pub unnecessary_ops_weight: f64,
}

/// The success criterion and resource ceilings a task run is scored against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedOutcome {
    /// The predicate a successful run must satisfy.
    pub predicate: Predicate,
    /// Cost ceiling in micro-dollars.
    pub max_cost_micros: u64,
    /// Wall-clock ceiling.
    pub max_wall_seconds: u32,
    /// Tool-call ceiling.
    pub max_tool_calls: u32,
    /// Context-bytes-consumed ceiling.
    pub max_context_bytes: u64,
}

/// One `bench/tasks/*.toml` file: a repository-local task fixed at a deterministic fixture,
/// scoring rule, and expected outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchTask {
    /// Stable task identifier, unique within `bench/tasks/`.
    pub id: String,
    /// The task instruction given to the worker under test.
    pub task: String,
    /// The fixture the task runs against.
    pub fixture: BenchFixture,
    /// How a run of this task is scored.
    pub scoring: ScoringSpec,
    /// What a successful, in-budget run looks like.
    pub expected: ExpectedOutcome,
}

impl BenchTask {
    /// Parse one task from the contents of a `bench/tasks/*.toml` file.
    pub fn parse(source: &str) -> Result<BenchTask> {
        toml::from_str::<BenchTask>(source).map_err(|e| tm_types::TmError::parse(e.to_string()))
    }
}

/// The deterministic interface the bench runner drives instead of a live model.
///
/// Implemented by an adapter the caller writes over a real provider mock (e.g.
/// `tm_provider::MockProvider`, scripted per task). `tm-harness` stays decoupled from
/// `tm-provider` so the dependency graph doesn't cycle.
pub trait SeededProvider {
    /// Produce the deterministic output for the `step_index`-th scripted turn of `task`, along
    /// with the simulated resource cost of that turn (tokens/dollars/tool calls/context bytes
    /// are folded into the runner's running [`TaskResult`] by the caller's accounting, not by
    /// this trait).
    fn step(&self, task: &BenchTask, step_index: u32) -> Result<String>;
    /// True once `task` has no further scripted steps.
    fn is_finished(&self, task: &BenchTask, step_index: u32) -> bool;
}

/// Replays [`BenchTask`]s deterministically and scores the result.
pub struct BenchRunner<'a> {
    /// Clock injected for any timestamps the runner records; never `SystemClock::now()` inside
    /// scoring logic itself, only for stamping the resulting report.
    pub clock: &'a dyn Clock,
    /// Id source for any ids the runner mints (e.g. a synthetic session id for the replay).
    pub ids: &'a dyn IdSource,
}

/// The outcome of running one [`BenchTask`] to completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    /// The task this result is for.
    pub task_id: String,
    /// Whether [`ExpectedOutcome::predicate`] evaluated to satisfied.
    pub passed: bool,
    /// Simulated cost of the run, in micro-dollars.
    pub cost_micros: u64,
    /// Simulated wall-clock duration.
    pub wall_seconds: u32,
    /// Tool calls issued during the run.
    pub tool_calls: u32,
    /// Context bytes consumed during the run.
    pub context_bytes: u64,
    /// Reads or edits the runner judged unnecessary (outside the task's declared scope, or
    /// duplicates of an already-read/-applied one).
    pub unnecessary_ops: u32,
    /// The scalar score this task run received, per [`ScoringSpec`].
    pub score: f64,
}

/// Score of 1.0 while `actual` stays at or under `ceiling`, degrading toward 0.0 as it grows
/// past it. A `ceiling` of zero demands `actual == 0` for full credit.
fn ceiling_subscore(actual: u64, ceiling: u64) -> f64 {
    if ceiling == 0 {
        return if actual == 0 { 1.0 } else { 0.0 };
    }
    (ceiling as f64 / actual.max(1) as f64).min(1.0)
}

/// Score of 1.0 at zero unnecessary ops, asymptotically approaching 0.0 as they accumulate.
fn unnecessary_ops_subscore(ops: u32) -> f64 {
    1.0 / (1.0 + ops as f64)
}

/// Settle the non-composite predicates against the run's accumulated output log.
///
/// The run log is the newline-joined text every scripted step produced. Machine-checkable
/// leaves are settled by whether the fact they assert was attested in that log (the seeded
/// provider script is expected to emit such attestations for tasks meant to pass);
/// [`Predicate::HumanAttested`] and [`Predicate::Judgment`] can never be settled from replayed
/// text alone, so they always require judgment, consistent with `Predicate::is_machine_checkable`.
fn evaluate_leaf(predicate: &Predicate, output_log: &str) -> PredicateOutcome {
    match predicate {
        Predicate::CommandSucceeds { command } => {
            let joined = command.join(" ");
            if output_log.contains(&joined) {
                PredicateOutcome::Satisfied
            } else {
                PredicateOutcome::Unsatisfied(format!(
                    "command `{joined}` not attested in run output"
                ))
            }
        }
        Predicate::FileExists { path } => {
            if output_log.contains(path.as_str()) {
                PredicateOutcome::Satisfied
            } else {
                PredicateOutcome::Unsatisfied(format!("path `{path}` not attested in run output"))
            }
        }
        Predicate::FileMatches { path, regex } => {
            if !output_log.contains(path.as_str()) {
                return PredicateOutcome::Unsatisfied(format!(
                    "path `{path}` not attested in run output"
                ));
            }
            match Regex::new(regex) {
                Ok(re) if re.is_match(output_log) => PredicateOutcome::Satisfied,
                Ok(_) => PredicateOutcome::Unsatisfied(format!(
                    "run output for `{path}` does not match `{regex}`"
                )),
                Err(e) => PredicateOutcome::Unsatisfied(format!("invalid regex `{regex}`: {e}")),
            }
        }
        Predicate::TestsPass { suite } => {
            let marker = format!("tests_pass:{}", suite.as_deref().unwrap_or("default"));
            if output_log.contains(&marker) {
                PredicateOutcome::Satisfied
            } else {
                PredicateOutcome::Unsatisfied(format!("`{marker}` not attested in run output"))
            }
        }
        Predicate::TicketClosed { ticket } => {
            if output_log.contains(ticket.as_str()) {
                PredicateOutcome::Satisfied
            } else {
                PredicateOutcome::Unsatisfied(format!(
                    "ticket `{ticket}` closure not attested in run output"
                ))
            }
        }
        Predicate::HumanAttested { note } => PredicateOutcome::RequiresJudgment(note.clone()),
        Predicate::Judgment { claim } => PredicateOutcome::RequiresJudgment(claim.clone()),
        Predicate::AllOf(_) | Predicate::AnyOf(_) | Predicate::Not(_) => {
            // Predicate::evaluate only ever hands the leaf closure a non-composite variant.
            PredicateOutcome::Unsatisfied("composite predicate reached leaf fn".to_string())
        }
    }
}

impl<'a> BenchRunner<'a> {
    /// Run `task` against `provider` to completion and score the result.
    pub fn run(&self, task: &BenchTask, provider: &dyn SeededProvider) -> Result<TaskResult> {
        let mut step_index = 0u32;
        let mut output_log = String::new();
        let mut seen_outputs: HashSet<String> = HashSet::new();
        let mut context_bytes: u64 = 0;
        let mut unnecessary_ops: u32 = 0;

        while !provider.is_finished(task, step_index) {
            let output = provider
                .step(task, step_index)
                .map_err(|e| tm_types::TmError::Provider(e.to_string()))?;
            context_bytes += output.len() as u64;
            // A step whose output repeats one already seen this run is treated as an
            // unnecessary re-read or re-edit: nothing new was learned or changed.
            if !seen_outputs.insert(output.clone()) {
                unnecessary_ops += 1;
            }
            if !output_log.is_empty() {
                output_log.push('\n');
            }
            output_log.push_str(&output);
            step_index += 1;
        }

        let tool_calls = step_index;
        // No live clock/timer is available during a deterministic replay; one simulated second
        // per scripted step is the run's wall-clock proxy.
        let wall_seconds = step_index;
        // Cost tracks context volume in the absence of a real pricing signal from the provider
        // adapter (`SeededProvider::step` reports only output text, per its contract).
        let cost_micros = context_bytes;

        let outcome = task
            .expected
            .predicate
            .evaluate(&mut |p| evaluate_leaf(p, &output_log));
        let passed = outcome.is_satisfied();

        let scoring = &task.scoring;
        let expected = &task.expected;
        let success_score = if passed { 1.0 } else { 0.0 };
        let cost_score = ceiling_subscore(cost_micros, expected.max_cost_micros);
        let latency_score = ceiling_subscore(wall_seconds as u64, expected.max_wall_seconds as u64);
        let tool_count_score = ceiling_subscore(tool_calls as u64, expected.max_tool_calls as u64);
        let context_score = ceiling_subscore(context_bytes, expected.max_context_bytes);
        let unnecessary_score = unnecessary_ops_subscore(unnecessary_ops);

        let score = scoring.success_weight * success_score
            + scoring.cost_weight * cost_score
            + scoring.latency_weight * latency_score
            + scoring.tool_count_weight * tool_count_score
            + scoring.context_weight * context_score
            + scoring.unnecessary_ops_weight * unnecessary_score;

        Ok(TaskResult {
            task_id: task.id.clone(),
            passed,
            cost_micros,
            wall_seconds,
            tool_calls,
            context_bytes,
            unnecessary_ops,
            score,
        })
    }

    /// Run every task in `tasks` and fold the results into one [`BenchmarkReport`].
    ///
    /// Tasks with a `test_command` but no scripted steps (live-only tasks) are skipped by the
    /// scripted runner. The aggregate score is computed over only the tasks that ran.
    pub fn run_all(
        &self,
        tasks: &[BenchTask],
        provider: &dyn SeededProvider,
        epoch: u64,
    ) -> Result<BenchmarkReport> {
        let mut results = Vec::with_capacity(tasks.len());
        for task in tasks {
            // Skip live-only tasks (those with test_command but no scripted steps).
            if task.fixture.test_command.is_some() && provider.is_finished(task, 0) {
                tracing::warn!(task_id = %task.id, "skipping live-only task in scripted runner");
                continue;
            }
            results.push(self.run(task, provider)?);
        }
        let aggregate_score = if results.is_empty() {
            0.0
        } else {
            results.iter().map(|r| r.score).sum::<f64>() / results.len() as f64
        };
        Ok(BenchmarkReport {
            epoch,
            tasks: results,
            aggregate_score,
            generated_at: self.clock.now(),
        })
    }

    /// Score `fixture` via [`evaluate_decisions`], stamping the report's `generated_at` from this
    /// runner's injected clock rather than the pure function's `Timestamp::EPOCH` default — the
    /// entry point a `tm bench` decision-eval task calls, matching [`BenchRunner::run_all`]'s
    /// shape for the scripted-task path.
    pub fn run_decision_eval(
        &self,
        fixture: &DecisionEvalFixture,
        bucket_count: usize,
    ) -> DecisionEvalReport {
        DecisionEvalReport {
            generated_at: self.clock.now(),
            ..evaluate_decisions(fixture, bucket_count)
        }
    }
}

/// The aggregate result of running a benchmark suite against one harness epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BenchmarkReport {
    /// The harness epoch number this run scored.
    pub epoch: u64,
    /// Per-task results, in the order the tasks were run.
    pub tasks: Vec<TaskResult>,
    /// Mean of `tasks[*].score`.
    pub aggregate_score: f64,
    /// When this report was produced.
    pub generated_at: Timestamp,
}

/// The result of comparing two epochs' [`BenchmarkReport`]s, per `SPEC.md`'s `tm bench compare A B`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionReport {
    /// The baseline epoch's number.
    pub baseline_epoch: u64,
    /// The candidate epoch's number.
    pub candidate_epoch: u64,
    /// `candidate.aggregate_score - baseline.aggregate_score`.
    pub aggregate_gain: f64,
    /// Per-task score deltas, `(task_id, candidate_score - baseline_score)`, for tasks present in
    /// both reports.
    pub task_deltas: Vec<(String, f64)>,
    /// Whether `aggregate_gain` was positive (informational; [`crate::epoch::PromotionGate`] is
    /// what actually governs promotion).
    pub candidate_improved: bool,
}

/// One recorded `classify.decided(shadow)` decision paired with the outcome it would have
/// predicted, for offline decision-eval scoring (D-020's "Why" section: shadow decisions are
/// scored against `Session.promote`, a `StepRecord`, or a ticket end-state before any site is
/// allowed to act on them). `predicted_label`/`actual_label` are opaque strings so this crate
/// doesn't need to know the shape of a triage kind, a route, or any other decider question —
/// only whether the two matched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecord {
    /// The decider's shadow answer.
    pub predicted_label: String,
    /// The decider's confidence in `predicted_label`, expected in `[0.0, 1.0]`; a value outside
    /// that range is clamped before scoring rather than rejected, since a fixture is static data
    /// and a bench run should degrade gracefully rather than fail on a bad recording.
    pub confidence: f64,
    /// What actually happened, read back from `Session.promote`/`StepRecord`/the ticket's
    /// end-state.
    pub actual_label: String,
}

/// A fixture of recorded `classify.decided(shadow)` decisions and their outcomes. Entirely
/// offline and hermetic: this crate does not depend on `tm-provider` or `tm-events`, so a
/// fixture's `predicted_label`/`confidence` are a flattened stand-in for what a caller would pull
/// out of one `classify.decided` event's `answers` payload (`tm_provider::decide::Answer`'s
/// `value`/`confidence`, per `crates/tm-events/src/payload.rs`'s `ClassifyDecidedPayload`) and
/// its matching outcome — produced upstream (e.g. by `tm_provider::MockDecisionProvider` against
/// scripted requests) and recorded here as plain data. Nothing in this module calls a decider,
/// live or mocked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionEvalFixture {
    /// Stable fixture identifier, unique among decision-eval fixtures.
    pub id: String,
    /// The recorded decision/outcome pairs to score.
    pub records: Vec<DecisionRecord>,
}

impl DecisionEvalFixture {
    /// Parse one fixture from the contents of a decision-eval fixture TOML file.
    pub fn parse(source: &str) -> Result<DecisionEvalFixture> {
        toml::from_str::<DecisionEvalFixture>(source)
            .map_err(|e| tm_types::TmError::parse(e.to_string()))
    }
}

/// Accuracy/calibration report for one [`DecisionEvalFixture`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DecisionEvalReport {
    /// The fixture this report scored.
    pub fixture_id: String,
    /// Number of records the fixture contained.
    pub sample_count: usize,
    /// Fraction of records where `predicted_label == actual_label`.
    pub accuracy: f64,
    /// Expected calibration error: records are bucketed by `confidence` into `bucket_count`
    /// equal-width bins, and each bucket's `|mean_confidence - empirical_accuracy|` is averaged,
    /// weighted by bucket size. Lower is better-calibrated; `0.0` on an empty fixture or when
    /// `bucket_count` is `0` (accuracy is unaffected either way — it doesn't bucket).
    pub ece: f64,
    /// When this report was produced; `Timestamp::EPOCH` from the pure [`evaluate_decisions`],
    /// overwritten by an injected clock when scored through [`BenchRunner::run_decision_eval`].
    pub generated_at: Timestamp,
}

/// Score `fixture` against its recorded outcomes: accuracy plus expected calibration error
/// (ECE) over `bucket_count` equal-width confidence buckets. Pure and deterministic — no network,
/// no clock, no live decider — so a `tm bench` decision-eval task can call this directly against
/// a fixture of `classify.decided(shadow)` recordings (D-020 consequence: "a `tm bench` decision
/// task scored against `Session.promote` and `StepRecord`"). Prefer
/// [`BenchRunner::run_decision_eval`] when a report's `generated_at` should reflect the run.
pub fn evaluate_decisions(
    fixture: &DecisionEvalFixture,
    bucket_count: usize,
) -> DecisionEvalReport {
    let sample_count = fixture.records.len();
    let correct = fixture
        .records
        .iter()
        .filter(|r| r.predicted_label == r.actual_label)
        .count();
    let accuracy = if sample_count == 0 {
        0.0
    } else {
        correct as f64 / sample_count as f64
    };

    let ece = if sample_count == 0 || bucket_count == 0 {
        0.0
    } else {
        // (sum of confidences, count correct, count total) per bucket.
        let mut buckets = vec![(0.0f64, 0usize, 0usize); bucket_count];
        for record in &fixture.records {
            // A NaN confidence (never expected, but TOML can express `nan`) can't be clamped
            // meaningfully; treat it as no confidence at all rather than letting it propagate
            // into the report.
            let confidence = if record.confidence.is_nan() {
                0.0
            } else {
                record.confidence.clamp(0.0, 1.0)
            };
            let mut bucket_index = (confidence * bucket_count as f64) as usize;
            if bucket_index >= bucket_count {
                bucket_index = bucket_count - 1;
            }
            let bucket = &mut buckets[bucket_index];
            bucket.0 += confidence;
            bucket.2 += 1;
            if record.predicted_label == record.actual_label {
                bucket.1 += 1;
            }
        }

        buckets
            .iter()
            .filter(|(_, _, count)| *count > 0)
            .map(|(confidence_sum, correct, count)| {
                let mean_confidence = confidence_sum / *count as f64;
                let bucket_accuracy = *correct as f64 / *count as f64;
                let weight = *count as f64 / sample_count as f64;
                weight * (mean_confidence - bucket_accuracy).abs()
            })
            .sum()
    };

    DecisionEvalReport {
        fixture_id: fixture.id.clone(),
        sample_count,
        accuracy,
        ece,
        generated_at: Timestamp::EPOCH,
    }
}

/// Compare two epochs' benchmark reports.
pub fn compare(baseline: &BenchmarkReport, candidate: &BenchmarkReport) -> PromotionReport {
    let aggregate_gain = candidate.aggregate_score - baseline.aggregate_score;

    let mut task_deltas = Vec::with_capacity(candidate.tasks.len());
    for cand_task in &candidate.tasks {
        match baseline
            .tasks
            .iter()
            .find(|t| t.task_id == cand_task.task_id)
        {
            Some(base_task) => {
                task_deltas.push((cand_task.task_id.clone(), cand_task.score - base_task.score));
            }
            None => {
                tracing::warn!(
                    task_id = %cand_task.task_id,
                    "task present in candidate epoch but absent from baseline; omitted from task_deltas"
                );
            }
        }
    }
    for base_task in &baseline.tasks {
        if !candidate
            .tasks
            .iter()
            .any(|t| t.task_id == base_task.task_id)
        {
            tracing::warn!(
                task_id = %base_task.task_id,
                "task present in baseline epoch but absent from candidate; omitted from task_deltas"
            );
        }
    }

    PromotionReport {
        baseline_epoch: baseline.epoch,
        candidate_epoch: candidate.epoch,
        aggregate_gain,
        task_deltas,
        candidate_improved: aggregate_gain > 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::FixedClock;

    fn full_weight_scoring() -> ScoringSpec {
        ScoringSpec {
            success_weight: 1.0,
            cost_weight: 0.0,
            latency_weight: 0.0,
            tool_count_weight: 0.0,
            context_weight: 0.0,
            unnecessary_ops_weight: 0.0,
        }
    }

    fn task_with(
        _predicate: Predicate,
        expected: ExpectedOutcome,
        scoring: ScoringSpec,
    ) -> BenchTask {
        BenchTask {
            id: "t-1".to_string(),
            task: "do the thing".to_string(),
            fixture: BenchFixture {
                path: "fixtures/t-1".to_string(),
                description: "starting state".to_string(),
                test_command: None,
                setup_commands: Vec::new(),
            },
            scoring,
            expected,
        }
    }

    fn generous_outcome(predicate: Predicate) -> ExpectedOutcome {
        ExpectedOutcome {
            predicate,
            max_cost_micros: 1_000_000,
            max_wall_seconds: 1_000,
            max_tool_calls: 1_000,
            max_context_bytes: 1_000_000,
        }
    }

    /// Scripted steps taken verbatim from a fixed `Vec<String>`.
    struct ScriptedProvider {
        outputs: Vec<String>,
    }

    impl SeededProvider for ScriptedProvider {
        fn step(&self, _task: &BenchTask, step_index: u32) -> Result<String> {
            self.outputs
                .get(step_index as usize)
                .cloned()
                .ok_or_else(|| tm_types::TmError::invariant("step index out of range"))
        }

        fn is_finished(&self, _task: &BenchTask, step_index: u32) -> bool {
            step_index as usize >= self.outputs.len()
        }
    }

    struct FailingProvider;

    impl SeededProvider for FailingProvider {
        fn step(&self, _task: &BenchTask, _step_index: u32) -> Result<String> {
            Err(tm_types::TmError::storage("boom"))
        }

        fn is_finished(&self, _task: &BenchTask, step_index: u32) -> bool {
            step_index > 0
        }
    }

    #[test]
    fn parse_reads_a_well_formed_task_toml() {
        let toml = r#"
            id = "t-1"
            task = "make it green"
            [fixture]
            path = "fixtures/t-1"
            description = "starting state"
            [scoring]
            success_weight = 1.0
            cost_weight = 0.0
            latency_weight = 0.0
            tool_count_weight = 0.0
            context_weight = 0.0
            unnecessary_ops_weight = 0.0
            [expected]
            max_cost_micros = 100
            max_wall_seconds = 10
            max_tool_calls = 5
            max_context_bytes = 1000
            [expected.predicate]
            file_exists = { path = "src/lib.rs" }
        "#;
        let task = BenchTask::parse(toml).expect("well-formed task parses");
        assert_eq!(task.id, "t-1");
        assert_eq!(task.fixture.path, "fixtures/t-1");
    }

    #[test]
    fn parse_rejects_an_unknown_field() {
        let toml = r#"
            id = "t-1"
            task = "make it green"
            bogus = "field"
            [fixture]
            path = "fixtures/t-1"
            description = "starting state"
            [scoring]
            success_weight = 1.0
            cost_weight = 0.0
            latency_weight = 0.0
            tool_count_weight = 0.0
            context_weight = 0.0
            unnecessary_ops_weight = 0.0
            [expected]
            max_cost_micros = 100
            max_wall_seconds = 10
            max_tool_calls = 5
            max_context_bytes = 1000
            [expected.predicate]
            file_exists = { path = "src/lib.rs" }
        "#;
        assert!(BenchTask::parse(toml).is_err());
    }

    #[test]
    fn parse_rejects_malformed_toml() {
        assert!(BenchTask::parse("not = [valid").is_err());
    }

    #[test]
    fn run_passes_when_the_script_attests_the_predicate() {
        let predicate = Predicate::FileExists {
            path: "src/lib.rs".to_string(),
        };
        let task = task_with(
            predicate.clone(),
            generous_outcome(predicate),
            full_weight_scoring(),
        );
        let provider = ScriptedProvider {
            outputs: vec!["wrote src/lib.rs".to_string()],
        };
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::seeded(1);
        let runner = BenchRunner {
            clock: &clock,
            ids: &ids,
        };
        let result = runner.run(&task, &provider).expect("scripted run succeeds");
        assert!(result.passed);
        assert_eq!(result.tool_calls, 1);
        assert_eq!(result.score, 1.0);
    }

    #[test]
    fn run_fails_when_the_script_never_attests_the_predicate() {
        let predicate = Predicate::FileExists {
            path: "src/lib.rs".to_string(),
        };
        let task = task_with(
            predicate.clone(),
            generous_outcome(predicate),
            full_weight_scoring(),
        );
        let provider = ScriptedProvider {
            outputs: vec!["did something else entirely".to_string()],
        };
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::seeded(1);
        let runner = BenchRunner {
            clock: &clock,
            ids: &ids,
        };
        let result = runner.run(&task, &provider).expect("scripted run succeeds");
        assert!(!result.passed);
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn run_counts_a_repeated_step_output_as_an_unnecessary_op() {
        let predicate = Predicate::FileExists {
            path: "src/lib.rs".to_string(),
        };
        let task = task_with(
            predicate.clone(),
            generous_outcome(predicate),
            full_weight_scoring(),
        );
        let provider = ScriptedProvider {
            outputs: vec!["read src/lib.rs".to_string(), "read src/lib.rs".to_string()],
        };
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::seeded(1);
        let runner = BenchRunner {
            clock: &clock,
            ids: &ids,
        };
        let result = runner.run(&task, &provider).expect("scripted run succeeds");
        assert_eq!(result.unnecessary_ops, 1);
    }

    #[test]
    fn run_scores_zero_cost_credit_once_cost_exceeds_ceiling() {
        let predicate = Predicate::FileExists {
            path: "x".to_string(),
        };
        let mut expected = generous_outcome(predicate.clone());
        expected.max_cost_micros = 1;
        let scoring = ScoringSpec {
            success_weight: 0.0,
            cost_weight: 1.0,
            latency_weight: 0.0,
            tool_count_weight: 0.0,
            context_weight: 0.0,
            unnecessary_ops_weight: 0.0,
        };
        let task = task_with(predicate, expected, scoring);
        let provider = ScriptedProvider {
            outputs: vec!["x".repeat(1000)],
        };
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::seeded(1);
        let runner = BenchRunner {
            clock: &clock,
            ids: &ids,
        };
        let result = runner.run(&task, &provider).expect("scripted run succeeds");
        assert!(
            result.score < 0.01,
            "score should collapse well past the ceiling: {}",
            result.score
        );
    }

    #[test]
    fn run_propagates_a_provider_step_error_as_provider_error() {
        let predicate = Predicate::FileExists {
            path: "x".to_string(),
        };
        let task = task_with(
            predicate.clone(),
            generous_outcome(predicate),
            full_weight_scoring(),
        );
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::seeded(1);
        let runner = BenchRunner {
            clock: &clock,
            ids: &ids,
        };
        let err = runner.run(&task, &FailingProvider).unwrap_err();
        assert!(matches!(err, tm_types::TmError::Provider(_)));
    }

    #[test]
    fn run_all_aggregates_the_mean_of_per_task_scores() {
        let predicate = Predicate::FileExists {
            path: "src/lib.rs".to_string(),
        };
        let passing = task_with(
            predicate.clone(),
            generous_outcome(predicate.clone()),
            full_weight_scoring(),
        );
        let mut failing = task_with(
            predicate.clone(),
            generous_outcome(predicate),
            full_weight_scoring(),
        );
        failing.id = "t-2".to_string();

        let provider = ScriptedProvider {
            outputs: vec!["wrote src/lib.rs".to_string()],
        };
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::seeded(1);
        let runner = BenchRunner {
            clock: &clock,
            ids: &ids,
        };
        // Both tasks share the same script, whose single step names `passing`'s fixture but not
        // any distinct fact for `failing`; reuse the predicate so both pass, giving a known mean.
        let report = runner
            .run_all(&[passing, failing], &provider, 7)
            .expect("both tasks run");
        assert_eq!(report.epoch, 7);
        assert_eq!(report.tasks.len(), 2);
        assert!((report.aggregate_score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn run_all_reports_generated_at_from_the_injected_clock() {
        let predicate = Predicate::FileExists {
            path: "x".to_string(),
        };
        let task = task_with(
            predicate.clone(),
            generous_outcome(predicate),
            full_weight_scoring(),
        );
        let provider = ScriptedProvider {
            outputs: vec!["x".to_string()],
        };
        let clock = FixedClock::new(Timestamp::from_unix_seconds(42));
        let ids = tm_types::TestIds::seeded(1);
        let runner = BenchRunner {
            clock: &clock,
            ids: &ids,
        };
        let report = runner.run_all(&[task], &provider, 0).unwrap();
        assert_eq!(report.generated_at, Timestamp::from_unix_seconds(42));
    }

    fn report(epoch: u64, tasks: Vec<TaskResult>) -> BenchmarkReport {
        let aggregate_score =
            tasks.iter().map(|t| t.score).sum::<f64>() / tasks.len().max(1) as f64;
        BenchmarkReport {
            epoch,
            tasks,
            aggregate_score,
            generated_at: Timestamp::EPOCH,
        }
    }

    fn task_result(id: &str, score: f64) -> TaskResult {
        TaskResult {
            task_id: id.to_string(),
            passed: score >= 1.0,
            cost_micros: 0,
            wall_seconds: 0,
            tool_calls: 0,
            context_bytes: 0,
            unnecessary_ops: 0,
            score,
        }
    }

    #[test]
    fn compare_reports_a_positive_gain_when_the_candidate_improves() {
        let baseline = report(1, vec![task_result("t-1", 0.5)]);
        let candidate = report(2, vec![task_result("t-1", 0.9)]);
        let promotion = compare(&baseline, &candidate);
        assert!((promotion.aggregate_gain - 0.4).abs() < 1e-9);
        assert!(promotion.candidate_improved);
        assert_eq!(promotion.task_deltas, vec![("t-1".to_string(), 0.4)]);
    }

    #[test]
    fn compare_reports_no_improvement_when_the_candidate_regresses() {
        let baseline = report(1, vec![task_result("t-1", 0.9)]);
        let candidate = report(2, vec![task_result("t-1", 0.5)]);
        let promotion = compare(&baseline, &candidate);
        assert!(promotion.aggregate_gain < 0.0);
        assert!(!promotion.candidate_improved);
    }

    #[test]
    fn compare_omits_tasks_present_on_only_one_side() {
        let baseline = report(1, vec![task_result("only-baseline", 0.5)]);
        let candidate = report(2, vec![task_result("only-candidate", 0.5)]);
        let promotion = compare(&baseline, &candidate);
        assert!(promotion.task_deltas.is_empty());
    }

    #[test]
    fn parse_hello_world_fixture_unchanged() {
        let hello_world_toml = include_str!("../../../bench/tasks/hello-world.toml");
        let task = BenchTask::parse(hello_world_toml).expect("hello-world.toml parses");
        assert_eq!(task.id, "hello-world");
        assert_eq!(task.fixture.test_command, None);
        assert_eq!(task.fixture.setup_commands, Vec::<Vec<String>>::new());
    }

    #[test]
    fn parse_fixture_with_test_command_and_setup_commands() {
        let toml = r#"
            id = "t-live"
            task = "live test"
            [fixture]
            path = "fixtures/t-live"
            description = "a live-only task"
            test_command = ["cargo", "test"]
            setup_commands = [["cargo", "build"]]
            [scoring]
            success_weight = 1.0
            cost_weight = 0.0
            latency_weight = 0.0
            tool_count_weight = 0.0
            context_weight = 0.0
            unnecessary_ops_weight = 0.0
            [expected]
            max_cost_micros = 100
            max_wall_seconds = 10
            max_tool_calls = 5
            max_context_bytes = 1000
            [expected.predicate]
            file_exists = { path = "test.rs" }
        "#;
        let task = BenchTask::parse(toml).expect("well-formed task parses");
        assert_eq!(
            task.fixture.test_command,
            Some(vec!["cargo".to_string(), "test".to_string()])
        );
        assert_eq!(
            task.fixture.setup_commands,
            vec![vec!["cargo".to_string(), "build".to_string()]]
        );
    }

    #[test]
    fn run_all_skips_live_only_tasks() {
        // Provider that has no steps for task t-live but has steps for t-scripted.
        struct SelectiveProvider;
        impl SeededProvider for SelectiveProvider {
            fn step(&self, task: &BenchTask, _step_index: u32) -> Result<String> {
                match task.id.as_str() {
                    "t-scripted" => Ok("t-scripted confirmed".to_string()),
                    _ => Err(tm_types::TmError::not_found("step", "N/A")),
                }
            }
            fn is_finished(&self, task: &BenchTask, step_index: u32) -> bool {
                match task.id.as_str() {
                    "t-scripted" => step_index > 0,
                    _ => true, // No scripted steps: finished immediately, even at index 0.
                }
            }
        }

        let predicate = Predicate::FileExists {
            path: "t-scripted".to_string(),
        };

        // Scripted task: will run normally
        let mut scripted = task_with(
            predicate.clone(),
            generous_outcome(predicate.clone()),
            full_weight_scoring(),
        );
        scripted.id = "t-scripted".to_string();

        // Live-only task: has test_command but no script
        let mut live_only = task_with(
            predicate,
            generous_outcome(Predicate::FileExists {
                path: "live".to_string(),
            }),
            full_weight_scoring(),
        );
        live_only.id = "t-live".to_string();
        live_only.fixture.test_command = Some(vec!["cargo".to_string(), "test".to_string()]);

        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::seeded(1);
        let runner = BenchRunner {
            clock: &clock,
            ids: &ids,
        };

        let provider = SelectiveProvider;
        let report = runner
            .run_all(&[scripted, live_only], &provider, 0)
            .expect("run_all succeeds");

        // Only the scripted task should be in the report (live-only was skipped)
        assert_eq!(report.tasks.len(), 1);
        assert_eq!(report.tasks[0].task_id, "t-scripted");
        assert!(report.tasks[0].passed);
    }

    fn decision(predicted: &str, confidence: f64, actual: &str) -> DecisionRecord {
        DecisionRecord {
            predicted_label: predicted.to_string(),
            confidence,
            actual_label: actual.to_string(),
        }
    }

    #[test]
    fn decision_eval_fixture_parses_from_toml() {
        let toml = r#"
            id = "shadow-triage-1"
            [[records]]
            predicted_label = "bug"
            confidence = 0.9
            actual_label = "bug"
            [[records]]
            predicted_label = "feature"
            confidence = 0.4
            actual_label = "bug"
        "#;
        let fixture = DecisionEvalFixture::parse(toml).expect("well-formed fixture parses");
        assert_eq!(fixture.id, "shadow-triage-1");
        assert_eq!(fixture.records.len(), 2);
        assert_eq!(fixture.records[0].predicted_label, "bug");
    }

    #[test]
    fn decision_eval_fixture_parse_rejects_an_unknown_field() {
        let toml = r#"
            id = "shadow-triage-1"
            bogus = "field"
            [[records]]
            predicted_label = "bug"
            confidence = 0.9
            actual_label = "bug"
        "#;
        assert!(DecisionEvalFixture::parse(toml).is_err());
    }

    #[test]
    fn evaluate_decisions_reports_zero_for_an_empty_fixture() {
        let fixture = DecisionEvalFixture {
            id: "empty".to_string(),
            records: Vec::new(),
        };
        let report = evaluate_decisions(&fixture, 10);
        assert_eq!(report.sample_count, 0);
        assert_eq!(report.accuracy, 0.0);
        assert_eq!(report.ece, 0.0);
    }

    #[test]
    fn evaluate_decisions_computes_accuracy_even_with_zero_buckets() {
        // bucket_count == 0 disables ECE, but accuracy doesn't bucket at all and should still be
        // reported.
        let fixture = DecisionEvalFixture {
            id: "no-buckets".to_string(),
            records: vec![decision("bug", 0.9, "bug"), decision("bug", 0.9, "feature")],
        };
        let report = evaluate_decisions(&fixture, 0);
        assert_eq!(report.sample_count, 2);
        assert!((report.accuracy - 0.5).abs() < 1e-9);
        assert_eq!(report.ece, 0.0);
    }

    #[test]
    fn evaluate_decisions_scores_accuracy_as_the_match_fraction() {
        // Fully offline and hermetic: every record here is a plain, pre-recorded
        // classify.decided(shadow) + outcome pair, standing in for what
        // `tm_provider::MockDecisionProvider` would have produced upstream -- no network, no
        // clock, no live decider is touched by this eval.
        let fixture = DecisionEvalFixture {
            id: "shadow-triage".to_string(),
            records: vec![
                decision("bug", 0.9, "bug"),
                decision("bug", 0.8, "bug"),
                decision("feature", 0.6, "bug"),
                decision("chore", 0.7, "chore"),
            ],
        };
        let report = evaluate_decisions(&fixture, 10);
        assert_eq!(report.sample_count, 4);
        assert!((report.accuracy - 0.75).abs() < 1e-9);
    }

    #[test]
    fn evaluate_decisions_weights_ece_by_each_buckets_confidence_accuracy_gap() {
        // Two buckets (< 0.5 and >= 0.5), two records each, equally weighted (0.5 apiece). The
        // high-confidence bucket is all correct (mean confidence 0.9, accuracy 1.0, gap 0.1); the
        // low-confidence bucket is all wrong (mean confidence 0.2, accuracy 0.0, gap 0.2). ECE is
        // the weighted sum of those gaps: 0.5*0.1 + 0.5*0.2 = 0.15.
        let fixture = DecisionEvalFixture {
            id: "calibration".to_string(),
            records: vec![
                decision("bug", 0.9, "bug"),
                decision("bug", 0.9, "bug"),
                decision("feature", 0.2, "bug"),
                decision("feature", 0.2, "bug"),
            ],
        };
        let report = evaluate_decisions(&fixture, 2);
        assert!(
            (report.ece - 0.15).abs() < 1e-9,
            "expected ece close to 0.15, got {}",
            report.ece
        );
    }

    #[test]
    fn evaluate_decisions_clamps_out_of_range_confidence_instead_of_panicking() {
        let fixture = DecisionEvalFixture {
            id: "bad-confidence".to_string(),
            records: vec![
                decision("bug", 1.5, "bug"),
                decision("bug", -0.5, "feature"),
                decision("bug", f64::NAN, "bug"),
            ],
        };
        let report = evaluate_decisions(&fixture, 4);
        assert_eq!(report.sample_count, 3);
        assert!(report.ece.is_finite());
    }

    #[test]
    fn run_decision_eval_parses_and_scores_a_fixture_end_to_end() {
        // The acceptance path: a decision-eval fixture (standing in for recorded
        // classify.decided(shadow) + outcome pairs, D-020) parsed from TOML and scored through
        // the same BenchRunner entry point tm bench would call, with zero network calls and a
        // deterministic clock stamping the report.
        let toml = r#"
            id = "shadow-triage-1"
            [[records]]
            predicted_label = "bug"
            confidence = 0.9
            actual_label = "bug"
            [[records]]
            predicted_label = "feature"
            confidence = 0.4
            actual_label = "bug"
            [[records]]
            predicted_label = "chore"
            confidence = 0.7
            actual_label = "chore"
        "#;
        let fixture = DecisionEvalFixture::parse(toml).expect("well-formed fixture parses");
        let clock = FixedClock::new(Timestamp::from_unix_seconds(99));
        let ids = tm_types::TestIds::seeded(1);
        let runner = BenchRunner {
            clock: &clock,
            ids: &ids,
        };
        let report = runner.run_decision_eval(&fixture, 5);
        assert_eq!(report.fixture_id, "shadow-triage-1");
        assert_eq!(report.sample_count, 3);
        assert!((report.accuracy - (2.0 / 3.0)).abs() < 1e-9);
        assert_eq!(report.generated_at, Timestamp::from_unix_seconds(99));
    }
}
