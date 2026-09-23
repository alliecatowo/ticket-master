//! Translate the agent's step records into the chat screen's plain transcript data.
//!
//! `tm-tui` never sees a `tm_agent::StepRecord`: this module is the one place that decides what
//! of a step is worth showing — the model's text, and each tool call reduced to a status glyph,
//! its salient argument (the command, the path, the query), and a short outcome.

use serde_json::Value;
use tm_agent::outcome::{AgentOutcome, StepRecord, ToolCallRecord, ToolCallResolution};
use tm_tui::chat::approval::{ApprovalChoice, ApprovalRequest};
use tm_tui::chat::diff::{parse_unified, DiffLine, DiffLineKind};
use tm_tui::chat::transcript::{
    tool_label, Entry, NoticeLevel, ToolBody, ToolCallView, ToolStatus,
};
use tm_tui::screens::chat::TurnUpdate;

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

/// A tool call reduced to what the transcript shows (`docs/decisions/D-019-claude-code-parity-
/// shell.md`): commands as `Bash` output, reads as `Read N lines`, edits as a numbered diff.
/// The full input and output ride along for the transcript viewer (Ctrl+O).
pub(crate) fn tool_view(call: &ToolCallRecord) -> ToolCallView {
    let name = call.tool_name.as_str();
    let target = tool_target(name, &call.input);
    let input = pretty(&call.input);
    let mut view = match &call.resolution {
        ToolCallResolution::Completed { result, .. } => completed_view(name, &target, result),
        ToolCallResolution::Denied { reason } => ToolCallView {
            detail: Some(reason.trim().to_string()),
            ..ToolCallView::new(ToolStatus::Denied, name, target.clone())
        },
        ToolCallResolution::Errored { detail } => ToolCallView {
            detail: Some(detail.trim().to_string()),
            output: detail.clone(),
            ..ToolCallView::new(ToolStatus::Failed, name, target.clone())
        },
    };
    view.name = name.to_string();
    view.target = target;
    view.input = input;
    view
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// `n thing` / `n things`.
fn plural(n: u64, thing: &str) -> String {
    if n == 1 {
        format!("1 {thing}")
    } else {
        format!("{n} {thing}s")
    }
}

/// A result too big to return inline was stored as an artifact
/// (`tm_agent::tools::MAX_INLINE_RESULT_BYTES`); only a byte preview of its JSON came back.
fn stored_artifact(result: &Value) -> Option<&str> {
    if result.get("truncated").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    result.get("artifact").and_then(Value::as_str)
}

fn completed_view(name: &str, target: &str, result: &Value) -> ToolCallView {
    let output = pretty(result);
    let base = |status| ToolCallView {
        output: output.clone(),
        ..ToolCallView::new(status, name, target)
    };

    if let Some(code) = result.get("exit_code").and_then(Value::as_i64) {
        let text = |key: &str| result.get(key).and_then(Value::as_str).unwrap_or_default();
        let mut view = ToolCallView::command(name, target, code, text("stdout"), text("stderr"));
        if result.get("truncated").and_then(Value::as_bool) == Some(true) {
            view.output.push_str(
                "\n[output was truncated in the middle; the full streams are in artifacts]",
            );
        }
        return view;
    }

    if let Some(artifact) = stored_artifact(result) {
        let what = if name.starts_with("edit.") {
            "Changed"
        } else {
            "Returned"
        };
        return ToolCallView {
            body: ToolBody::Summary(format!("{what} more than can be shown (stored as {artifact})")),
            ..base(ToolStatus::Ok)
        };
    }

    match name {
        "fs.read" | "fs.read_range" => {
            let lines = result
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, |c| c.lines().count());
            ToolCallView {
                body: ToolBody::Summary(format!("Read {}", plural(lines as u64, "line"))),
                ..base(ToolStatus::Ok)
            }
        }
        "fs.list" => {
            let n = result
                .get("entries")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            ToolCallView {
                body: ToolBody::Summary(format!("Listed {}", plural(n as u64, "path"))),
                ..base(ToolStatus::Ok)
            }
        }
        "edit.apply_patch" => {
            // `{path, edits: [{applied, patch | error}]}`: one patch per applied edit, in order,
            // stopping at the first that failed. Later hunks' line numbers are against the file
            // as the earlier edits left it, which is what the human wants to see.
            let edits = result
                .get("edits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let patches: Vec<&Value> = edits.iter().filter_map(|e| e.get("patch")).collect();
            let error = edits.iter().find_map(|e| {
                (e.get("applied").and_then(Value::as_bool) == Some(false)).then(|| {
                    e.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("the edit did not apply")
                        .to_string()
                })
            });
            match error {
                Some(error) => ToolCallView {
                    detail: Some(if patches.is_empty() {
                        error
                    } else {
                        format!(
                            "{} of {} applied, then: {error}",
                            patches.len(),
                            edits.len()
                        )
                    }),
                    ..base(ToolStatus::Failed)
                },
                None => ToolCallView {
                    body: diff_body(target, &patches),
                    ..base(ToolStatus::Ok)
                },
            }
        }
        n if n.starts_with("edit.") => match result.get("applied").and_then(Value::as_bool) {
            Some(false) => ToolCallView {
                detail: Some(
                    result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("the edit did not apply")
                        .to_string(),
                ),
                ..base(ToolStatus::Failed)
            },
            _ => {
                let patches: Vec<&Value> = result.get("patch").into_iter().collect();
                ToolCallView {
                    body: diff_body(target, &patches),
                    ..base(ToolStatus::Ok)
                }
            }
        },
        _ => {
            let count = result_count(result);
            let body = match count {
                Some(n) if name.starts_with("search.") || name.starts_with("history.") => {
                    ToolBody::Summary(format!("Found {}", plural(n as u64, "result")))
                }
                _ => ToolBody::None,
            };
            ToolCallView {
                detail: count.map(|n| plural(n as u64, "result")),
                body,
                ..base(ToolStatus::Ok)
            }
        }
    }
}

/// The body for one or more applied patches (`tm_agent::tools`'s `patch_json` shape).
fn diff_body(path: &str, patches: &[&Value]) -> ToolBody {
    let count = |p: &Value, key: &str| p.get(key).and_then(Value::as_u64).unwrap_or(0);
    let flag = |p: &Value, key: &str| p.get(key).and_then(Value::as_bool) == Some(true);
    let added: u64 = patches.iter().map(|p| count(p, "lines_added")).sum();
    let removed: u64 = patches.iter().map(|p| count(p, "lines_removed")).sum();
    let mut lines = Vec::new();
    for patch in patches {
        let parsed = patch
            .get("unified_diff")
            .and_then(Value::as_str)
            .map(parse_unified)
            .unwrap_or_default();
        if !lines.is_empty() && !parsed.is_empty() {
            lines.push(DiffLine {
                kind: DiffLineKind::Gap,
                number: None,
                text: String::new(),
            });
        }
        lines.extend(parsed);
    }
    let summary = if patches.iter().any(|p| flag(p, "deleted")) {
        return ToolBody::Summary(format!("Deleted {path}"));
    } else if patches.iter().any(|p| flag(p, "created")) {
        format!("Wrote {} to {path}", plural(added, "line"))
    } else {
        format!(
            "Updated {path} with {} and {}",
            plural(added, "addition"),
            plural(removed, "removal")
        )
    };
    ToolBody::Diff { summary, lines }
}

/// How many items a result that is (or wraps) a list holds.
fn result_count(result: &Value) -> Option<usize> {
    match result {
        Value::Array(items) => Some(items.len()),
        Value::Object(map) => ["results", "matches", "hits", "entries", "items", "tickets"]
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_array).map(Vec::len)),
        _ => None,
    }
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
        AgentOutcome::Interrupted { .. } => (
            Some((NoticeLevel::Warning, "Interrupted.".to_string())),
            false,
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

/// What the permission prompt shows for a pending call.
pub(crate) fn approval_request(pending: &tm_agent::outcome::PendingApproval) -> ApprovalRequest {
    ApprovalRequest {
        tool: pending.tool_name.clone(),
        target: tool_target(&pending.tool_name, &pending.input),
        reason: pending.reason.trim().to_string(),
    }
}

/// The transcript's record of how the human answered a permission prompt, placed where the
/// question came up.
pub(crate) fn approval_notice(tool: &str, target: &str, choice: ApprovalChoice) -> Entry {
    let what = if target.is_empty() {
        tool_label(tool)
    } else {
        format!("{}({target})", tool_label(tool))
    };
    let (level, text) = match choice {
        ApprovalChoice::Yes => (NoticeLevel::Info, format!("Allowed {what}")),
        ApprovalChoice::YesForSession => (
            NoticeLevel::Info,
            format!("Allowed {what}, and won't ask again this session"),
        ),
        ApprovalChoice::No => (
            NoticeLevel::Warning,
            format!("Declined {what}. Tell tm what to do instead."),
        ),
    };
    Entry::Notice { level, text }
}

/// A saved conversation's turns as transcript entries, for a resumed session (`tm -c`,
/// `tm -r`, `/resume`): each prompt, then its steps; a `!` command the human ran shows as the
/// shell block it was.
pub(crate) fn conversation_entries(turns: &[tm_agent::ConversationTurn]) -> Vec<Entry> {
    let mut out = Vec::new();
    for turn in turns {
        let message = turn.user_message.trim();
        if let Some(view) = shell_turn_view(message) {
            out.push(Entry::Shell(view));
        } else if message.starts_with("[Request interrupted") {
            out.push(Entry::Notice {
                level: NoticeLevel::Warning,
                text: "Interrupted.".to_string(),
            });
        } else if !message.is_empty() {
            out.push(Entry::User(message.to_string()));
        }
        for step in &turn.steps {
            out.extend(step_entries(step));
        }
    }
    out
}

/// The text between `<tag>` and `</tag>` in `text`.
fn tagged<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&close)?;
    Some(&text[start..end])
}

