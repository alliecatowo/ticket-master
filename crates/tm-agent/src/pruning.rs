//! `SPEC.md` §30.4 "Pruning within a run" (`docs/audit-2026-09-18-fable.md` B-08): a pure,
//! deterministic function of a [`crate::outcome::StepRecord`] transcript that decides which
//! recorded tool results are still live and which are superseded, so
//! [`crate::agent_loop::rebuild_messages`] can render a *working set* instead of replaying every
//! step's tool output verbatim forever.
//!
//! Two things make a recorded tool result stale, and both are checked purely from `steps` — no
//! model judgement, no wall clock, no I/O, matching §30.4's "pruning is a deterministic function
//! of the event log, not a model judgement call":
//! - **Re-addressing**: a later call with the identical [`addressable_for`] key supersedes an
//!   earlier one — a second `fs.read` of the same path, the same `search.exact` query run again,
//!   the same failed `shell.run` retried. "Normalized" per §30.4 means whitespace-collapsed, not
//!   case-folded: case is semantically significant for `search.regex`/`search.exact` (`"Foo"` and
//!   `"foo"` are different queries, not the same one re-run), so only whitespace is normalized.
//! - **Staleness-by-edit**: an earlier path-backed read (`fs.read`, `fs.read_range`, `fs.stat`,
//!   `fs.list`, `symbol.outline`, `history.why`) goes stale the moment a *completed* `edit.*` call
//!   later writes that same path, even if nothing ever re-read it.
//!
//! Every lookup here goes through a `BTreeMap` keyed by string/step-order alone, and the output
//! `Vec` is built by a single forward pass over `steps` in its original order — never by
//! iterating a map to produce output — so nothing here can depend on hash-iteration order.
//! `crates/tm-scheduler/src/plan.rs`'s module docs spell out why that distinction matters for a
//! "same input, same output, forever" guarantee; this module follows the same discipline: no
//! `HashMap`/`HashSet` anywhere below.

use std::collections::BTreeMap;

use crate::outcome::{StepRecord, ToolCallResolution};

/// Whether one tool call's recorded result is still safe to re-send to the provider as-is, or has
/// been superseded and should render as a short stub instead (§30.4: "a superseded result is
/// dropped from the compiled context while remaining in the event log").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolCallState {
    /// Render this call's real result.
    Full,
    /// Render a `[superseded by step N]` stub instead. `by_step` is the earliest later step whose
    /// call made this one stale — either a re-addressing (a later call with the identical key)
    /// or a completed edit to the same path.
    Superseded {
        /// The step index that superseded this record.
        by_step: u32,
    },
}

/// One step's working-set annotation: the step itself plus a per-tool-call verdict, index-aligned
/// with `step.tool_calls`.
pub(crate) struct StepRef<'a> {
    /// The original step. Never cloned or rewritten: assistant text and every tool-use block
    /// (id/name/input) are always rendered in full regardless of pruning — only a tool *result*
    /// can be stubbed, since the wire format requires every `ToolResult` to stay paired with the
    /// `ToolUse` that requested it (§30.4's "tool-use/tool-result pairing" constraint).
    pub(crate) step: &'a StepRecord,
    /// `tool_call_states[i]` is the verdict for `step.tool_calls[i]`.
    pub(crate) tool_call_states: Vec<ToolCallState>,
}

/// The deterministic function of `steps` this module exists to compute: which tool results are
/// still live, and how many bytes/tokens re-sending the pruned ones instead of their stubs would
/// have cost (`docs/audit-2026-09-18-fable.md` B-08 item 5, feeding `SPEC.md` §30.2's
/// rent-accounting ledger a real number for working-set pruning specifically).
pub(crate) struct WorkingSet<'a> {
    /// Every input step, annotated, in the same order and count as `steps`.
    pub(crate) steps: Vec<StepRef<'a>>,
    /// Bytes saved by stubbing every `Superseded` result instead of rendering it in full: the
    /// sum, over every superseded call, of `full_text.len() - stub.len()`.
    pub(crate) bytes_pruned: u64,
    /// The same saving estimated in tokens, via [`tm_context::estimate_tokens_source`] — tool
    /// results are structured/code-shaped, the same assumption
    /// [`tm_context::ToolSurfaceCost::compute`] makes for tool schemas.
    pub(crate) tokens_pruned: u64,
}

