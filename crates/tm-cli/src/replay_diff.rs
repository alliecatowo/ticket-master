//! `tm harness replay-diff <a> <b>`: a structural diff of two session transcripts.
//!
//! The comparison itself ([`diff_sessions`]) is a pure function of two [`SessionTranscript`]
//! values, unit-tested directly against hand-built fixtures per `SPEC.md` §0 — no network, no
//! model calls. [`load_session_transcript`] (reading the JSON) and [`run_replay_diff`] (wiring
//! that into a rendered result) are the only IO layered on top, matching `crate::stats`'s
//! pure-fold-then-render split.
//!
//! A "session transcript" here is the on-disk shape a saved chat session already writes
//! (`crate::agent`'s `SavedSession`, `<state_dir>/sessions/<id>.json`: `turns: Vec<ConversationTurn>`)
//! plus an optional `harness_epoch`, so this command also reads a future cassette-header-style
//! file or hand-built fixture that carries one. Today's saved sessions don't stamp an epoch, so
//! it deserializes as `None` and the epoch check is simply skipped for them.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tm_agent::outcome::{ConversationTurn, StepRecord, ToolCallRecord, ToolCallResolution};
use tm_types::{Result, Spend, TmError};

use crate::args::HarnessReplayDiffArgs;
use crate::render::Renderer;

/// The on-disk shape this command reads: a saved session's turns, plus whichever harness epoch
/// (if any) the session was pinned to.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionTranscript {
    /// The harness epoch the session (or cassette recording) was pinned to, if the file carries
    /// one. Absent for today's saved chat sessions.
    #[serde(default)]
    pub harness_epoch: Option<u64>,
    /// Every completed turn, oldest first. Required: a JSON object with no `turns` field isn't a
    /// session transcript, and treating it as an empty one would make two unrelated files
    /// (a `{}`, a bench report) silently "diff" as identical.
    pub turns: Vec<ConversationTurn>,
}

/// One difference found between two transcripts, anchored to where it occurred.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDifference {
    /// 0-based index of the turn the difference belongs to.
    pub turn_index: usize,
    /// 1-based step index within the turn ([`StepRecord::index`]), or `None` for a turn-level
    /// mismatch (e.g. one transcript has more steps than the other).
    pub step_index: Option<u32>,
    /// What differed, as a plain sentence.
    pub detail: String,
}

/// The full result of comparing two [`SessionTranscript`]s.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDiff {
    /// `a`'s harness epoch, if its file carried one.
    pub epoch_a: Option<u64>,
    /// `b`'s harness epoch, if its file carried one.
    pub epoch_b: Option<u64>,
    /// Whether both transcripts carried a harness epoch and they differ.
    pub epoch_mismatch: bool,
    /// Every difference found, in transcript order.
    pub differences: Vec<SessionDifference>,
}

impl SessionDiff {
    /// True when the two transcripts are structurally identical (no epoch mismatch, no
    /// differences).
    pub fn is_empty(&self) -> bool {
        !self.epoch_mismatch && self.differences.is_empty()
    }
}

/// Compare two session transcripts. Pure: no IO, no clock, no randomness.
///
/// Walks the two transcripts' turns pairwise, then each pair's steps pairwise, reporting:
/// assistant-text mismatches, tool-call name/input/resolution mismatches, spend deltas, and
/// step- or turn-count mismatches. A harness epoch mismatch is reported separately via
/// [`SessionDiff::epoch_mismatch`] rather than as a [`SessionDifference`], since it describes the
/// whole run rather than one step.
pub fn diff_sessions(a: &SessionTranscript, b: &SessionTranscript) -> SessionDiff {
    let epoch_mismatch =
        matches!((a.harness_epoch, b.harness_epoch), (Some(ea), Some(eb)) if ea != eb);

    let mut differences = Vec::new();
    let turn_count = a.turns.len().max(b.turns.len());
    for turn_index in 0..turn_count {
        match (a.turns.get(turn_index), b.turns.get(turn_index)) {
            (Some(ta), Some(tb)) => diff_turn(turn_index, ta, tb, &mut differences),
            (Some(ta), None) => differences.push(SessionDifference {
                turn_index,
                step_index: None,
                detail: format!("turn only present in a, with {} step(s)", ta.steps.len()),
            }),
            (None, Some(tb)) => differences.push(SessionDifference {
                turn_index,
                step_index: None,
                detail: format!("turn only present in b, with {} step(s)", tb.steps.len()),
            }),
            // Unreachable in practice: `turn_index` is bounded by the longer transcript's
            // length, so at least one side always has an entry. Written as a no-op rather than
            // `unreachable!` so there is no panic path in a pure comparison function.
            (None, None) => {}
        }
    }

    SessionDiff {
        epoch_a: a.harness_epoch,
        epoch_b: b.harness_epoch,
        epoch_mismatch,
        differences,
    }
}

