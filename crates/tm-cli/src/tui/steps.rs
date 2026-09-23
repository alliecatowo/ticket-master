//! Translate the agent's step records into the chat screen's plain transcript data.
//!
//! `tm-tui` never sees a `tm_agent::StepRecord`: this module is the one place that decides what
//! of a step is worth showing — the model's text, and each tool call reduced to a status glyph,
//! its salient argument (the command, the path, the query), and a short outcome.

use serde_json::Value;
use tm_agent::outcome::{AgentOutcome, StepRecord, ToolCallRecord, ToolCallResolution};
use tm_tui::chat::transcript::{Entry, NoticeLevel, ToolCallView, ToolStatus};
use tm_tui::screens::chat::TurnUpdate;

/// How many output lines a failing command shows inline.
const PREVIEW_LINES: usize = 3;
/// The longest salient argument kept (the transcript wraps it; this only bounds pathological
/// inputs like a whole file passed as a command).
const MAX_TARGET_CHARS: usize = 400;

/// The cumulative progress update for a turn whose steps so far are `steps`, with `approvals`
/// (auto-denied approval notices, each tagged with how many steps had completed when it fired)
/// interleaved at the point they happened.
pub(crate) fn progress(steps: &[StepRecord], approvals: &[(usize, Entry)]) -> TurnUpdate {
    let mut entries = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        entries.extend(
            approvals
                .iter()
                .filter(|(at, _)| *at == i)
                .map(|(_, e)| e.clone()),
        );
        entries.extend(step_entries(step));
    }
    entries.extend(
        approvals
            .iter()
            .filter(|(at, _)| *at >= steps.len())
            .map(|(_, e)| e.clone()),
    );
    let served_by = steps
        .iter()
        .rev()
        .map(|s| s.served_by.as_str())
        .find(|s| !s.is_empty() && *s != "resumed")
        .map(str::to_string);
    TurnUpdate::Progress {
        entries,
        served_by,
        tokens: steps.iter().map(|s| s.spend.tokens).sum(),
        activity: Some("Thinking".to_string()),
    }
}

/// One step's transcript entries: its text (if any), then one entry per tool call.
pub(crate) fn step_entries(step: &StepRecord) -> Vec<Entry> {
    let mut out = Vec::new();
    if let Some(text) = &step.assistant_text {
        if !text.trim().is_empty() {
            out.push(Entry::Assistant(text.trim_end().to_string()));
        }
    }
    out.extend(step.tool_calls.iter().map(|c| Entry::Tool(tool_view(c))));
    out
}

/// A tool call reduced to what the transcript shows.
pub(crate) fn tool_view(call: &ToolCallRecord) -> ToolCallView {
    let target = tool_target(&call.tool_name, &call.input);
    let (status, detail, preview) = match &call.resolution {
        ToolCallResolution::Completed { result, .. } => match exit_code(result) {
            Some(0) => (ToolStatus::Ok, Some("exit 0".to_string()), Vec::new()),
            Some(code) => (
                ToolStatus::Failed,
                Some(format!("exit {code}")),
                output_tail(result),
            ),
            None => (ToolStatus::Ok, result_summary(result), Vec::new()),
        },
        ToolCallResolution::Denied { reason } => (
            ToolStatus::Denied,
            Some(reason.trim().to_string()),
            Vec::new(),
        ),
        ToolCallResolution::Errored { detail } => (
            ToolStatus::Failed,
            Some(detail.trim().to_string()),
            Vec::new(),
        ),
    };
    ToolCallView {
        status,
        name: call.tool_name.clone(),
        target,
        detail,
        preview,
    }
}

fn exit_code(result: &Value) -> Option<i64> {
    result.get("exit_code").and_then(Value::as_i64)
}

/// The last few non-blank lines of a failing command's stderr (or stdout, when stderr is empty).
fn output_tail(result: &Value) -> Vec<String> {
    let pick = |key: &str| {
        result
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    };
    let Some(text) = pick("stderr").or_else(|| pick("stdout")) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(PREVIEW_LINES)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

/// `12 results` for a result that is (or wraps) a list; nothing otherwise.
fn result_summary(result: &Value) -> Option<String> {
    let list = match result {
        Value::Array(items) => Some(items.len()),
        Value::Object(map) => ["results", "matches", "hits", "entries", "items", "tickets"]
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_array).map(Vec::len)),
        _ => None,
    }?;
    Some(match list {
        1 => "1 result".to_string(),
        n => format!("{n} results"),
    })
}