/// A tool call's addressable identity under §30.4: the dedup key a later identical call
/// supersedes, and — for calls whose result depends on one filesystem path's content — which path
/// a later completed edit invalidates it against.
struct Addressable {
    key: String,
    path_dependency: Option<String>,
}

/// Collapse redundant whitespace without folding case (see module docs for why case is kept).
fn normalize_query(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn str_field<'a>(input: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    input.get(field).and_then(serde_json::Value::as_str)
}

/// `result_key` derivation per tool category (`docs/audit-2026-09-18-fable.md` B-08 item 1), read
/// off the exact input fields `crates/tm-agent/src/tools.rs`'s dispatcher parses for each
/// `ToolName`:
/// - `fs.read`/`fs.read_range`/`fs.stat`/`fs.list` and `symbol.outline`: the `path` field —
///   path-backed reads, so they also carry a `path_dependency` for the staleness-by-edit check.
/// - `symbol.definition`: `name`+`from_path`, the two fields `tools.rs` requires to resolve it;
///   there is no single `path` a caller could stat.
/// - `symbol.references`/`symbol.callers`/`symbol.callees`/`symbol.rename_preview`: `symbol_id`,
///   the only identifying field `tools.rs` reads for these (no path or name is available there).
/// - `history.why`: `path`+`line_start`+`line_end` (its answer is specific to that range); also
///   path-backed.
/// - `search.semantic`/`search.hybrid`/`search.exact`/`history.search`/`history.deleted`: the
///   normalized `query` field.
/// - `search.regex`: the normalized `pattern` field (its input has no `query` field).
/// - `shell.run`/`build.run`/`test.run`: a canonical key over `argv`+`cwd`, via the exact same
///   `tools.rs::command_argv`/`deterministic_command_key` the tool dispatcher itself uses to
///   resolve a call's effective argv (whether the caller passed `argv` or a `command` shell-line
///   string — the tool's own schema accepts either) and to key its own, unrelated output cache.
///   Reusing those functions directly (rather than re-parsing `command`/`argv` a second time here)
///   is what keeps a `{"command": "go test ./..."}` call de-duplicable the same way an
///   `{"argv": [...]}` call already was — pruning runs with no `CallContext`/root available, but
///   `cwd` resolution isn't needed for the dedup key itself, only the raw `cwd` string.
///
/// Every other tool (`edit.*`, `git.*`, `ticket.*`, `decision.record`, `artifact.store`,
/// `evidence.attach`, `ask.human`) returns `None`: each call is its own distinct action rather
/// than a re-checkable "view" of something, so it is never superseded by a later call.
fn addressable_for(tool_name: &str, input: &serde_json::Value) -> Option<Addressable> {
    match tool_name {
        "fs.read" | "fs.read_range" | "fs.stat" | "fs.list" => {
            let path = str_field(input, "path")?.to_string();
            Some(Addressable {
                key: format!("fs:{path}"),
                path_dependency: Some(path),
            })
        }
        "symbol.outline" => {
            let path = str_field(input, "path")?.to_string();
            Some(Addressable {
                key: format!("symbol.outline:{path}"),
                path_dependency: Some(path),
            })
        }
        "symbol.definition" => {
            let name = str_field(input, "name")?;
            let from_path = str_field(input, "from_path")?;
            Some(Addressable {
                key: format!("symbol.definition:{name}@{from_path}"),
                path_dependency: None,
            })
        }
        "symbol.references" | "symbol.callers" | "symbol.callees" | "symbol.rename_preview" => {
            let symbol_id = input.get("symbol_id")?;
            Some(Addressable {
                key: format!("{tool_name}:{symbol_id}"),
                path_dependency: None,
            })
        }
        "history.why" => {
            let path = str_field(input, "path")?.to_string();
            let line_start = input.get("line_start")?;
            let line_end = input.get("line_end")?;
            Some(Addressable {
                key: format!("history.why:{path}:{line_start}:{line_end}"),
                path_dependency: Some(path),
            })
        }
        "search.semantic" | "search.hybrid" | "search.exact" | "history.search"
        | "history.deleted" => {
            let query = normalize_query(str_field(input, "query")?);
            Some(Addressable {
                key: format!("{tool_name}:{query}"),
                path_dependency: None,
            })
        }
        "search.regex" => {
            let pattern = normalize_query(str_field(input, "pattern")?);
            Some(Addressable {
                key: format!("search.regex:{pattern}"),
                path_dependency: None,
            })
        }
        "shell.run" | "build.run" | "test.run" => {
            let argv = crate::tools::command_argv(input).ok()?;
            let cwd = str_field(input, "cwd").unwrap_or(".");
            let key = format!(
                "{tool_name}:{}",
                crate::tools::deterministic_command_key(&argv, cwd)
            );
            Some(Addressable {
                key,
                path_dependency: None,
            })
        }
        _ => None,
    }
}