fn diff_turn(
    turn_index: usize,
    a: &ConversationTurn,
    b: &ConversationTurn,
    out: &mut Vec<SessionDifference>,
) {
    if a.steps.len() != b.steps.len() {
        out.push(SessionDifference {
            turn_index,
            step_index: None,
            detail: format!(
                "step count differs: {} in a vs {} in b",
                a.steps.len(),
                b.steps.len()
            ),
        });
    }
    let shared = a.steps.len().min(b.steps.len());
    for (sa, sb) in a.steps[..shared].iter().zip(&b.steps[..shared]) {
        diff_step(turn_index, sa, sb, out);
    }
}

fn diff_step(turn_index: usize, a: &StepRecord, b: &StepRecord, out: &mut Vec<SessionDifference>) {
    let step_index = Some(a.index);

    if a.assistant_text != b.assistant_text {
        out.push(SessionDifference {
            turn_index,
            step_index,
            detail: format!(
                "assistant text differs: {} vs {}",
                describe_text(a.assistant_text.as_deref()),
                describe_text(b.assistant_text.as_deref())
            ),
        });
    }

    if a.tool_calls.len() != b.tool_calls.len() {
        out.push(SessionDifference {
            turn_index,
            step_index,
            detail: format!(
                "tool call count differs: {} in a vs {} in b",
                a.tool_calls.len(),
                b.tool_calls.len()
            ),
        });
    }
    let shared = a.tool_calls.len().min(b.tool_calls.len());
    for (i, (ca, cb)) in a.tool_calls[..shared]
        .iter()
        .zip(&b.tool_calls[..shared])
        .enumerate()
    {
        diff_tool_call(turn_index, step_index, i, ca, cb, out);
    }

    if a.spend != b.spend {
        out.push(SessionDifference {
            turn_index,
            step_index,
            detail: format!(
                "spend differs: {} vs {}",
                describe_spend(&a.spend),
                describe_spend(&b.spend)
            ),
        });
    }
}

fn diff_tool_call(
    turn_index: usize,
    step_index: Option<u32>,
    call_index: usize,
    a: &ToolCallRecord,
    b: &ToolCallRecord,
    out: &mut Vec<SessionDifference>,
) {
    let call_number = call_index + 1;
    if a.tool_name != b.tool_name {
        out.push(SessionDifference {
            turn_index,
            step_index,
            detail: format!(
                "tool call {call_number} differs: {} vs {}",
                a.tool_name, b.tool_name
            ),
        });
        return;
    }
    if a.input != b.input {
        out.push(SessionDifference {
            turn_index,
            step_index,
            detail: format!("tool call {call_number} ({}) input differs", a.tool_name),
        });
    }
    if a.resolution != b.resolution {
        out.push(SessionDifference {
            turn_index,
            step_index,
            detail: format!(
                "tool call {call_number} ({}) result differs: {} vs {}",
                a.tool_name,
                describe_resolution(&a.resolution),
                describe_resolution(&b.resolution)
            ),
        });
    }
}

/// A person-readable one-liner for how a tool call resolved, with no internal enum/type names
/// and no `{:?}` dump of its payload.
fn describe_resolution(resolution: &ToolCallResolution) -> String {
    match resolution {
        ToolCallResolution::Completed { .. } => "completed".to_string(),
        ToolCallResolution::Denied { reason } => format!("denied ({reason})"),
        ToolCallResolution::Errored { detail } => format!("errored ({detail})"),
    }
}