/// A `!` command recorded into the conversation (`agent::shell_context_message`'s tags), back as
/// the shell block it was shown as.
fn shell_turn_view(message: &str) -> Option<ToolCallView> {
    let command = tagged(message, "bash-input")?;
    let code = tagged(message, "bash-exit-code")
        .and_then(|c| c.trim().parse::<i64>().ok())
        .unwrap_or(0);
    Some(ToolCallView::command(
        "shell",
        command,
        code,
        tagged(message, "bash-stdout").unwrap_or_default(),
        tagged(message, "bash-stderr").unwrap_or_default(),
    ))
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
    fn a_passing_command_carries_its_output() {
        let view = tool_view(&call(
            "shell.run",
            json!({"command": "python3 -m pytest -q"}),
            completed(json!({"exit_code": 0, "stdout": "3 passed\n", "stderr": ""})),
        ));
        assert_eq!(view.status, ToolStatus::Ok);
        assert_eq!(view.target, "python3 -m pytest -q");
        assert_eq!(view.detail, None);
        assert_eq!(view.body, ToolBody::Output(vec!["3 passed".to_string()]));
        assert!(view.input.contains("\"command\""), "{}", view.input);
    }

    #[test]
    fn a_failing_command_is_a_failure_with_all_of_its_output() {
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
        assert_eq!(view.detail.as_deref(), Some("Exit code 1"));
        let ToolBody::Output(lines) = &view.body else {
            panic!("a command's body is its output");
        };
        assert_eq!(lines.len(), 6);
        assert_eq!(lines.last().map(String::as_str), Some("1 failed"));
    }

    #[test]
    fn a_read_counts_its_lines_and_a_list_its_paths() {
        let read = tool_view(&call(
            "fs.read",
            json!({"path": "calc.py"}),
            completed(json!({"path": "calc.py", "content": "a\nb\nc\n"})),
        ));
        assert_eq!(read.body, ToolBody::Summary("Read 3 lines".to_string()));
        let list = tool_view(&call(
            "fs.list",
            json!({"path": "."}),
            completed(json!({"path": ".", "entries": [{"name": "a"}]})),
        ));
        assert_eq!(list.body, ToolBody::Summary("Listed 1 path".to_string()));
    }

    const PATCH: &str = "--- a/calc.py\n+++ b/calc.py\n@@ -1,2 +1,3 @@\n def div(a, b):\n-    return a / b\n+    if b == 0: raise ValueError\n+    return a / b\n";

    #[test]
    fn an_applied_edit_becomes_a_diff_with_counts() {
        let view = tool_view(&call(
            "edit.write_file",
            json!({"path": "calc.py", "content": "..."}),
            completed(json!({"applied": true, "patch": {
                "path": "calc.py", "unified_diff": PATCH, "lines_added": 2,
                "lines_removed": 1, "created": false, "deleted": false,
            }})),
        ));
        assert_eq!(view.status, ToolStatus::Ok);
        let ToolBody::Diff { summary, lines } = &view.body else {
            panic!("an edit's body is a diff, got {:?}", view.body);
        };
        assert_eq!(summary, "Updated calc.py with 2 additions and 1 removal");
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn a_multi_edit_patch_shows_every_hunk_and_a_failed_one_is_red() {
        let patch = json!({"unified_diff": PATCH, "lines_added": 2, "lines_removed": 1});
        let view = tool_view(&call(
            "edit.apply_patch",
            json!({"path": "calc.py", "edits": []}),
            completed(json!({"path": "calc.py", "edits": [
                {"applied": true, "patch": patch.clone()},
                {"applied": true, "patch": patch.clone()},
            ]})),
        ));
        let ToolBody::Diff { summary, lines } = &view.body else {
            panic!("got {:?}", view.body);
        };
        assert_eq!(summary, "Updated calc.py with 4 additions and 2 removals");
        assert_eq!(lines.len(), 9, "two hunks and the gap between them");

        let failed = tool_view(&call(
            "edit.apply_patch",
            json!({"path": "calc.py", "edits": []}),
            completed(json!({"path": "calc.py", "edits": [
                {"applied": true, "patch": patch},
                {"applied": false, "error": "hash mismatch"},
            ]})),
        ));
        assert_eq!(failed.status, ToolStatus::Failed);
        assert_eq!(
            failed.detail.as_deref(),
            Some("1 of 2 applied, then: hash mismatch")
        );

        let refused = tool_view(&call(
            "edit.create_file",
            json!({"path": "x.py"}),
            completed(json!({"applied": false, "error": "already exists"})),
        ));
        assert_eq!(refused.status, ToolStatus::Failed);
        assert_eq!(refused.detail.as_deref(), Some("already exists"));
    }

    #[test]
    fn a_created_file_says_how_many_lines_were_written() {
        let view = tool_view(&call(
            "edit.create_file",
            json!({"path": "new.py"}),
            completed(json!({"applied": true, "patch": {
                "unified_diff": "@@ -0,0 +1,2 @@\n+a\n+b\n", "lines_added": 2,
                "lines_removed": 0, "created": true,
            }})),
        ));
        let ToolBody::Diff { summary, .. } = &view.body else {
            panic!("got {:?}", view.body);
        };
        assert_eq!(summary, "Wrote 2 lines to new.py");
    }

    #[test]
    fn an_oversized_result_says_where_it_went() {
        let view = tool_view(&call(
            "fs.read",
            json!({"path": "big.log"}),
            completed(json!({"truncated": true, "artifact": "ART-1", "preview": "{\"con"})),
        ));
        assert_eq!(view.status, ToolStatus::Ok);
        assert!(
            matches!(&view.body, ToolBody::Summary(s) if s.contains("ART-1")),
            "{:?}",
            view.body
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
        assert_eq!(search.body, ToolBody::Summary("Found 3 results".to_string()));

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
        let approvals = vec![(
            1,
            approval_notice("shell.run", "git push", ApprovalChoice::No),
        )];
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
    fn a_resumed_conversation_renders_prompts_steps_and_shell_commands() {
        let turns = vec![
            tm_agent::ConversationTurn {
                user_message: "fix it".to_string(),
                steps: vec![step(Some("Done."), vec![], 10)],
            },
            tm_agent::ConversationTurn {
                user_message: "<bash-input>ls</bash-input>\n<bash-exit-code>0</bash-exit-code>\n<bash-stdout>a.txt</bash-stdout>\n".to_string(),
                steps: Vec::new(),
            },
        ];
        let entries = conversation_entries(&turns);
        assert!(matches!(&entries[0], Entry::User(t) if t == "fix it"));
        assert!(matches!(&entries[1], Entry::Assistant(t) if t == "Done."));
        let Entry::Shell(view) = &entries[2] else {
            panic!("a recorded ! command comes back as a shell block: {entries:?}");
        };
        assert_eq!(view.target, "ls");
        assert_eq!(view.body, ToolBody::Output(vec!["a.txt".to_string()]));
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