/// The one argument that says what a call was about.
pub(crate) fn tool_target(name: &str, input: &Value) -> String {
    let s = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
    let argv = || {
        input.get("argv").and_then(Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(shell_word)
                .collect::<Vec<_>>()
                .join(" ")
        })
    };
    let target = match name {
        "shell.run" | "test.run" | "build.run" => s("command").or_else(argv),
        "ticket.transition" => match (s("ticket"), s("to")) {
            (Some(t), Some(to)) => Some(format!("{t} → {to}")),
            (t, _) => t,
        },
        n if n.starts_with("search.") || n.starts_with("history.") => s("query")
            .or_else(|| s("pattern"))
            .map(|q| format!("\"{q}\""))
            .or_else(|| s("path")),
        n if n.starts_with("fs.") || n.starts_with("edit.") => s("path"),
        n if n.starts_with("ticket.") => s("ticket").or_else(|| s("objective")),
        _ => [
            "command",
            "path",
            "query",
            "pattern",
            "ticket",
            "url",
            "name",
            "symbol_id",
            "objective",
            "message",
            "summary",
        ]
        .iter()
        .find_map(|k| s(k))
        .or_else(argv),
    };
    let target = target.unwrap_or_default();
    if target.chars().count() > MAX_TARGET_CHARS {
        let cut: String = target.chars().take(MAX_TARGET_CHARS).collect();
        format!("{cut}…")
    } else {
        target
    }
}

/// Quote an argv word for display only when it needs it.
fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The closing notice for a finished turn, and whether it counts as a failure.
pub(crate) fn outcome_notice(outcome: &AgentOutcome) -> (Option<(NoticeLevel, String)>, bool) {
    match outcome {
        AgentOutcome::Replied { .. } => (None, false),
        AgentOutcome::Submitted { evidence, .. } => (
            Some((
                NoticeLevel::Success,
                format!("Submitted {}: {}", evidence.ticket, evidence.summary),
            )),
            false,
        ),
        AgentOutcome::BudgetExhausted { exhausted, .. } => (
            Some((
                NoticeLevel::Warning,
                format!(
                    "Stopped: the {} budget for this turn ran out.",
                    crate::agent::format_budget_dimension(*exhausted)
                ),
            )),
            true,
        ),
        AgentOutcome::Failed { class, detail, .. } => (
            Some((
                NoticeLevel::Error,
                format!("The turn failed ({class:?}): {detail}"),
            )),
            true,
        ),
        AgentOutcome::AwaitingApproval { pending_call, .. } => (
            Some((
                NoticeLevel::Warning,
                crate::agent::format_pending_approval(pending_call),
            )),
            true,
        ),
    }
}