/// The path an `edit.*` call targets, per the single `path` field every `edit.apply_patch`/
/// `edit.write_file`/`edit.create_file`/`edit.delete_file` variant reads in `tools.rs`.
fn edit_target_path(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    if tool_name.starts_with("edit.") {
        str_field(input, "path").map(str::to_string)
    } else {
        None
    }
}

/// Whether an `edit.*` call actually wrote its target path, as opposed to merely being dispatched
/// without error. `tools.rs`'s edit handlers report failure *inside* an `Ok` result (`{"applied":
/// false, "error": ...}`) rather than as a dispatch-level `Err`/`ToolCallResolution::Errored` (see
/// `EditApplyPatch`/`patch_outcome_json`), so a top-level `ToolCallResolution::Completed` is not
/// by itself evidence the file changed — the embedded `applied` flag(s) are.
fn edit_actually_wrote(tool_name: &str, resolution: &ToolCallResolution) -> bool {
    let ToolCallResolution::Completed { result, .. } = resolution else {
        return false;
    };
    match tool_name {
        "edit.apply_patch" => result
            .get("edits")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|edits| {
                edits
                    .iter()
                    .any(|e| e.get("applied").and_then(serde_json::Value::as_bool) == Some(true))
            }),
        "edit.write_file" | "edit.create_file" | "edit.delete_file" => {
            result.get("applied").and_then(serde_json::Value::as_bool) == Some(true)
        }
        _ => false,
    }
}

/// Fold `candidates` (positions paired with the step index they occurred at) into `acc`, keeping
/// the smallest step index among every candidate whose position is strictly after `my_position` —
/// i.e. the earliest later event that supersedes something at `my_position`.
fn earliest_after(
    candidates: &[(usize, u32)],
    my_position: usize,
    acc: Option<u32>,
) -> Option<u32> {
    candidates
        .iter()
        .filter(|&&(pos, _)| pos > my_position)
        .map(|&(_, step_idx)| step_idx)
        .fold(acc, |acc, step_idx| {
            Some(match acc {
                Some(s) => s.min(step_idx),
                None => step_idx,
            })
        })
}

