//! Read-only text renderers for slash commands that answer with a notice in the chat transcript,
//! kept separate from `chat_ops.rs`'s session/UI plumbing so the text is a pure function of its
//! inputs and easy to unit test without a live `AgentSession`. `/board`, `/milestones`,
//! `/timeline` and `/deps` (screen navigation) live directly on `App` in `tui.rs`/`chat_ops.rs`;
//! `/ticket`, `/run`, `/stats`, `/bench`, `/events`, `/replay` and `/genesis` (`show_ticket_cmd`/
//! `run_ticket_cmd`/`run_stats_cmd`/`run_bench_cmd`/`run_events_cmd`/`run_replay_cmd`/
//! `run_genesis_cmd` below) need `App`'s `project`/`chat`/`attached` fields for their one-shot
//! reads (or, for the latter three, to shell out to `tm`'s own CLI implementation of the same
//! verb — see `run_tm`'s own doc comment), so they're `impl crate::tui::App` methods here rather
//! than pure functions — placed in this file instead of `chat_ops.rs` to keep that file's diff to
//! one-line match arms (`chat_ops.rs` carries uncommitted edits in the untouched `odw-integrate`
//! worktree). `crate::tui::App`, not `super::super::App`, so the path still resolves once this
//! file is folded into a plain `mod slash_views;` in `tui.rs` (see this file's own `#[path]` note
//! in `chat_ops.rs`).

use tm_codeintel::hybrid::RankedHit;
use tm_context::SectionKind;
use tm_tui::chat::status::{format_tokens, PermissionMode};
use tm_tui::chat::transcript::NoticeLevel;
use tm_types::TicketId;

use crate::agent::{ContextReport, AUTO_COMPACT_TOKENS};
use crate::project::DoctorCheck;
use crate::render::{kind_label, state_label};

/// The most hits `/search` prints, matching `tm search`'s own default feel without letting one
/// broad query flood the transcript.
const MAX_SEARCH_HITS: usize = 10;

/// `/context`'s table: context-window size, then tokens by system prompt, instructions
/// (AGENTS.md), tools and conversation, computed from `report` and `total_tokens` (the last
/// turn's own `AgentSession::last_context_report`/`context_tokens`), rather than estimated fresh
/// here. `report` is `None` before any turn has run, in which case there is honestly nothing to
/// show yet. When `attached` is `Some`, a second block lists every section of the last-compiled
/// context pack (the ticket's prefetched material: outlines, symbols, history, search hits) with
/// its own token count, reusing `tm_context::Section::tokens` directly (`SPEC.md` §30.2's "an
/// unattributed context is a bug") — those sections sum to the block's own `Total` line by
/// construction (summed here, not read back off `ContextPack::tokens`). If `report`'s pack was
/// compiled for a different ticket than `attached` (or for none, a plain chat turn), that pack
/// would mislabel whichever ticket is attached now, so the block says to send a message instead
/// of printing stale numbers under the wrong ticket's name.
pub(super) fn context_table(
    report: Option<&ContextReport>,
    total_tokens: u64,
    attached: Option<&str>,
) -> String {
    let Some(report) = report else {
        return "Nothing measured yet in this session. Send a message, then run /context again."
            .to_string();
    };
    let instructions_tokens = report
        .context_pack
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Conventions)
        .map_or(0, |s| s.tokens as u64);
    let accounted = report.system_tokens + instructions_tokens + report.tools_tokens;
    // Everything the last call actually spent that isn't accounted for above: prior-turn
    // history, plus any prefetched section other than instructions (listed on its own below when
    // the pack matches the attached ticket). A remainder of a measured total, not a fresh guess.
    let conversation_tokens = total_tokens.saturating_sub(accounted);
    let free_tokens = AUTO_COMPACT_TOKENS.saturating_sub(total_tokens);

    let mut out = format!(
        "Context window: {} tokens (this session auto-compacts above it)",
        format_tokens(AUTO_COMPACT_TOKENS)
    );
    out.push_str(&row("System prompt", report.system_tokens));
    out.push_str(&row("Instructions (AGENTS.md)", instructions_tokens));
    out.push_str(&row("Tools", report.tools_tokens));
    out.push_str(&row("Conversation", conversation_tokens));
    out.push_str(&row("Free space", free_tokens));

    if let Some(attached) = attached {
        let pack_ticket = report.ticket.as_ref().map(TicketId::as_str);
        if pack_ticket != Some(attached) {
            out.push_str(&format!(
                "\n\nSend a message to see what's prefetched for {attached}."
            ));
        } else if report.context_pack.sections.is_empty() {
            out.push_str(&format!(
                "\n\nNothing was prefetched for {attached} on the last turn."
            ));
        } else {
            out.push_str(&format!("\n\nPrefetched for {attached}:"));
            let mut sum = 0u64;
            for section in &report.context_pack.sections {
                let tokens = section.tokens as u64;
                sum += tokens;
                out.push_str(&row(&section.title, tokens));
            }
            out.push_str(&row("Total", sum));
        }
    }

    out
}

/// One `label: value` line, indented and column-aligned the same way in both the main table and
/// the per-ticket section list below it.
fn row(label: &str, tokens: u64) -> String {
    format!("\n  {label:<26} {:>7}", format_tokens(tokens))
}