/// The notice shown when the model asks for something that needs approval: the TUI cannot
/// prompt for it yet, so it is denied — said plainly, with the way around it.
pub(crate) fn approval_notice(tool: &str, reason: &str) -> Entry {
    Entry::Notice {
        level: NoticeLevel::Warning,
        text: format!(
            "{tool} needs approval ({reason}). Denied automatically: the TUI cannot ask yet. \
             Run `tm --plain` to approve interactively."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tm_types::{Spend, Timestamp};

    fn call(name: &str, input: Value, resolution: ToolCallResolution) -> ToolCallRecord {
        ToolCallRecord {
            tool_use_id: "t1".to_string(),
            tool_name: name.to_string(),
            input,
            resolution,
        }
    }

    fn completed(result: Value) -> ToolCallResolution {
        ToolCallResolution::Completed {
            result,
            artifact: None,
        }
    }

    fn step(text: Option<&str>, calls: Vec<ToolCallRecord>, tokens: u64) -> StepRecord {
        StepRecord {
            index: 1,
            served_by: "devpass/muse-spark".to_string(),
            assistant_text: text.map(str::to_string),
            tool_calls: calls,
            spend: Spend {
                tokens,
                ..Spend::default()
            },
            at: Timestamp::EPOCH,
        }
    }

    #[test]
    fn a_passing_command_shows_its_command_and_exit_code() {
        let view = tool_view(&call(
            "shell.run",
            json!({"command": "python3 -m pytest -q"}),
            completed(json!({"exit_code": 0, "stdout": "3 passed", "stderr": ""})),
        ));
        assert_eq!(view.status, ToolStatus::Ok);
        assert_eq!(view.target, "python3 -m pytest -q");
        assert_eq!(view.detail.as_deref(), Some("exit 0"));
        assert!(view.preview.is_empty());
    }

    #[test]
    fn a_failing_command_is_a_failure_with_the_tail_of_its_output() {
        let view = tool_view(&call(
            "test.run",
            json!({"argv": ["pytest", "-q", "tests/test calc.py"]}),
            completed(json!({
                "exit_code": 1,
                "stdout": "",
                "stderr": "a\nb\n\nFAILED test_div\nZeroDivisionError\n1 failed\n"
            })),
        ));
        assert_eq!(view.status, ToolStatus::Failed);
        assert_eq!(view.target, "pytest -q 'tests/test calc.py'");
        assert_eq!(view.detail.as_deref(), Some("exit 1"));
        assert_eq!(
            view.preview,
            vec!["FAILED test_div", "ZeroDivisionError", "1 failed"]
        );
    }

    #[test]
    fn paths_queries_and_errors_are_the_salient_parts() {
        let edit = tool_view(&call(
            "edit.apply_patch",
            json!({"path": "calc.py", "edits": []}),
            ToolCallResolution::Errored {
                detail: "patch did not apply".to_string(),
            },
        ));
        assert_eq!(edit.status, ToolStatus::Failed);
        assert_eq!(edit.target, "calc.py");
        assert_eq!(edit.detail.as_deref(), Some("patch did not apply"));

        let search = tool_view(&call(
            "search.hybrid",
            json!({"query": "divide by zero"}),
            completed(json!({"results": [1, 2, 3]})),
        ));
        assert_eq!(search.target, "\"divide by zero\"");
        assert_eq!(search.detail.as_deref(), Some("3 results"));

        let denied = tool_view(&call(
            "shell.run",
            json!({"command": "rm -rf /"}),
            ToolCallResolution::Denied {
                reason: "outside authority".to_string(),
            },
        ));
        assert_eq!(denied.status, ToolStatus::Denied);

        let transition = tool_target(
            "ticket.transition",
            &json!({"ticket": "T-3", "to": "activate"}),
        );
        assert_eq!(transition, "T-3 → activate");
        assert_eq!(tool_target("git.status", &json!({})), "");
    }

    #[test]
    fn progress_is_cumulative_and_interleaves_approvals() {
        let steps = vec![
            step(
                Some("Let me look."),
                vec![call(
                    "fs.read",
                    json!({"path": "a.rs"}),
                    completed(json!({})),
                )],
                100,
            ),
            step(Some("Done."), vec![], 50),
        ];
        let approvals = vec![(1, approval_notice("shell.run", "network access"))];
        let TurnUpdate::Progress {
            entries,
            served_by,
            tokens,
            ..
        } = progress(&steps, &approvals)
        else {
            panic!("progress builds a Progress update");
        };
        assert_eq!(entries.len(), 4);
        assert!(matches!(&entries[0], Entry::Assistant(t) if t == "Let me look."));
        assert!(matches!(&entries[1], Entry::Tool(v) if v.target == "a.rs"));
        assert!(matches!(
            &entries[2],
            Entry::Notice {
                level: NoticeLevel::Warning,
                ..
            }
        ));
        assert!(matches!(&entries[3], Entry::Assistant(t) if t == "Done."));
        assert_eq!(served_by.as_deref(), Some("devpass/muse-spark"));
        assert_eq!(tokens, 150);
    }

    #[test]
    fn a_plain_reply_needs_no_closing_notice() {
        let (notice, failed) = outcome_notice(&AgentOutcome::Replied {
            text: "hi".to_string(),
            steps: Vec::new(),
        });
        assert!(notice.is_none());
        assert!(!failed);
        let (notice, failed) = outcome_notice(&AgentOutcome::Failed {
            steps: Vec::new(),
            class: tm_core::FailureClass::Other,
            detail: "provider unreachable".to_string(),
        });
        assert!(failed);
        assert!(notice
            .is_some_and(|(level, text)| level == NoticeLevel::Error
                && text.contains("provider unreachable")));
    }
}