/// Compute the working set: which of `steps`' tool results are still live, per §30.4.
///
/// Pure function of `steps` alone — no clock, no randomness, no hidden state — so the same
/// transcript produces byte-identical output every call, including across a suspend/resume
/// boundary ([`crate::agent_loop::AgentLoop::resume`] re-derives it from the same durable `steps`
/// a fresh [`crate::agent_loop::AgentLoop::run`] would have).
pub(crate) fn working_set(steps: &[StepRecord]) -> WorkingSet<'_> {
    // Pass 1: assign every tool call a sequential position (call order across the whole
    // transcript, not just within its step) and index, by key, every call's position plus its
    // step index -- the only two facts pass 2 needs, looked up, never iterated, so map iteration
    // order can never leak into the output.
    let mut by_key: BTreeMap<String, Vec<(usize, u32)>> = BTreeMap::new();
    let mut edits_by_path: BTreeMap<String, Vec<(usize, u32)>> = BTreeMap::new();
    let mut addressables: Vec<Vec<Option<Addressable>>> = Vec::with_capacity(steps.len());

    let mut position = 0usize;
    for step in steps {
        let mut step_addr = Vec::with_capacity(step.tool_calls.len());
        for tc in &step.tool_calls {
            let addr = addressable_for(&tc.tool_name, &tc.input);
            if let Some(a) = &addr {
                by_key
                    .entry(a.key.clone())
                    .or_default()
                    .push((position, step.index));
            }
            if let Some(path) = edit_target_path(&tc.tool_name, &tc.input) {
                if edit_actually_wrote(&tc.tool_name, &tc.resolution) {
                    edits_by_path
                        .entry(path)
                        .or_default()
                        .push((position, step.index));
                }
            }
            step_addr.push(addr);
            position += 1;
        }
        addressables.push(step_addr);
    }

    // Pass 2: for every tool call, find the earliest later position that supersedes it, built by
    // walking `steps` in order -- the output `Vec`'s order is `steps`' order, never a map's.
    let mut bytes_pruned = 0u64;
    let mut tokens_pruned = 0u64;
    let mut position = 0usize;
    let mut refs = Vec::with_capacity(steps.len());
    for (step, step_addr) in steps.iter().zip(addressables.iter()) {
        let mut states = Vec::with_capacity(step.tool_calls.len());
        for (tc, addr) in step.tool_calls.iter().zip(step_addr.iter()) {
            let my_position = position;
            position += 1;

            let mut superseded_by: Option<u32> = None;
            if let Some(addr) = addr {
                if let Some(candidates) = by_key.get(&addr.key) {
                    superseded_by = earliest_after(candidates, my_position, superseded_by);
                }
                if let Some(path) = &addr.path_dependency {
                    if let Some(candidates) = edits_by_path.get(path) {
                        superseded_by = earliest_after(candidates, my_position, superseded_by);
                    }
                }
            }

            let state = match superseded_by {
                Some(by_step) => {
                    let (full_text, _) = crate::agent_loop::tool_result_text(&tc.resolution);
                    let stub = format!("[superseded by step {by_step}]");
                    bytes_pruned += full_text.len().saturating_sub(stub.len()) as u64;
                    tokens_pruned += tm_context::estimate_tokens_source(&full_text)
                        .saturating_sub(tm_context::estimate_tokens_source(&stub))
                        as u64;
                    ToolCallState::Superseded { by_step }
                }
                None => ToolCallState::Full,
            };
            states.push(state);
        }
        refs.push(StepRef {
            step,
            tool_call_states: states,
        });
    }

    WorkingSet {
        steps: refs,
        bytes_pruned,
        tokens_pruned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::ToolCallRecord;
    use tm_types::{Spend, Timestamp};

    fn step(index: u32, text: Option<&str>, calls: Vec<ToolCallRecord>) -> StepRecord {
        StepRecord {
            index,
            served_by: "mock/mock".to_string(),
            assistant_text: text.map(str::to_string),
            tool_calls: calls,
            spend: Spend::default(),
            at: Timestamp::from_unix_nanos(0),
        }
    }

    fn read(id: &str, path: &str) -> ToolCallRecord {
        ToolCallRecord {
            tool_use_id: id.to_string(),
            tool_name: "fs.read".to_string(),
            input: serde_json::json!({"path": path}),
            resolution: ToolCallResolution::Completed {
                result: serde_json::json!({"path": path, "content": "x".repeat(200)}),
                artifact: None,
            },
        }
    }

    fn write(id: &str, path: &str, applied: bool) -> ToolCallRecord {
        ToolCallRecord {
            tool_use_id: id.to_string(),
            tool_name: "edit.write_file".to_string(),
            input: serde_json::json!({"path": path, "content": "new"}),
            resolution: ToolCallResolution::Completed {
                result: serde_json::json!({"applied": applied}),
                artifact: None,
            },
        }
    }

    fn search(id: &str, query: &str) -> ToolCallRecord {
        ToolCallRecord {
            tool_use_id: id.to_string(),
            tool_name: "search.exact".to_string(),
            input: serde_json::json!({"query": query}),
            resolution: ToolCallResolution::Completed {
                result: serde_json::json!({"hits": []}),
                artifact: None,
            },
        }
    }

    /// A `shell.run` call carrying the `command` shell-line form (no `argv` field), the shape
    /// `u1-shell-run-command-field-bypasses-pruning` was filed against.
    fn shell_run_command(id: &str, command: &str) -> ToolCallRecord {
        ToolCallRecord {
            tool_use_id: id.to_string(),
            tool_name: "shell.run".to_string(),
            input: serde_json::json!({"command": command}),
            resolution: ToolCallResolution::Completed {
                result: serde_json::json!({"exit_code": 0, "output": "x".repeat(200)}),
                artifact: None,
            },
        }
    }

    #[test]
    fn a_second_read_of_the_same_path_supersedes_the_first() {
        let steps = vec![
            step(1, None, vec![read("c1", "a.rs")]),
            step(2, None, vec![read("c2", "a.rs")]),
        ];
        let ws = working_set(&steps);
        assert_eq!(
            ws.steps[0].tool_call_states[0],
            ToolCallState::Superseded { by_step: 2 }
        );
        assert_eq!(ws.steps[1].tool_call_states[0], ToolCallState::Full);
        assert!(ws.bytes_pruned > 0);
        assert!(ws.tokens_pruned > 0);
    }

    #[test]
    fn a_read_before_a_completed_edit_to_the_same_path_goes_stale_without_a_reread() {
        let steps = vec![
            step(1, None, vec![read("c1", "a.rs")]),
            step(2, None, vec![write("c2", "a.rs", true)]),
        ];
        let ws = working_set(&steps);
        assert_eq!(
            ws.steps[0].tool_call_states[0],
            ToolCallState::Superseded { by_step: 2 }
        );
    }

    #[test]
    fn a_failed_edit_does_not_stale_an_earlier_read() {
        let steps = vec![
            step(1, None, vec![read("c1", "a.rs")]),
            step(2, None, vec![write("c2", "a.rs", false)]),
        ];
        let ws = working_set(&steps);
        assert_eq!(ws.steps[0].tool_call_states[0], ToolCallState::Full);
    }

    /// `u1-shell-run-command-field-bypasses-pruning`'s acceptance case: a `shell.run` call issued
    /// with a `command` shell-line string (no `argv` field, the natural form the tool's own
    /// description offers for build/test) must be superseded by an identical later call, the same
    /// as the already-covered `argv` form -- instead of falling through `addressable_for`'s
    /// `_ => None` arm and replaying the same capped-but-large result at every later step.
    #[test]
    fn a_second_identical_command_form_shell_run_supersedes_the_first() {
        let steps = vec![
            step(1, None, vec![shell_run_command("c1", "go test ./...")]),
            step(2, None, vec![shell_run_command("c2", "go test ./...")]),
        ];
        let ws = working_set(&steps);
        assert_eq!(
            ws.steps[0].tool_call_states[0],
            ToolCallState::Superseded { by_step: 2 }
        );
        assert_eq!(ws.steps[1].tool_call_states[0], ToolCallState::Full);
        assert!(ws.bytes_pruned > 0);
    }

    #[test]
    fn a_read_is_unaffected_by_an_edit_to_a_different_path() {
        let steps = vec![
            step(1, None, vec![read("c1", "a.rs")]),
            step(2, None, vec![write("c2", "b.rs", true)]),
        ];
        let ws = working_set(&steps);
        assert_eq!(ws.steps[0].tool_call_states[0], ToolCallState::Full);
    }

    #[test]
    fn a_repeated_search_query_supersedes_the_earlier_one() {
        let steps = vec![
            step(1, None, vec![search("c1", "TODO")]),
            step(2, None, vec![search("c2", "TODO")]),
        ];
        let ws = working_set(&steps);
        assert_eq!(
            ws.steps[0].tool_call_states[0],
            ToolCallState::Superseded { by_step: 2 }
        );
    }

    #[test]
    fn assistant_text_only_steps_are_never_pruned() {
        let steps = vec![step(1, Some("hello"), vec![])];
        let ws = working_set(&steps);
        assert_eq!(ws.steps[0].step.assistant_text.as_deref(), Some("hello"));
        assert!(ws.steps[0].tool_call_states.is_empty());
    }

    #[test]
    fn unaddressable_tools_like_ticket_submit_are_never_superseded() {
        let submit = ToolCallRecord {
            tool_use_id: "c1".to_string(),
            tool_name: "ticket.submit".to_string(),
            input: serde_json::json!({"summary": "done"}),
            resolution: ToolCallResolution::Completed {
                result: serde_json::json!({}),
                artifact: None,
            },
        };
        let steps = vec![
            step(1, None, vec![submit.clone()]),
            step(2, None, vec![submit]),
        ];
        let ws = working_set(&steps);
        assert_eq!(ws.steps[0].tool_call_states[0], ToolCallState::Full);
        assert_eq!(ws.steps[1].tool_call_states[0], ToolCallState::Full);
    }

    #[test]
    fn search_regex_keys_are_case_sensitive() {
        let a = addressable_for("search.regex", &serde_json::json!({"pattern": "Foo"})).unwrap();
        let b = addressable_for("search.regex", &serde_json::json!({"pattern": "foo"})).unwrap();
        assert_ne!(a.key, b.key);
    }

    #[test]
    fn search_query_keys_collapse_whitespace_only() {
        let a = addressable_for(
            "search.exact",
            &serde_json::json!({"query": "  foo   bar "}),
        )
        .unwrap();
        let b = addressable_for("search.exact", &serde_json::json!({"query": "foo bar"})).unwrap();
        assert_eq!(a.key, b.key);
    }

    #[test]
    fn shell_run_keys_are_argv_and_cwd_exact() {
        let a = addressable_for(
            "shell.run",
            &serde_json::json!({"argv": ["cargo", "test"], "cwd": "crates/tm-agent"}),
        )
        .unwrap();
        let b = addressable_for(
            "shell.run",
            &serde_json::json!({"argv": ["cargo", "test"], "cwd": "crates/tm-core"}),
        )
        .unwrap();
        assert_ne!(a.key, b.key, "different cwd must not collide");

        let c = addressable_for(
            "shell.run",
            &serde_json::json!({"argv": ["cargo", "test"], "cwd": "crates/tm-agent"}),
        )
        .unwrap();
        assert_eq!(a.key, c.key, "identical argv+cwd must collide");

        // Regression pin: the argv-form key must stay byte-identical to the old, pre-fix
        // formula now that it's derived via `tools.rs::deterministic_command_key` instead of
        // being built inline -- no behavior change for calls already using the argv form.
        let d = addressable_for(
            "shell.run",
            &serde_json::json!({"argv": ["cargo", "test"], "cwd": "."}),
        )
        .unwrap();
        assert_eq!(d.key, "shell.run:cargo\u{1}test\u{1}\u{1e}.");
    }

    /// The bug this module exists to fix: a `shell.run`/`build.run`/`test.run` call issued with a
    /// `command` shell-line string (no `argv` field at all — the natural form for build/test, per
    /// `tools.rs::command_argv`'s own doc comment) must still be addressable, so an identical
    /// later call supersedes it instead of replaying the same capped-but-still-large result at
    /// every subsequent step for the rest of the run.
    #[test]
    fn shell_run_command_field_is_addressable_like_argv() {
        let a = addressable_for(
            "shell.run",
            &serde_json::json!({"command": "go test ./..."}),
        )
        .expect("a command-form call must resolve to an addressable key, not None");
        let b = addressable_for(
            "shell.run",
            &serde_json::json!({"command": "go test ./..."}),
        )
        .unwrap();
        assert_eq!(a.key, b.key, "identical command strings must collide");

        let c =
            addressable_for("shell.run", &serde_json::json!({"command": "go vet ./..."})).unwrap();
        assert_ne!(a.key, c.key, "different command strings must not collide");
    }

    /// When a call carries both fields, `tools.rs::command_argv` resolves `command` first and
    /// never looks at `argv` at all -- so the call actually *executes* as `/bin/sh -c <command>`,
    /// ignoring `argv` entirely. Dedup must follow execution here: keying on `argv` instead would
    /// let two calls that run completely different commands (same `argv`, different `command`)
    /// collide onto the same pruning key, which is exactly the cross-command collision the task's
    /// adversarial self-review asks to rule out. (The task description's acceptance text assumed
    /// `argv` wins in this case; the real `tools.rs` dispatcher resolves `command` first, so
    /// dedup mirrors that instead.)
    #[test]
    fn shell_run_command_takes_priority_over_argv_matching_execution() {
        let argv_only = addressable_for(
            "shell.run",
            &serde_json::json!({"argv": ["cargo", "test"], "cwd": "."}),
        )
        .unwrap();
        // `command` takes priority over `argv` per `tools.rs::command_argv`, so a call carrying
        // both fields is keyed on `command`, exactly matching how the tool itself would execute
        // it -- no drift between what runs and what gets deduplicated.
        let both_fields = addressable_for(
            "shell.run",
            &serde_json::json!({"command": "cargo build", "argv": ["cargo", "test"], "cwd": "."}),
        )
        .unwrap();
        assert_ne!(
            argv_only.key, both_fields.key,
            "a call with a different command must not collide with a plain argv call"
        );

        let command_form = addressable_for(
            "shell.run",
            &serde_json::json!({"command": "cargo build", "cwd": "."}),
        )
        .unwrap();
        assert_eq!(
            both_fields.key, command_form.key,
            "when both fields are present, the key must match the command-only form, since \
             tools.rs::command_argv resolves both the same way"
        );
    }

    /// The one argv-form input whose key *does* change versus the pre-fix inline formula:
    /// `tools.rs::command_argv` shell-wraps a single-element `argv` whose only element contains
    /// whitespace (the most common way a model gets `argv` wrong, per that function's own doc
    /// comment), so it executes -- and must now key -- identically to the equivalent `command`
    /// string, not as a literal one-argument argv.
    #[test]
    fn shell_run_single_element_whitespace_argv_keys_like_the_equivalent_command() {
        let one_element_argv =
            addressable_for("shell.run", &serde_json::json!({"argv": ["cargo test"]})).unwrap();
        let command_form =
            addressable_for("shell.run", &serde_json::json!({"command": "cargo test"})).unwrap();
        assert_eq!(
            one_element_argv.key, command_form.key,
            "command_argv shell-wraps a whole-line single-element argv, so both execute \
             identically and must dedup together"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Determinism property test (`docs/audit-2026-09-18-fable.md` B-08 item 4): `working_set`
    // and `rebuild_messages` are pure functions of `steps` alone, checked across randomized
    // transcripts rather than one hand-picked case, mirroring
    // `crates/tm-scheduler/src/plan.rs`'s `determinism_props` module.
    // -----------------------------------------------------------------------------------------
    mod determinism_props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// Same `steps` in, same `WorkingSet` verdicts and same rendered `Message`s out,
            /// every time -- across a variety of randomly generated tool-call sequences (reads,
            /// edits with mixed success, repeated searches, shell commands, and text-only
            /// steps), not just the fixed examples above.
            #[test]
            fn working_set_and_rebuild_messages_are_pure(
                ops in proptest::collection::vec(0u8..5, 1..12),
            ) {
                let paths = ["a.rs", "b.rs"];
                let queries = ["foo", "bar"];
                let argvs: [[&str; 2]; 2] = [["cargo", "test"], ["cargo", "build"]];

                let mut steps = Vec::new();
                for (i, op) in ops.iter().enumerate() {
                    let index = i as u32 + 1;
                    let built = match op {
                        0 => step(index, Some("looking"), vec![read(&format!("c{i}"), paths[i % 2])]),
                        1 => step(index, None, vec![write(&format!("c{i}"), paths[i % 2], i % 3 != 0)]),
                        2 => step(index, None, vec![search(&format!("c{i}"), queries[i % 2])]),
                        3 => step(
                            index,
                            None,
                            vec![ToolCallRecord {
                                tool_use_id: format!("c{i}"),
                                tool_name: "shell.run".to_string(),
                                input: serde_json::json!({"argv": argvs[i % 2]}),
                                resolution: ToolCallResolution::Completed {
                                    result: serde_json::json!({"exit_code": 0}),
                                    artifact: None,
                                },
                            }],
                        ),
                        _ => step(index, Some("just text"), vec![]),
                    };
                    steps.push(built);
                }

                let a = working_set(&steps);
                let b = working_set(&steps);
                let a_states: Vec<Vec<ToolCallState>> =
                    a.steps.iter().map(|s| s.tool_call_states.clone()).collect();
                let b_states: Vec<Vec<ToolCallState>> =
                    b.steps.iter().map(|s| s.tool_call_states.clone()).collect();
                prop_assert_eq!(a_states, b_states);
                prop_assert_eq!(a.bytes_pruned, b.bytes_pruned);
                prop_assert_eq!(a.tokens_pruned, b.tokens_pruned);

                let m1 = crate::agent_loop::rebuild_messages("task", &steps);
                let m2 = crate::agent_loop::rebuild_messages("task", &steps);
                prop_assert_eq!(m1, m2);
            }
        }
    }
}