/// A person-readable stand-in for optional assistant text — never `{:?}`'s `Some("...")`/`None`.
fn describe_text(text: Option<&str>) -> String {
    match text {
        Some(t) => format!("{t:?}"),
        None => "no text".to_string(),
    }
}

fn describe_spend(spend: &Spend) -> String {
    format!(
        "{} tokens, ${:.6}, {}s",
        spend.tokens,
        spend.dollars_micros as f64 / 1_000_000.0,
        spend.wall_seconds
    )
}

/// Read and parse a session transcript from `path`.
pub fn load_session_transcript(path: &Path) -> Result<SessionTranscript> {
    let bytes = std::fs::read(path).map_err(|e| {
        TmError::storage(format!(
            "Can't read the session file {}: {e}",
            path.display()
        ))
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        TmError::parse(format!(
            "{} doesn't look like a session transcript: {e}",
            path.display()
        ))
    })
}

/// `tm harness replay-diff <a> <b>`: load both session files, compare them, and render the
/// result.
pub fn run_replay_diff(args: &HarnessReplayDiffArgs, renderer: &Renderer) -> Result<()> {
    let a = load_session_transcript(&args.a)?;
    let b = load_session_transcript(&args.b)?;
    let diff = diff_sessions(&a, &b);
    render_diff(&args.a, &args.b, &diff, renderer)
}