/// `/search <query>`'s rendering: the top hits from the same hybrid search `tm search --mode
/// hybrid` uses (`crates/tm-cli/src/search.rs`), one per line as `path:line  snippet`.
///
/// `project_indexed` distinguishes "nothing has ever been indexed for this project" from "the
/// index is current and this query just has no matches" — the caller (`chat_ops.rs::run_search`)
/// computes it from whether the index database existed before this call, or from this call's own
/// `update_incremental` having just added files, so a project that has never run `tm doctor` (or
/// any prior search) is told to build the index instead of being shown an empty list that looks
/// identical to a real no-match.
pub(super) fn search_results(hits: &[RankedHit], project_indexed: bool) -> String {
    if hits.is_empty() {
        return if project_indexed {
            "No matches for that search.".to_string()
        } else {
            "This project has no indexed code yet. Run `tm doctor` to see why.".to_string()
        };
    }
    hits.iter()
        .take(MAX_SEARCH_HITS)
        .map(|hit| {
            let location = match hit.line_start {
                Some(line) => format!("{}:{line}", hit.path),
                None => hit.path.clone(),
            };
            let snippet = hit.snippet.lines().next().unwrap_or("").trim();
            format!("{location}  {snippet}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/ticket <T>`'s rendering: a readable one-ticket summary, the transcript-friendly cousin of
/// `tm ticket show`'s own `format_ticket_text` (`crates/tm-cli/src/tickets.rs`) — fewer fields,
/// laid out to read inline rather than as a full detail block.
pub(super) fn ticket_summary(ticket: &tm_core::ticket::Ticket) -> String {
    let mut out = format!(
        "{} ({})\n  {} · {}",
        ticket.id,
        ticket.objective,
        state_label(ticket.state),
        kind_label(ticket.kind)
    );
    if let Some(milestone) = &ticket.milestone {
        out.push_str(&format!("\n  Milestone: {milestone}"));
    }
    if let Some(due) = ticket.due {
        out.push_str(&format!(
            "\n  Due: {}",
            tm_core::ticket::format_due_date(due)
        ));
    }
    if !ticket.dependencies.is_empty() {
        let deps = ticket
            .dependencies
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("\n  Depends on: {deps}"));
    }
    out
}

impl crate::tui::App {
    /// `/ticket <T>`: a readable summary of one ticket, printed inline in the transcript
    /// ([`ticket_summary`]) rather than switching screens.
    pub(super) fn show_ticket_cmd(&mut self, arg: &str) {
        let arg = arg.trim();
        let Ok(id) = TicketId::new(arg) else {
            return self.chat.push_notice(
                NoticeLevel::Warning,
                format!("\"{arg}\" doesn't look like a ticket id, like T-3."),
            );
        };
        let Ok(view) = self.project.store.view() else {
            return self
                .chat
                .push_notice(NoticeLevel::Warning, "Couldn't read the project.");
        };
        match view.tickets.get(&id) {
            Some(ticket) => self
                .chat
                .push_notice(NoticeLevel::Info, ticket_summary(ticket)),
            None => self
                .chat
                .push_notice(NoticeLevel::Warning, format!("No ticket {id}.")),
        }
    }

    /// `/run <T>`: activate a Draft ticket (`Store::activate`, `Draft -> Blocked`, continuing to
    /// `Ready` when its dependencies are already satisfied) so the in-process worker (or `tm
    /// sched run`) picks it up — the same transition `tm ticket activate` drives from the CLI.
    /// Any other starting state answers plainly instead of surfacing `activate`'s
    /// `InvalidTransition` error.
    pub(super) fn run_ticket_cmd(&mut self, arg: &str) {
        let arg = arg.trim();
        let Ok(id) = TicketId::new(arg) else {
            return self.chat.push_notice(
                NoticeLevel::Warning,
                format!("\"{arg}\" doesn't look like a ticket id, like T-3."),
            );
        };
        let Ok(view) = self.project.store.view() else {
            return self
                .chat
                .push_notice(NoticeLevel::Warning, "Couldn't read the project.");
        };
        let Some(ticket) = view.tickets.get(&id) else {
            return self
                .chat
                .push_notice(NoticeLevel::Warning, format!("No ticket {id}."));
        };
        use tm_core::ticket::TicketState;
        match ticket.state {
            TicketState::Draft => {}
            TicketState::Ready => {
                return self.chat.push_notice(
                    NoticeLevel::Info,
                    format!("{id} is already queued; watch it in /tickets."),
                );
            }
            TicketState::Blocked => {
                return self.chat.push_notice(
                    NoticeLevel::Info,
                    format!("{id} is already queued, waiting on its dependencies."),
                );
            }
            TicketState::Closed => {
                return self
                    .chat
                    .push_notice(NoticeLevel::Info, format!("{id} is already closed."));
            }
            TicketState::Escalated => {
                return self.chat.push_notice(
                    NoticeLevel::Info,
                    format!("{id} ran out of attempts. Run `tm ticket retry {id}` to try again."),
                );
            }
            other => {
                return self.chat.push_notice(
                    NoticeLevel::Info,
                    format!(
                        "{id} is {} right now, so there's nothing to queue.",
                        state_label(other)
                    ),
                );
            }
        }
        match self.project.store.activate(&id, self.project.actor.clone()) {
            Ok(_) => {
                let ready = self
                    .project
                    .store
                    .view()
                    .ok()
                    .and_then(|v| v.tickets.get(&id).map(|t| t.state))
                    == Some(TicketState::Ready);
                let msg = if !ready {
                    format!("{id} is waiting on its dependencies.")
                } else if self.local_worker {
                    format!("Queued {id}; watch it in /tickets.")
                } else {
                    format!(
                        "Queued {id}, but no worker is running here — run `tm sched run` to work it."
                    )
                };
                self.chat.push_notice(NoticeLevel::Success, msg);
            }
            Err(e) => self
                .chat
                .push_notice(NoticeLevel::Warning, format!("Couldn't queue {id}: {e}")),
        }
    }
}

/// The most `git diff HEAD` characters `/review` embeds directly in its prompt, past which the
/// agent reads the rest itself with its own shell tool rather than the turn opening with an
/// enormous diff — mirrors `agent.rs`'s `MAX_RESULT_CHARS` truncation of a tool call's own
/// result, just with a larger budget since this is the turn's actual subject, not one call's log.
const MAX_REVIEW_DIFF_CHARS: usize = 20_000;

/// The prompt `/review [focus]` sends: `diff` is `git diff HEAD`'s own output
/// (`chat_ops.rs::run_review` runs it, the way `/init` runs nothing and instead hands the agent a
/// task — here the diff is the point, so it's read once here rather than asked of the agent a
/// second time as a tool call).
pub(super) fn review_prompt(focus: &str, diff: &str) -> String {
    let focus = focus.trim();
    let scoped = if focus.is_empty() {
        String::new()
    } else {
        format!(" Focus on {focus}.")
    };
    let (diff, truncated) = match diff.char_indices().nth(MAX_REVIEW_DIFF_CHARS) {
        Some((cut, _)) => (&diff[..cut], true),
        None => (diff, false),
    };
    let note = if truncated {
        "\n\n[diff truncated — read the rest yourself with `git diff HEAD` if you need it]"
    } else {
        ""
    };
    format!(
        "Review these uncommitted changes against HEAD:{scoped}\n\n```diff\n{diff}\n```{note}\n\n\
         Point out real bugs, correctness risks, and anything unfinished or inconsistent with \
         the rest of the codebase — not style nitpicks. Check `git status` for untracked files \
         that might belong in this change if that seems relevant."
    )
}

/// `/doctor`'s table: one `check  status  detail` line per [`DoctorCheck`]
/// (`crate::project::doctor` — the same checks `tm doctor` runs). `status` is `ok`/`FAIL`/`warn`;
/// an optional check's failure (e.g. `computer-use`) is a warning, not a doctor failure, matching
/// `DoctorCheck::required`'s own convention.
pub(super) fn doctor_report(checks: &[DoctorCheck]) -> String {
    checks
        .iter()
        .map(|c| {
            let status = match (c.ok, c.required) {
                (true, _) => "ok",
                (false, true) => "FAIL",
                (false, false) => "warn",
            };
            format!("{:<14} {status:<4} {}", c.name, c.detail)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/permissions` with no argument: the current auto/plan/ask mode, marked, with what each
/// allows — the same three modes Shift+Tab cycles (`tm_tui::chat::status::PermissionMode`).
pub(super) fn permissions_table(current: PermissionMode) -> String {
    const ROWS: [(PermissionMode, &str); 3] = [
        (
            PermissionMode::Auto,
            "acts on its own: edits, commands and git run without asking",
        ),
        (
            PermissionMode::Plan,
            "read-only: investigates and proposes a plan, changes nothing",
        ),
        (
            PermissionMode::Ask,
            "asks before every edit, command, git operation or pty keystroke",
        ),
    ];
    ROWS.iter()
        .map(|(mode, what)| {
            let marker = if *mode == current { "* " } else { "  " };
            format!("{marker}{:<7} {what}", mode.label())
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n\n/permissions <auto|plan|ask> to set one, same as Shift+Tab."
}

/// `/permissions <mode>`: parse the argument against the same three names the table above shows.
pub(super) fn parse_permission_mode(arg: &str) -> Option<PermissionMode> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(PermissionMode::Auto),
        "plan" => Some(PermissionMode::Plan),
        "ask" => Some(PermissionMode::Ask),
        _ => None,
    }
}

/// `/workflow` with no name: every workflow discovered under this project's workflows directory
/// (`Project::state_dir`/`workflows`, not necessarily `<root>/.tm/workflows` — a global-scope
/// project's `state_dir` lives elsewhere, D-003), one line each — text rendering of the same
/// summary `tm workflow list` prints as a table. `results` pairs each discovered name with its
/// parsed definition or the parse error for it; one malformed `.toml` does not hide the rest, the
/// same convention `tm workflow list`'s own row-per-file handling follows.
pub(super) fn workflow_list(
    workflows_dir: &std::path::Path,
    results: &[(String, Result<tm_workflow::WorkflowDef, String>)],
) -> String {
    if results.is_empty() {
        return format!(
            "No workflows defined yet. Add one under {}/<name>.toml.",
            workflows_dir.display()
        );
    }
    results
        .iter()
        .map(|(name, result)| match result {
            Ok(def) => format!(
                "{name}  {} node(s), {} param(s){}",
                def.nodes.len(),
                def.params.len(),
                if def.is_one_by_one() { " (1x1)" } else { "" }
            ),
            Err(e) => format!("{name}  ERROR: {e}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/workflow <name>`: what starting it did — every ticket the expansion created, and whether
/// each was queued (`Store::activate`) for the in-process scheduler to work while the TUI is
/// open, the same way `/bg` queues a single ticket.
pub(super) fn workflow_started(
    name: &str,
    outcome: &tm_workflow::CommitOutcome,
    started: &[TicketId],
    failed: &[(TicketId, String)],
) -> String {
    let mut out = format!(
        "Started workflow \"{name}\" (version {}): {} ticket(s) created.",
        outcome.version,
        outcome.tickets.len()
    );
    let mut refs: Vec<_> = outcome.tickets.iter().collect();
    refs.sort_by(|a, b| a.0.cmp(b.0));
    for (ticket_ref, id) in refs {
        let note = if started.contains(id) {
            "queued".to_string()
        } else {
            failed
                .iter()
                .find(|(f, _)| f == id)
                .map(|(_, e)| format!("not queued: {e}"))
                .unwrap_or_else(|| "not queued".to_string())
        };
        out.push_str(&format!("\n  {ticket_ref} -> {id} ({note})"));
    }
    out.push_str("\n\nPress \u{2190} to watch them on the tickets screen.");
    out
}

/// `/export`'s markdown body: this session's saved turns, oldest first, each user message as a
/// quote and each step's reply/tool calls as plain text (`crate::tui::steps::tool_target` for the
/// same one-line call description the transcript itself shows). Written to disk by
/// `chat_ops.rs::export_transcript`, not here, so this stays a pure function of the conversation.
pub(super) fn export_markdown(session: &str, turns: &[tm_agent::ConversationTurn]) -> String {
    let mut out = format!("# tm session {session}\n");
    if turns.is_empty() {
        out.push_str("\n(no turns yet)\n");
        return out;
    }
    for turn in turns {
        out.push_str("\n## You\n\n> ");
        out.push_str(&turn.user_message.replace('\n', "\n> "));
        out.push('\n');
        for step in &turn.steps {
            if let Some(text) = &step.assistant_text {
                if !text.trim().is_empty() {
                    out.push_str("\n### tm\n\n");
                    out.push_str(text);
                    out.push('\n');
                }
            }
            for call in &step.tool_calls {
                out.push_str(&format!(
                    "\n- `{}`\n",
                    crate::tui::steps::tool_target(&call.tool_name, &call.input)
                ));
            }
        }
    }
    out
}

/// The most events `/events` tails, matching the feel of a quick glance rather than a full `tm
/// events tail` session.
const MAX_EVENTS_TAILED: usize = 20;

/// Read this project's whole event log, oldest first -- an independent read of the same log
/// `stats.rs`'s own private `read_all_events` and `project.rs`'s `open_event_log` each already
/// duplicate, per `stats.rs`'s own doc comment on why each command module reads through its own
/// thin handle rather than a shared one.
fn read_project_events(
    project: &crate::project::Project,
) -> tm_types::Result<Vec<tm_events::Event>> {
    let db_path = project.state_dir.join("project.db");
    let log = tm_events::EventLog::open_with_clock(&db_path, project.clock.clone())?;
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

/// `$` for an unpriced (zero-micros) call, matching `stats.rs`'s own `format_dollars` -- kept as
/// a separate copy rather than made `pub` there, the same "each command module renders its own
/// text" convention `stats.rs`'s doc comment on `read_all_events` already documents for the IO
/// side.
fn format_dollars(micros: u64) -> String {
    if micros == 0 {
        "not priced".to_string()
    } else {
        format!("${:.6}", micros as f64 / 1_000_000.0)
    }
}

/// `/stats`'s table: one row per ticket that has recorded usage, matching `tm stats`'s own
/// default (`--by ticket`) rollup.
pub(super) fn stats_table(rows: &[tm_harness::metrics::TicketMetrics]) -> String {
    if rows.is_empty() {
        return "No usage recorded yet. Run `tm run <ticket>` to record some.".to_string();
    }
    let mut out = "Ticket   Tool calls  Tokens  Cost         Wall time (s)".to_string();
    for m in rows {
        out.push_str(&format!(
            "\n{:<8} {:<11} {:<7} {:<12} {}",
            m.ticket,
            m.tool_calls,
            m.tokens_in,
            format_dollars(m.dollars_micros),
            m.wall_seconds
        ));
    }
    out
}

/// `/events`'s listing: `seq  kind  subject`, the last [`MAX_EVENTS_TAILED`] events, oldest of
/// that window first -- matching `tm events tail`'s own one-line-per-event human format
/// (`ops.rs`'s private `event_tail_human`).
pub(super) fn events_tail(events: &[tm_events::Event]) -> String {
    if events.is_empty() {
        return "No events recorded yet.".to_string();
    }
    let start = events.len().saturating_sub(MAX_EVENTS_TAILED);
    events[start..]
        .iter()
        .map(|e| format!("{}  {}  {}", e.seq, e.kind, e.subject))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Runs this same `tm` binary as a subprocess against `project.root`, with `args`, and returns
/// its captured stdout (trimmed) on success or a plain description of what went wrong on
/// failure. `/bench`, `/replay` and `/genesis` each already have a full CLI implementation (`tm
/// bench`, `tm run --replay`, `tm genesis`); spawning the compiled binary reuses that directly
/// instead of a second copy of `discover_bench_tasks`/`bench_run`/`run_genesis_stages`'s logic
/// here. `current_dir` (not `--project`) resolves the project the same way `chat_ops.rs::
/// run_review`'s own `git diff` subprocess already does, so this also works for a global-scope
/// project (`--project` always means repo scope, D-003). Callers pass an option's value as
/// `--flag=value` rather than two separate args, so a value starting with `-` (a path like
/// `-replay.json`, an unlucky prompt) is never misread as a flag of its own.
fn run_tm(project: &crate::project::Project, args: &[&str]) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Could not find the tm binary: {e}"))?;
    let output = std::process::Command::new(exe)
        .args(args)
        .current_dir(&project.root)
        .output()
        .map_err(|e| format!("Could not run tm: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        // `Renderer::error`'s human mode already prefixes with "error: "; strip it so this
        // doesn't stack a second "Could not X: error: ..." prefix on top (voice rules).
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stderr = stderr
            .strip_prefix("error: ")
            .unwrap_or(&stderr)
            .to_string();
        Err(if stderr.is_empty() {
            match output.status.code() {
                Some(code) => format!("tm didn't finish (exit code {code})"),
                None => "tm didn't finish".to_string(),
            }
        } else {
            stderr
        })
    }
}

impl crate::tui::App {
    /// `/stats`: the same per-ticket rollup `tm stats` prints by default, read straight from the
    /// event log rather than shelling out (unlike `/bench`/`/replay`/`/genesis` below) since the
    /// aggregation itself ([`crate::stats::stats_by_ticket`]) is a pure, already-public function.
    pub(super) fn run_stats_cmd(&mut self) {
        match read_project_events(&self.project) {
            Ok(events) => {
                let rows = crate::stats::stats_by_ticket(&events, None);
                self.chat.push_notice(NoticeLevel::Info, stats_table(&rows));
            }
            Err(e) => self
                .chat
                .push_notice(NoticeLevel::Warning, format!("Could not read usage: {e}")),
        }
    }

    /// `/events`: the last [`MAX_EVENTS_TAILED`] events, the same read [`Self::run_stats_cmd`]
    /// does.
    pub(super) fn run_events_cmd(&mut self) {
        match read_project_events(&self.project) {
            Ok(events) => self
                .chat
                .push_notice(NoticeLevel::Info, events_tail(&events)),
            Err(e) => self
                .chat
                .push_notice(NoticeLevel::Warning, format!("Could not read events: {e}")),
        }
    }

    /// `/bench [task]`: bare, `tm bench list`'s table; with a task name, `tm bench run --filter
    /// <task>`'s summary.
    pub(super) fn run_bench_cmd(&mut self, arg: &str) {
        let task = arg.trim();
        let result = if task.is_empty() {
            run_tm(&self.project, &["bench", "list"])
        } else {
            let filter = format!("--filter={task}");
            run_tm(&self.project, &["bench", "run", filter.as_str()])
        };
        match result {
            Ok(output) => self.chat.push_notice(
                NoticeLevel::Info,
                if output.is_empty() {
                    "No benchmark tasks found under bench/tasks/.".to_string()
                } else {
                    output
                },
            ),
            Err(e) => self
                .chat
                .push_notice(NoticeLevel::Warning, format!("Could not run tm bench: {e}")),
        }
    }

    /// `/replay <path>`: rerun the attached ticket offline against a saved cassette (`tm run <T>
    /// --replay <path>`), same as `replay-cli-replay-flag`'s CLI flag. Needs an attached ticket
    /// (`/attach <ticket>` first), since a cassette replays *a* ticket's calls, not a bare chat
    /// turn.
    pub(super) fn run_replay_cmd(&mut self, arg: &str) {
        let path = arg.trim();
        let Some(ticket) = self.attached.clone() else {
            return self.chat.push_notice(
                NoticeLevel::Warning,
                "No ticket attached. Run /attach <ticket>, then /replay again.",
            );
        };
        // The subprocess's cwd is `project.root` (see `run_tm`), not wherever the user actually
        // typed `/replay` from, so a relative path has to be resolved against *this* process's
        // cwd before crossing that boundary, or it would (silently, and wrongly) resolve against
        // the project root instead.
        let path = std::path::Path::new(path);
        let resolved = if path.is_relative() {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        } else {
            path.to_path_buf()
        };
        let replay = format!("--replay={}", resolved.display());
        match run_tm(&self.project, &["run", ticket.as_str(), replay.as_str()]) {
            Ok(output) => self.chat.push_notice(
                NoticeLevel::Info,
                if output.is_empty() {
                    format!("Replayed {ticket} from {}.", resolved.display())
                } else {
                    output
                },
            ),
            Err(e) => self.chat.push_notice(
                NoticeLevel::Warning,
                format!("Could not replay {ticket}: {e}"),
            ),
        }
    }

    /// `/genesis <prompt>`: start `tm genesis --prompt <prompt>` as a background process rather
    /// than blocking the chat -- genesis is a multi-stage run that can take minutes and, per
    /// D-027, stops partway for `tm sched run`/`tm genesis --resume` rather than finishing in one
    /// shot, so waiting on it here the way `/replay` waits on a (fast, offline) replay would just
    /// freeze the chat for no benefit.
    pub(super) fn run_genesis_cmd(&mut self, arg: &str) {
        let prompt = arg.trim();
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                return self.chat.push_notice(
                    NoticeLevel::Warning,
                    format!("Could not find the tm binary: {e}"),
                )
            }
        };
        let prompt_flag = format!("--prompt={prompt}");
        match std::process::Command::new(exe)
            .args(["genesis", prompt_flag.as_str()])
            .current_dir(&self.project.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                // Reap it on a detached thread rather than leaving a zombie process for the rest
                // of this TUI session -- nothing here needs its exit status, only that something
                // eventually calls `wait` on it.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                self.chat.push_notice(
                    NoticeLevel::Success,
                    format!(
                        "Started genesis for \"{prompt}\" in the background. It may stop early \
                         for you to run `tm sched run`, then continue with `tm genesis \
                         --resume`; watch new tickets appear on the tickets screen."
                    ),
                );
            }
            Err(e) => self.chat.push_notice(
                NoticeLevel::Warning,
                format!("Could not start genesis: {e}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_context::{ContextPack, Section};

    fn section(kind: SectionKind, title: &str, tokens: usize) -> Section {
        Section {
            kind,
            title: title.to_string(),
            body: String::new(),
            tokens,
            bytes: tokens * 4,
            provenance: Vec::new(),
        }
    }

    fn pack(sections: Vec<Section>) -> ContextPack {
        let tokens = sections.iter().map(|s| s.tokens).sum();
        let bytes = sections.iter().map(|s| s.bytes).sum();
        ContextPack {
            sections,
            tokens,
            bytes,
            provenance: Vec::new(),
            dropped: Vec::new(),
        }
    }

    fn ticket(id: &str) -> TicketId {
        TicketId::new(id).expect("valid ticket id")
    }

    #[test]
    fn no_report_says_so_instead_of_inventing_numbers() {
        let text = context_table(None, 0, None);
        assert!(text.contains("Nothing measured yet"));
    }

    #[test]
    fn table_breaks_down_the_measured_tokens() {
        let report = ContextReport {
            system_tokens: 500,
            tools_tokens: 300,
            ticket: None,
            context_pack: pack(vec![section(SectionKind::Conventions, "Conventions", 100)]),
        };
        let text = context_table(Some(&report), 2_000, None);
        assert!(text.contains(&format!(
            "Context window: {}",
            format_tokens(AUTO_COMPACT_TOKENS)
        )));
        assert!(text.contains(&row("System prompt", 500)));
        assert!(text.contains(&row("Instructions (AGENTS.md)", 100)));
        assert!(text.contains(&row("Tools", 300)));
        // 2000 - (500 + 100 + 300) = 1100 left over as conversation.
        assert!(text.contains(&row("Conversation", 1_100)));
        assert!(!text.contains("Prefetched for"));
    }

    #[test]
    fn attached_ticket_lists_sections_summing_to_the_pack_total() {
        let report = ContextReport {
            system_tokens: 500,
            tools_tokens: 300,
            ticket: Some(ticket("T-4")),
            context_pack: pack(vec![
                section(SectionKind::Objective, "Objective", 50),
                section(SectionKind::Retrieval, "Retrieval", 150),
                section(SectionKind::Conventions, "Conventions", 100),
            ]),
        };
        let text = context_table(Some(&report), 2_000, Some("T-4"));
        assert!(text.contains("Prefetched for T-4:"));
        assert!(text.contains(&row("Objective", 50)));
        assert!(text.contains(&row("Retrieval", 150)));
        // The three sections sum to 300, and the Total row is that same sum, not a separately
        // trusted `ContextPack::tokens` that could in principle disagree with it.
        assert!(text.contains(&row("Total", 300)));
    }

    #[test]
    fn empty_pack_says_nothing_was_prefetched() {
        let report = ContextReport {
            system_tokens: 500,
            tools_tokens: 300,
            ticket: Some(ticket("T-1")),
            context_pack: pack(vec![]),
        };
        let text = context_table(Some(&report), 800, Some("T-1"));
        assert!(text.contains("Nothing was prefetched for T-1"));
    }

    #[test]
    fn stale_pack_from_a_different_ticket_is_not_shown_as_the_attached_ticket_s() {
        // The last turn ran against T-4 (or no ticket at all); the conversation has since
        // attached to T-9. Printing T-4's sections under "Prefetched for T-9" would be wrong.
        let report = ContextReport {
            system_tokens: 500,
            tools_tokens: 300,
            ticket: Some(ticket("T-4")),
            context_pack: pack(vec![section(SectionKind::Objective, "Objective", 50)]),
        };
        let text = context_table(Some(&report), 800, Some("T-9"));
        assert!(text.contains("Send a message to see what's prefetched for T-9"));
        assert!(!text.contains("Prefetched for T-9"));
        assert!(!text.contains("Objective"));
    }

    fn hit(path: &str, line: u32, snippet: &str) -> RankedHit {
        RankedHit {
            path: path.to_string(),
            line_start: Some(line),
            line_end: Some(line),
            snippet: snippet.to_string(),
            fused_score: 1.0,
            explain: Vec::new(),
        }
    }

    #[test]
    fn search_results_lists_path_line_and_snippet() {
        let hits = vec![
            hit("src/graph.rs", 42, "fn build_dependency_graph() {"),
            hit("src/graph.rs", 60, "    graph.add_edge(a, b);"),
        ];
        let text = search_results(&hits, true);
        assert_eq!(
            text,
            "src/graph.rs:42  fn build_dependency_graph() {\n\
             src/graph.rs:60  graph.add_edge(a, b);"
        );
    }

    #[test]
    fn search_results_caps_at_ten_hits() {
        let hits: Vec<RankedHit> = (0..15).map(|i| hit("f.rs", i, "line")).collect();
        assert_eq!(search_results(&hits, true).lines().count(), MAX_SEARCH_HITS);
    }

    #[test]
    fn search_results_distinguishes_no_matches_from_never_indexed() {
        assert_eq!(search_results(&[], true), "No matches for that search.");
        assert!(search_results(&[], false).contains("no indexed code yet"));
    }

    #[test]
    fn review_prompt_embeds_the_diff_and_an_optional_focus() {
        let plain = review_prompt("", "diff --git a/x b/x\n+added line\n");
        assert!(plain.contains("diff --git a/x b/x"));
        assert!(plain.contains("+added line"));
        assert!(!plain.contains("Focus on"));
        let focused = review_prompt("error handling", "diff --git a/x b/x\n");
        assert!(focused.contains("Focus on error handling."));
    }

    #[test]
    fn review_prompt_truncates_an_oversized_diff() {
        let huge = "x".repeat(MAX_REVIEW_DIFF_CHARS + 500);
        let text = review_prompt("", &huge);
        assert!(text.contains("[diff truncated"));
        assert!(
            text.len() < huge.len() + 500,
            "diff body itself was cut down"
        );
    }

    fn check(name: &str, ok: bool, required: bool, detail: &str) -> DoctorCheck {
        DoctorCheck {
            name: name.to_string(),
            ok,
            required,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn doctor_report_marks_ok_fail_and_warn() {
        let text = doctor_report(&[
            check("scope", true, true, "repo scope"),
            check("hash-chain", false, true, "chain broken"),
            check("computer-use", false, false, "no permission"),
        ]);
        assert!(text.contains("scope") && text.contains("ok"));
        assert!(text.contains("hash-chain") && text.contains("FAIL"));
        assert!(text.contains("computer-use") && text.contains("warn"));
    }

    #[test]
    fn permissions_table_marks_the_current_mode_and_lists_the_others() {
        let text = permissions_table(PermissionMode::Plan);
        assert!(text.contains("* plan"));
        assert!(text.contains("  auto"));
        assert!(text.contains("  ask"));
        assert!(text.contains("/permissions <auto|plan|ask>"));
    }

    #[test]
    fn parse_permission_mode_reads_all_three_names_case_insensitively() {
        assert_eq!(parse_permission_mode("AUTO"), Some(PermissionMode::Auto));
        assert_eq!(parse_permission_mode(" plan "), Some(PermissionMode::Plan));
        assert_eq!(parse_permission_mode("ask"), Some(PermissionMode::Ask));
        assert_eq!(parse_permission_mode("nope"), None);
    }

    fn metrics(
        ticket: &str,
        tool_calls: u32,
        tokens_in: u64,
        dollars_micros: u64,
    ) -> tm_harness::metrics::TicketMetrics {
        tm_harness::metrics::TicketMetrics {
            ticket: TicketId::new(ticket).expect("valid ticket id"),
            session: tm_types::SessionId::new("S-1").expect("valid session id"),
            harness_epoch: 0,
            wall_seconds: 5,
            tokens_in,
            tokens_out: 0,
            dollars_micros,
            tool_calls,
            searches_before_first_relevant_hit: 0,
            verification_failures: 0,
            retries: 0,
            context_bytes: 0,
            commands_rerun: 0,
            human_interventions: 0,
            recorded_at: tm_types::Timestamp::EPOCH,
        }
    }

    #[test]
    fn stats_table_shows_the_empty_state_or_one_row_per_ticket() {
        assert!(stats_table(&[]).contains("No usage recorded yet"));
        let rows = vec![metrics("T-1", 3, 100, 1_500_000), metrics("T-2", 0, 0, 0)];
        let text = stats_table(&rows);
        assert!(text.contains("T-1"));
        assert!(text.contains("$1.500000"));
        assert!(text.contains("T-2"));
        assert!(text.contains("not priced"));
    }

    fn test_event(seq: u64, subject: tm_types::Id) -> tm_events::Event {
        let draft = tm_events::EventDraft::new(
            tm_types::ParticipantId::new("human:test").expect("valid participant id"),
            subject,
            tm_events::Payload::from(tm_events::payload::UsageRecordedPayload {
                ticket: None,
                session: None,
                tokens: 0,
                dollars_micros: 0,
                wall_seconds: 0,
                provider: None,
                model: None,
            }),
        );
        tm_events::Event {
            seq,
            ts: tm_types::Timestamp::EPOCH,
            kind: draft.kind(),
            subject: draft.subject,
            actor: draft.actor,
            session: draft.session,
            causation: draft.causation,
            correlation: draft.correlation,
            payload: draft.payload,
            hash: "test".to_string(),
        }
    }

    #[test]
    fn events_tail_shows_the_empty_state_or_the_most_recent_window() {
        assert!(events_tail(&[]).contains("No events"));
        let events: Vec<_> = (1..=(MAX_EVENTS_TAILED as u64 + 5))
            .map(|seq| test_event(seq, tm_types::Id::none()))
            .collect();
        let text = events_tail(&events);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), MAX_EVENTS_TAILED);
        assert!(lines[0].starts_with('6'));
        assert!(lines
            .last()
            .unwrap()
            .starts_with(&(MAX_EVENTS_TAILED + 5).to_string()));
    }

    fn workflow_def(name: &str) -> tm_workflow::WorkflowDef {
        let source = format!(
            r#"
name = "{name}"

[[node]]
id = "a"
role = "coder_fast"
objective = "a"
budget = {{ tokens = 1 }}
verification = "none"
"#
        );
        tm_workflow::WorkflowDef::parse(&source).expect("valid workflow definition")
    }

    #[test]
    fn workflow_list_empty_says_so() {
        let dir = std::path::Path::new("/tmp/project/.tm/workflows");
        let text = workflow_list(dir, &[]);
        assert!(text.contains("No workflows defined yet"));
        assert!(text.contains("/tmp/project/.tm/workflows"));
    }

    #[test]
    fn workflow_list_shows_node_and_param_counts_and_parse_errors() {
        let dir = std::path::Path::new("/tmp/project/.tm/workflows");
        let text = workflow_list(
            dir,
            &[
                ("release".to_string(), Ok(workflow_def("release"))),
                ("broken".to_string(), Err("missing `name`".to_string())),
            ],
        );
        assert!(text.contains("release  1 node(s), 0 param(s)"));
        assert!(text.contains("broken  ERROR: missing `name`"));
    }

    #[test]
    fn workflow_started_marks_queued_and_failed_tickets() {
        let mut tickets = std::collections::BTreeMap::new();
        tickets.insert("a".to_string(), ticket("T-1"));
        tickets.insert("b".to_string(), ticket("T-2"));
        let outcome = tm_workflow::CommitOutcome {
            tickets,
            content_hash: "abc123".to_string(),
            version: 1,
        };
        let text = workflow_started(
            "release",
            &outcome,
            &[ticket("T-1")],
            &[(ticket("T-2"), "already leased".to_string())],
        );
        assert!(text.contains("Started workflow \"release\" (version 1): 2 ticket(s) created."));
        assert!(text.contains("a -> T-1 (queued)"));
        assert!(text.contains("b -> T-2 (not queued: already leased)"));
    }

    #[test]
    fn export_markdown_includes_user_messages_and_replies() {
        let turn = tm_agent::ConversationTurn {
            user_message: "fix the build".to_string(),
            steps: vec![tm_agent::outcome::StepRecord {
                index: 1,
                served_by: "anthropic/claude".to_string(),
                assistant_text: Some("Fixed it.".to_string()),
                tool_calls: Vec::new(),
                spend: tm_types::Spend::default(),
                at: tm_types::Timestamp::EPOCH,
            }],
        };
        let text = export_markdown("S-1", &[turn]);
        assert!(text.contains("# tm session S-1"));
        assert!(text.contains("> fix the build"));
        assert!(text.contains("Fixed it."));
    }

    #[test]
    fn export_markdown_says_so_with_no_turns() {
        assert!(export_markdown("S-1", &[]).contains("no turns yet"));
    }

    fn sample_ticket(id: &str, objective: &str) -> tm_core::ticket::Ticket {
        tm_core::ticket::Ticket {
            id: TicketId::new(id).unwrap(),
            objective: objective.to_string(),
            kind: tm_core::ticket::TicketKind::Work,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            state: tm_core::ticket::TicketState::Draft,
            priority: 0,
            authority: tm_types::Authority::default(),
            resources: vec![],
            executor: crate::tickets::default_executor_requirements(),
            context_refs: vec![],
            success: vec![],
            verification: tm_core::ticket::VerificationPolicy::Single,
            budget: tm_types::Budget::unlimited(),
            retry: crate::tickets::default_retry_policy(),
            attempts: 0,
            failures: vec![],
            created: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            updated: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            cycle: None,
        }
    }

    #[test]
    fn ticket_summary_reads_plainly_and_has_no_debug_syntax() {
        let mut t = sample_ticket("T-1", "Fix the login redirect");
        t.milestone = Some(tm_types::MilestoneId::new("M-1").unwrap());
        t.due = Some(time::Date::from_calendar_date(2026, time::Month::October, 1).unwrap());
        t.dependencies = vec![ticket("T-2")];
        let text = ticket_summary(&t);
        assert!(text.contains("T-1 (Fix the login redirect)"));
        assert!(text.contains("draft"));
        assert!(text.contains("Milestone: M-1"));
        assert!(text.contains("Due: 2026-10-01"));
        assert!(text.contains("Depends on: T-2"));
        assert!(!text.contains('{'), "text contained a struct brace: {text}");
        assert!(!text.contains("Some("), "text contained Some(...): {text}");
    }

    #[test]
    fn ticket_summary_omits_optional_lines_when_unset() {
        let text = ticket_summary(&sample_ticket("T-9", "No milestone yet"));
        assert!(!text.contains("Milestone:"));
        assert!(!text.contains("Due:"));
        assert!(!text.contains("Depends on:"));
    }
}