fn render_diff(a: &Path, b: &Path, diff: &SessionDiff, renderer: &Renderer) -> Result<()> {
    if renderer.is_json() {
        return renderer.emit(diff, "");
    }

    if diff.is_empty() {
        renderer.note(&format!(
            "No differences between {} and {}.",
            a.display(),
            b.display()
        ));
        return Ok(());
    }

    let mut lines = Vec::new();
    if diff.epoch_mismatch {
        lines.push(format!(
            "Harness epoch differs: {} was pinned to epoch {}, {} to epoch {}.",
            a.display(),
            diff.epoch_a.unwrap_or_default(),
            b.display(),
            diff.epoch_b.unwrap_or_default()
        ));
    }
    for d in &diff.differences {
        // Turn and step numbers are shown 1-based, matching how a person counts turns and how
        // `StepRecord::index` already counts steps; only the struct fields stay 0-based/raw.
        let turn = d.turn_index + 1;
        match d.step_index {
            Some(step) => lines.push(format!("Turn {turn}, step {step}: {}", d.detail)),
            None => lines.push(format!("Turn {turn}: {}", d.detail)),
        }
    }
    renderer.emit(&(), &lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::Timestamp;

    fn step(index: u32, text: &str, tool_calls: Vec<ToolCallRecord>) -> StepRecord {
        StepRecord {
            index,
            served_by: "test".to_string(),
            assistant_text: Some(text.to_string()),
            tool_calls,
            spend: Spend::default(),
            at: Timestamp::EPOCH,
        }
    }

    fn tool_call(name: &str, resolution: ToolCallResolution) -> ToolCallRecord {
        ToolCallRecord {
            tool_use_id: "call-1".to_string(),
            tool_name: name.to_string(),
            input: serde_json::json!({}),
            resolution,
        }
    }

    fn completed(value: serde_json::Value) -> ToolCallResolution {
        ToolCallResolution::Completed {
            result: value,
            artifact: None,
        }
    }

    fn transcript(epoch: Option<u64>, steps: Vec<StepRecord>) -> SessionTranscript {
        SessionTranscript {
            harness_epoch: epoch,
            turns: vec![ConversationTurn {
                user_message: "do the thing".to_string(),
                steps,
            }],
        }
    }

    #[test]
    fn identical_step_vectors_give_an_empty_diff() {
        let steps = vec![step(
            1,
            "done",
            vec![tool_call("bash", completed(serde_json::json!("ok")))],
        )];
        let a = transcript(Some(3), steps.clone());
        let b = transcript(Some(3), steps);

        let diff = diff_sessions(&a, &b);

        assert!(diff.is_empty(), "expected no differences, got {diff:?}");
    }

    #[test]
    fn one_differing_tool_resolution_reports_exactly_that_step_index() {
        let a_steps = vec![
            step(1, "first", vec![]),
            step(
                2,
                "second",
                vec![tool_call("bash", completed(serde_json::json!("ok")))],
            ),
        ];
        let b_steps = vec![
            step(1, "first", vec![]),
            step(
                2,
                "second",
                vec![tool_call("bash", completed(serde_json::json!("different")))],
            ),
        ];
        let a = transcript(None, a_steps);
        let b = transcript(None, b_steps);

        let diff = diff_sessions(&a, &b);

        assert!(!diff.epoch_mismatch);
        assert_eq!(diff.differences.len(), 1);
        let only = &diff.differences[0];
        assert_eq!(only.turn_index, 0);
        assert_eq!(only.step_index, Some(2));
        assert!(only.detail.contains("result differs"));
    }

    #[test]
    fn differing_epochs_produce_the_warning() {
        let steps = vec![step(1, "same", vec![])];
        let a = transcript(Some(1), steps.clone());
        let b = transcript(Some(2), steps);

        let diff = diff_sessions(&a, &b);

        assert!(diff.epoch_mismatch);
        assert_eq!(diff.epoch_a, Some(1));
        assert_eq!(diff.epoch_b, Some(2));
        assert!(diff.differences.is_empty());
        assert!(!diff.is_empty());
    }

    #[test]
    fn a_missing_harness_epoch_on_either_side_never_warns() {
        let steps = vec![step(1, "same", vec![])];
        let a = transcript(Some(1), steps.clone());
        let b = transcript(None, steps);

        let diff = diff_sessions(&a, &b);

        assert!(!diff.epoch_mismatch);
    }

    #[test]
    fn step_count_mismatch_is_reported_at_the_turn_level() {
        let a = transcript(None, vec![step(1, "same", vec![])]);
        let b = transcript(
            None,
            vec![step(1, "same", vec![]), step(2, "second", vec![])],
        );

        let diff = diff_sessions(&a, &b);

        assert_eq!(diff.differences.len(), 1);
        assert_eq!(diff.differences[0].step_index, None);
        assert!(diff.differences[0].detail.contains("step count differs"));
    }

    #[test]
    fn load_session_transcript_rejects_a_missing_file() {
        let missing = std::path::Path::new("/nonexistent/does-not-exist.json");
        let err = load_session_transcript(missing).expect_err("missing file should error");
        assert!(matches!(err, TmError::Storage(_)));
    }

    #[test]
    fn a_json_object_with_no_turns_field_is_rejected_rather_than_treated_as_empty() {
        let err: Result<SessionTranscript> = serde_json::from_str("{}").map_err(TmError::parse);
        assert!(err.is_err(), "a session transcript requires `turns`");

        let err: Result<SessionTranscript> =
            serde_json::from_str(r#"{"foo":1}"#).map_err(TmError::parse);
        assert!(err.is_err());
    }

    /// `turns` deserializes from the same on-disk shape `crate::agent`'s `SavedSession` writes
    /// (`schema`/`session`/`started`/`updated`/`turns`) — this test builds that shape by hand
    /// since `SavedSession` itself is private to `agent.rs`. The extra fields it carries are
    /// simply ignored by `SessionTranscript`, which is the point: this command reads a
    /// compatible subset, not the exact struct.
    #[test]
    fn a_saved_session_shaped_object_parses() {
        let turn = ConversationTurn {
            user_message: "fix the bug".to_string(),
            steps: vec![step(1, "fixed it", vec![])],
        };
        let json = serde_json::json!({
            "schema": 1,
            "session": "S-1",
            "started": 0,
            "updated": 0,
            "turns": [turn],
        });

        let parsed: SessionTranscript = serde_json::from_value(json).expect("should parse");

        assert_eq!(parsed.harness_epoch, None);
        assert_eq!(parsed.turns.len(), 1);
        assert_eq!(parsed.turns[0].user_message, "fix the bug");
    }
}
