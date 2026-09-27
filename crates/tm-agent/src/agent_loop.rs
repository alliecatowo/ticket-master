//! [`AgentLoop::run`]: the step loop over the provider fabric, tool dispatch, denial handling,
//! approval suspension/resume, budget enforcement between steps, step limits, and clean
//! termination into an [`AgentOutcome`].
//!
//! The loop is deterministic under `tm_provider::MockProvider`: every provider call goes
//! through the injected [`tm_provider::fabric::Fabric`], every tool call through
//! [`crate::tools::ToolRegistry::dispatch`], and every timestamp/id through the injected
//! [`tm_types::Clock`]/[`tm_types::IdSource`], so replaying the same scripted provider against
//! the same task produces byte-identical [`AgentOutcome`]s.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tm_core::{FailureClass, Store};
use tm_events::payload::{
    ApprovalDecidedPayload, ApprovalRequestedPayload, ProviderDegradedPayload,
    ProviderExhaustedPayload, ProviderRecoveredPayload, ProviderSelectedPayload,
    SessionEndedPayload, SessionStartedPayload, ToolCallCompletedPayload,
};
use tm_events::{EventDraft, Payload};
use tm_harness::config::PromptFragments;
use tm_provider::fabric::{Fabric, FabricRecord};
use tm_provider::{CompletionRequest, ContentBlock, Message, MessageRole};
use tm_types::{
    Authority, Budget, CallContext, Clock, Decision, Id, IdSource, Oversight, ParticipantId,
    Result, Role, Spend, TicketId, TmError,
};

use crate::outcome::{
    AgentOutcome, AgentTask, BudgetDimension, Conversation, PendingApproval, StepRecord,
    ToolCallRecord,
};
use crate::pruning;
use crate::session::Session;
use crate::tools::{ToolCall, ToolOutcome, ToolRegistry};

/// The `ticket.submit` wire name (`SPEC.md` §11), checked here by literal string rather than
/// `crate::tools::ToolName` — that enum is a private implementation detail of
/// [`crate::tools::BuiltinCapability`] now (`docs/audit-2026-09-18-fable.md` A-01), and this
/// loop dispatches by wire name alone so it works identically for a future capability whose
/// tools were never `ToolName` variants at all.
const TICKET_SUBMIT: &str = "ticket.submit";

/// Upper bound on steps a single [`AgentLoop::run`] call will take before it forces a
/// [`AgentOutcome::Failed`] with `tm_core::FailureClass::Other`, guarding against a
/// non-terminating tool-call loop even when budget alone hasn't tripped yet.
pub const DEFAULT_MAX_STEPS: u32 = 64;

/// How many times [`AgentLoop::drive`] nudges a ticketed run that ended a turn with plain text
/// and no `ticket.submit` call, instead of failing it immediately (`u1-worker-submit-nudge`):
/// Claude Code/Codex-style harnesses give the model one more chance to either submit or keep
/// working rather than losing the run's progress to a turn that just stopped talking. See
/// [`AgentLoop::with_max_submit_nudges`] to change it per loop.
pub const DEFAULT_MAX_SUBMIT_NUDGES: u32 = 1;

/// Consecutive ticketed steps without a successful repository or evidence write before stopping.
const NO_PROGRESS_STEP_LIMIT: usize = 10;

/// Repeated inspections of one target with no productive action are stopped before the broader
/// no-progress step ceiling, which exists for varied-but-unproductive exploration.
const REPEATED_EXPLORATION_LIMIT: usize = 6;

/// The user message appended after a ticketed run's text-only turn, once per nudge budgeted by
/// [`AgentLoop::max_submit_nudges`] (see [`DEFAULT_MAX_SUBMIT_NUDGES`]).
const SUBMIT_NUDGE_TEXT: &str = "You ended your turn without calling ticket.submit. If the work \
     is done and verified, call ticket.submit with evidence now; if not, continue working.";

/// The exact [`AgentOutcome::Failed::detail`] a ticketed run's exhausted-nudge, no-`ticket.submit`
/// text-only ending produces (`recover-without-repeating-agent-investigation`). Named so a caller
/// on the far side of this crate (`crate::executor::BuiltinExecutor`) can recognize this specific
/// failure by equality rather than duplicating the literal, the same way [`CAPACITY_REFUSAL_PREFIX`]
/// lets [`is_capacity_refusal`] recognize its own failure shape.
pub const NO_SUBMIT_DETAIL: &str = "model ended turn without submitting";

/// Longest rendering of an [`InvestigationSummary`] carried into a following attempt's context or
/// a failure detail (`recover-without-repeating-agent-investigation`) — concise by design, not a
/// full transcript dump; a caller that needs the whole thing still has the durable steps/events.
const INVESTIGATION_SUMMARY_MAX_CHARS: usize = 2000;

/// A concise, comparable digest of one no-submit attempt's tool activity
/// (`recover-without-repeating-agent-investigation`, `docs/tasks/TASKS.md`'s
/// `t20260925-1314-BurntSushi-ripgrep-3376-recover-without-repeating-agent-investigation`): what a
/// run already did before it ended without calling `ticket.submit`, so a following attempt's
/// context can include it instead of starting cold, and so two consecutive no-submit attempts that
/// repeat the same investigation can be told apart from one that made real, different progress.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InvestigationSummary {
    /// One normalized signature per distinct tool call this attempt made (`tool_name` plus its
    /// most identifying argument), deduplicated and sorted — see [`tool_call_signature`].
    pub tool_signatures: Vec<String>,
    /// The last non-empty assistant text this attempt produced (its own stated conclusion, if
    /// any), truncated to [`INVESTIGATION_SUMMARY_MAX_CHARS`].
    pub conclusion: Option<String>,
    /// Whether this attempt called any mutating tool (`edit.*`, `git.commit`) at all, regardless
    /// of whether the call itself succeeded — a call that erred on the safe side of "did this
    /// attempt try to act, not just investigate" rather than requiring proof the edit landed. An
    /// attempt with `made_changes: true` moved from investigating to acting, however that attempt
    /// itself ended; [`crate::executor::BuiltinExecutor`] uses this to exempt such an attempt from
    /// [`InvestigationSummary::mostly_repeats`]'s check entirely; re-reading files it already read
    /// before editing one of them is unavoidable (an edit needs a fresh `expected_hash`), and
    /// without this exemption that unavoidable re-reading alone could still flag real, converging
    /// work as "the same investigation repeated".
    pub made_changes: bool,
}

impl InvestigationSummary {
    /// Build a summary from one attempt's own transcript.
    pub fn from_steps(steps: &[StepRecord]) -> Self {
        let mut signatures: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut conclusion = None;
        let mut made_changes = false;
        for step in steps {
            for call in &step.tool_calls {
                signatures.insert(tool_call_signature(&call.tool_name, &call.input));
                // Whether the call *itself* succeeded is deliberately not checked here: a call
                // that erred (a stale hash, a denied write) still shows this attempt tried to
                // act, not merely investigate, which is exactly what this flag exists to record.
                if call.tool_name.starts_with("edit.") || call.tool_name == "git.commit" {
                    made_changes = true;
                }
            }
            if let Some(text) = &step.assistant_text {
                if !text.trim().is_empty() {
                    conclusion = Some(text.trim().to_string());
                }
            }
        }
        InvestigationSummary {
            tool_signatures: signatures.into_iter().collect(),
            conclusion: conclusion.map(|c| truncate_chars(&c, INVESTIGATION_SUMMARY_MAX_CHARS)),
            made_changes,
        }
    }

    /// True when `self` (a *later* attempt's own investigation) is mostly a repeat of `prior` (an
    /// earlier one's): more than half of `self`'s own tool signatures also appear in `prior`.
    /// Deliberately directional, not symmetric like a plain set-overlap check would be: an
    /// attempt that re-reads one file from the prior attempt but then goes on to edit it, run the
    /// tests, and do real new work must not be flagged just because that one re-read is *all* the
    /// prior attempt happened to have done — re-reading a file the model doesn't already have the
    /// current `hash`/content of is unavoidable before it can safely edit it. Comparing against
    /// the *later* attempt's own total, rather than the smaller of the two sets, means a large
    /// amount of genuinely new tool activity dilutes one incidental shared read below the
    /// threshold, while an attempt that (like `prior`) does nothing but that same one read still
    /// counts as a full repeat. Two summaries where either side has no tool signatures at all
    /// never count as a repeat (nothing concrete to compare), even if their conclusions happen to
    /// match.
    pub fn mostly_repeats(&self, prior: &InvestigationSummary) -> bool {
        if self.tool_signatures.is_empty() || prior.tool_signatures.is_empty() {
            return false;
        }
        let prior_set: std::collections::BTreeSet<&str> =
            prior.tool_signatures.iter().map(String::as_str).collect();
        let shared = self
            .tool_signatures
            .iter()
            .filter(|s| prior_set.contains(s.as_str()))
            .count();
        shared * 2 > self.tool_signatures.len()
    }

    /// Render this summary as concise prose for a following attempt's context or a failure
    /// detail: what was touched, and what (if anything) the model itself concluded.
    pub fn render(&self) -> String {
        if self.tool_signatures.is_empty() && self.conclusion.is_none() {
            return "no tool activity recorded".to_string();
        }
        let mut out = String::new();
        if !self.tool_signatures.is_empty() {
            out.push_str("already investigated: ");
            out.push_str(&self.tool_signatures.join("; "));
        }
        if let Some(c) = &self.conclusion {
            if !out.is_empty() {
                out.push_str(". ");
            }
            out.push_str("its own conclusion: \"");
            out.push_str(c);
            out.push('"');
        }
        truncate_chars(&out, INVESTIGATION_SUMMARY_MAX_CHARS)
    }
}

/// A stable, comparable signature for one tool call: `tool_name` plus its most identifying
/// argument, in this priority order — `path`, `query`, `pattern`, `command`, then `argv` (a
/// string array, per `shell.run`/`build.run`/`test.run`'s own schema — joined with a space, since
/// `serde_json::Value::as_str` alone would silently skip it and fall through, treating every
/// distinct shell command as the same bare `"shell.run"` signature), then the first string value
/// found in the input object at all, then nothing. Two calls to the same tool against the same
/// file/query/pattern/command/argv hash to the same signature regardless of any other argument
/// (e.g. a line range or `cacheable` flag), which is deliberate: this is a "did it touch the same
/// ground" check, not a byte-exact input comparison.
fn tool_call_signature(tool_name: &str, input: &serde_json::Value) -> String {
    const KEY_PRIORITY: [&str; 4] = ["path", "query", "pattern", "command"];
    let arg = KEY_PRIORITY
        .iter()
        .find_map(|k| input.get(k).and_then(|v| v.as_str()))
        .map(str::to_string)
        .or_else(|| {
            input
                .get("argv")
                .and_then(|v| v.as_array())
                .and_then(|items| {
                    let joined = items
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    (!joined.is_empty()).then_some(joined)
                })
        })
        .or_else(|| {
            input
                .as_object()
                .and_then(|obj| obj.values().find_map(|v| v.as_str()).map(str::to_string))
        });
    match arg {
        Some(a) => format!("{tool_name}({a})"),
        None => tool_name.to_string(),
    }
}

/// Truncate `s` to at most `max` `char`s, appending an ellipsis when it was cut short.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}

/// Upper bound on the total number of events recorded against one ticket's subject before
/// [`AgentLoop::drive`] force-stops with [`AgentOutcome::Failed`] (`FailureClass::Other`),
/// regardless of `tm_core::CycleBudget`/[`AgentLoop::max_steps`] state — the project-wide "dumb
/// global backstop" `docs/audit-2026-09-18-fable.md` B-09 asks for, mirroring `SPEC.md` §21.5's
/// own "dumb global backstop" phrase for idempotent effects. `tm_scheduler::policy::
/// SchedulingPolicy::max_events_per_ticket` names the same ceiling for a caller that constructs
/// both a scheduling policy and an [`AgentLoop`] together and wants them to agree (no `Cargo.toml`
/// dependency runs from this crate to `tm-scheduler`, so nothing here reads that field directly;
/// a caller that has both threads it in via [`AgentLoop::with_max_events_per_ticket`]).
///
/// Arithmetic behind the default: one step of this loop appends at least a `provider.selected`
/// event and a `usage.recorded` event against the ticket's subject, plus a `goal.reoriented`
/// event once a goal exists, plus whatever a dispatched tool call itself appends (a
/// `command.started`/`command.completed` pair, an `effect.journaled`/`completed`, ...) — call it
/// 3-6 ticket-subject events per step. A full [`DEFAULT_MAX_STEPS`] (64) run is therefore on the
/// order of 200-400 events even when it never trips this at all; the default below is an order of
/// magnitude above that so it only trips on genuine runaway accumulation (e.g. across many
/// resumes of the same ticket, or a pathological caller with `max_steps` set far above the
/// default), never on ordinary single-run work.
pub const DEFAULT_MAX_EVENTS_PER_TICKET: u32 = 5000;

/// Upper bound on generated tokens per provider call. Defined in terms of
/// [`tm_context::sections::STEP_MAX_OUTPUT_TOKENS`] (not the reverse -- this crate already
/// depends on `tm-context`, not the other way around) so this loop's own pre-call dollar
/// estimate ([`estimate_prompt_tokens`]/[`Fabric::affordable_candidates`]) and
/// `tm_context::sections::build_budget`'s "affordable tiers" menu can never silently price a
/// step differently from each other.
const MAX_TOKENS_PER_STEP: u32 = tm_context::sections::STEP_MAX_OUTPUT_TOKENS;
const TOOL_RESULT_COMPACTION_CHARS: usize = 4_000;

/// How long, in total, one provider call waits for fabric capacity before the loop gives up and
/// reports [`FailureClass::ProviderUnavailable`] (`docs/decisions/D-023-capacity-wait-is-not-a-failed-attempt.md`).
/// When the fabric routes a call to [`tm_provider::RouteDecision::Wait`] (every candidate is
/// at its concurrency limit, or out of per-minute quota, or behind an open breaker, and one frees
/// up at a known time), the call waits for that time instead of failing: waiting for capacity is
/// not a failed attempt. Five minutes covers a concurrency slot held by another ticket's long
/// provider call and a per-minute quota window; a daily or monthly cap that would not reset
/// within it fails at once rather than waiting out the allowance. Override with
/// [`AgentLoop::with_capacity_wait`].
pub const DEFAULT_CAPACITY_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// The longest single sleep between two capacity checks. A concurrency slot has no real reset
/// time (the fabric reports a one-second hint), so the loop re-checks at least this often.
const CAPACITY_POLL_MAX: std::time::Duration = std::time::Duration::from_secs(1);

/// The shortest sleep between two capacity checks, so a wait whose `until` is already due (or a
/// slot that another caller keeps winning) never spins.
const CAPACITY_POLL_MIN: std::time::Duration = std::time::Duration::from_millis(25);

/// The prefix [`Fabric::execute`] gives the error it returns for a
/// [`tm_provider::RouteDecision::Wait`]: a routing refusal made before any provider was called.
/// Matched by text because the fabric reports it as a plain `TmError::Provider`;
/// `capacity_refusal_text_matches_the_fabric` pins it against the real fabric so a wording change
/// there fails a test instead of silently turning every capacity wait back into a failure.
const CAPACITY_REFUSAL_PREFIX: &str = "no candidate available for role ";

/// True when `err` is the fabric refusing to route for lack of capacity (see
/// [`CAPACITY_REFUSAL_PREFIX`]), as opposed to a provider that was called and failed.
fn is_capacity_refusal(err: &TmError) -> bool {
    matches!(err, TmError::Provider(msg) if msg.starts_with(CAPACITY_REFUSAL_PREFIX))
}

/// Minimal prompt-cache bookkeeping the loop carries across steps within one run, so repeated
/// system-prompt content can be marked cacheable on providers that support it. Opaque outside
/// this crate; [`AgentLoop`] is the only thing that reads or writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptCacheState {
    /// A stable fingerprint of the last-rendered system prompt, so consecutive steps that
    /// render an identical system prompt can be recognized as cache-eligible without
    /// re-hashing the full text every time.
    pub system_prompt_fingerprint: Option<String>,
}

impl PromptCacheState {
    /// An empty cache state, as every fresh [`AgentLoop`] starts with.
    pub fn new() -> Self {
        PromptCacheState::default()
    }
}

/// Recorded tool-call resolutions [`AgentLoop::with_replay_tool_source`] replays instead of
/// dispatching real tool calls (`replay-tool-replay-mode-design`; see
/// `docs/decisions/D-028-record-replay-harness.md`'s "Tool replay" section for the full design
/// and why a miss is a hard failure). Built from a prior run's own [`StepRecord`]s — never from
/// `tool_call.completed` telemetry events, whose [`ToolCallCompletedPayload`] carries only
/// `tool_name`/`duration_ms`/a collapsed outcome string, not the `tool_use_id`/`input`/full
/// [`crate::outcome::ToolCallResolution`] a verbatim replay needs.
///
/// `Clone` so a caller that builds a fresh [`AgentLoop`] per dispatch (e.g.
/// `crate::executor::BuiltinExecutor::build`) can hand each one its own full, unconsumed copy
/// rather than sharing one source across loops and having a second dispatch see it already
/// partly (or fully) drained by the first.
#[derive(Clone)]
pub struct ReplayToolSource {
    /// Recorded calls not yet consumed by a replayed dispatch, in original step order.
    remaining: Vec<ToolCallRecord>,
}

impl ReplayToolSource {
    /// Build a replay source from a prior run's transcript, flattening every step's tool calls
    /// in order.
    pub fn from_steps(steps: &[StepRecord]) -> Self {
        ReplayToolSource {
            remaining: steps
                .iter()
                .flat_map(|step| step.tool_calls.iter().cloned())
                .collect(),
        }
    }

    /// True once every recorded call has been matched and consumed.
    pub fn is_exhausted(&self) -> bool {
        self.remaining.is_empty()
    }

    /// Find and consume the recorded call matching `id`/`name`/`input`, first by `tool_use_id`
    /// (exact — a provider-assigned id is only ever reused for its own call), then by
    /// `tool_name` plus a `root`-normalized input hash (a fresh run's provider mints fresh
    /// `tool_use_id`s, so replaying under a re-run rather than a byte-identical resumed session
    /// falls through to this). `None` means no recorded call matches at all — the caller turns
    /// that into [`REPLAY_DIVERGENCE_PREFIX`]'s hard failure, never a real dispatch. Each record
    /// is consumed at most once, so two identical calls in the recording are matched to their own
    /// distinct recorded resolutions in order rather than both replaying the first one found.
    fn take(
        &mut self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
        root: &Path,
    ) -> Option<crate::outcome::ToolCallResolution> {
        if let Some(pos) = self.remaining.iter().position(|r| r.tool_use_id == id) {
            return Some(self.remaining.remove(pos).resolution);
        }
        let target = hash_tool_input(input, root);
        let pos = self
            .remaining
            .iter()
            .position(|r| r.tool_name == name && hash_tool_input(&r.input, root) == target)?;
        Some(self.remaining.remove(pos).resolution)
    }
}

/// Replace every occurrence of `root`'s string form in `value`'s string leaves with
/// [`tm_provider::cassette::PATH_PLACEHOLDER`] — the same normalization
/// [`tm_provider::cassette::normalize_request`] applies to a provider prompt, applied here to a
/// tool call's JSON input instead, so a recording made under one tempdir/project root still
/// matches a replay under a different one (`fs.read { "path": "<root>/..." }` is the routine
/// case; every builtin `fs.*`/`edit.*`/`shell.*` tool's input embeds the root somewhere).
fn normalize_tool_input_strings(value: &mut serde_json::Value, root_str: &str) {
    match value {
        serde_json::Value::String(s) => {
            *s = s.replace(root_str, tm_provider::cassette::PATH_PLACEHOLDER);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_tool_input_strings(item, root_str);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                normalize_tool_input_strings(v, root_str);
            }
        }
        _ => {}
    }
}

/// [`std::hash::DefaultHasher`] of `input` after normalizing `root`'s string form out of every
/// string leaf (see [`normalize_tool_input_strings`]) — the "normalized input hash"
/// `replay-tool-replay-mode-design` asks [`ReplayToolSource::take`] to match a recorded call's
/// `tool_name` fallback on. Mirrors `tm_provider::mock::hash_request`'s own canonical-JSON-then-hash
/// shape.
fn hash_tool_input(input: &serde_json::Value, root: &Path) -> u64 {
    let root_str = root.to_string_lossy();
    let mut normalized = input.clone();
    if !root_str.is_empty() {
        normalize_tool_input_strings(&mut normalized, root_str.as_ref());
    }
    // Invariant: `serde_json::Value` serialization cannot fail.
    let json = serde_json::to_string(&normalized)
        .expect("serde_json::Value serialization should never fail");
    let mut hasher = std::hash::DefaultHasher::new();
    json.as_bytes().hash(&mut hasher);
    hasher.finish()
}

/// Prefix every [`AgentOutcome::Failed`] detail [`AgentLoop::drive`]/[`AgentLoop::resume_with`]
/// produce for a tool-replay miss (no recorded [`ToolCallRecord`] matched a call the model
/// issued — see [`ReplayToolSource::take`]) starts with, so a caller can recognize this specific
/// failure without string-matching an unrelated one (mirrors [`CAPACITY_REFUSAL_PREFIX`]'s own
/// role for a capacity-wait timeout). Tool replay never falls through to a real dispatch on a
/// miss — `docs/decisions/D-028-record-replay-harness.md`'s "Tool replay" section is explicit
/// this is a hard stop, not a stale-recording warning.
pub const REPLAY_DIVERGENCE_PREFIX: &str = "tool replay diverged: ";

/// True when `outcome` is the [`REPLAY_DIVERGENCE_PREFIX`]-tagged failure a tool-replay miss
/// produces, as opposed to any other [`AgentOutcome::Failed`].
pub fn is_replay_divergence(outcome: &AgentOutcome) -> bool {
    matches!(
        outcome,
        AgentOutcome::Failed { class: FailureClass::Other, detail, .. }
            if detail.starts_with(REPLAY_DIVERGENCE_PREFIX)
    )
}

/// The tool-using agent loop.
pub struct AgentLoop {
    fabric: Arc<Fabric>,
    tools: ToolRegistry,
    authority: Authority,
    budget: Budget,
    /// The human-approval policy `drive` applies to an otherwise-`Allow`ed action before
    /// dispatching it — see [`AgentLoop::with_oversight`]. Defaults to [`Oversight::autonomous`]
    /// (asks nothing), matching this loop's behavior before M-16 wired a real caller.
    oversight: Oversight,
    /// Override for every tool call's [`tm_types::CallContext::root`] this run — see
    /// [`AgentLoop::with_root`]. `None` (the default) falls back to [`project_root`] (the
    /// process's own current directory), this loop's behavior before `--worktree` (`tm run
    /// <ticket> --worktree`, `docs/decisions/D-012-run-worktree-isolation.md`) needed a way to
    /// point a run's file/git tool calls at an isolated checkout instead.
    root_override: Option<PathBuf>,
    /// The system-prompt fragments every request renders with; empty unless a caller sets them
    /// via [`AgentLoop::with_prompt_fragments`].
    prompt_fragments: PromptFragments,
    /// Where each completed step is sent the moment it is recorded, for a caller that wants live
    /// progress rather than the whole transcript at the end — see [`AgentLoop::with_step_sender`].
    step_sender: Option<tokio::sync::mpsc::UnboundedSender<StepRecord>>,
    cache: PromptCacheState,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    role: Role,
    actor: ParticipantId,
    max_steps: u32,
    /// How many times a ticketed run's text-only turn gets nudged instead of failed outright;
    /// see [`DEFAULT_MAX_SUBMIT_NUDGES`] and [`AgentLoop::with_max_submit_nudges`].
    max_submit_nudges: u32,
    /// How long one provider call may wait for fabric capacity; see [`DEFAULT_CAPACITY_WAIT`].
    capacity_wait: std::time::Duration,
    /// The [`DEFAULT_MAX_EVENTS_PER_TICKET`]-shaped backstop this loop enforces; see that
    /// constant's doc comment.
    max_events_per_ticket: u32,
    /// Durable home for everything this loop produces (`docs/audit-2026-09-18-fable.md` B-07):
    /// session bracketing, usage debiting/history, provider routing events and approval
    /// events. Every write through this handle goes through [`tm_core::Store::append`] or one
    /// of its typed helpers, so it materializes in the same transaction it's appended in.
    store: Arc<Store>,
    /// When set (see [`AgentLoop::with_replay_tool_source`]), every tool call this loop
    /// dispatches is resolved from here instead of a real [`ToolRegistry::dispatch`] call —
    /// tool-replay mode (`replay-tool-replay-mode-design`,
    /// `docs/decisions/D-028-record-replay-harness.md`'s "Tool replay" section). `None` (the
    /// default) is this loop's ordinary behavior, unchanged.
    replay_tool_source: Option<ReplayToolSource>,
}

impl AgentLoop {
    /// Build a loop over `fabric` and `tools` (however many [`tm_types::CapabilityProvider`]s
    /// the caller assembled `tools` from — `docs/audit-2026-09-18-fable.md` A-01), gated by
    /// `authority` and capped at `budget` (this is the loop's own ceiling; [`AgentTask::budget`]
    /// is intersected with it per run so neither can override the other upward).
    ///
    /// Every dependency a builtin tool needs (code intelligence, project state, command
    /// execution) is already baked into `tools` by whoever built it (see
    /// [`ToolRegistry::standard`]); this loop only ever threads authority, identity and injected
    /// time/id sources through per call via [`tm_types::CallContext`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fabric: Arc<Fabric>,
        tools: ToolRegistry,
        authority: Authority,
        budget: Budget,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        role: Role,
        actor: ParticipantId,
        store: Arc<Store>,
    ) -> Self {
        AgentLoop {
            fabric,
            tools,
            authority,
            budget,
            oversight: Oversight::autonomous(),
            root_override: None,
            prompt_fragments: PromptFragments {
                system_preamble: String::new(),
                closing_reminder: String::new(),
                extra: BTreeMap::new(),
            },
            step_sender: None,
            cache: PromptCacheState::new(),
            clock,
            ids,
            role,
            actor,
            max_steps: DEFAULT_MAX_STEPS,
            max_submit_nudges: DEFAULT_MAX_SUBMIT_NUDGES,
            capacity_wait: DEFAULT_CAPACITY_WAIT,
            max_events_per_ticket: DEFAULT_MAX_EVENTS_PER_TICKET,
            store,
            replay_tool_source: None,
        }
    }

    /// Override how long one provider call may wait for fabric capacity before failing (see
    /// [`DEFAULT_CAPACITY_WAIT`]). `Duration::ZERO` restores fail-fast behavior.
    pub fn with_capacity_wait(mut self, wait: std::time::Duration) -> Self {
        self.capacity_wait = wait;
        self
    }

    /// Override how many times a ticketed run's text-only turn gets nudged before this loop
    /// fails the attempt with "model ended turn without submitting" (see
    /// [`DEFAULT_MAX_SUBMIT_NUDGES`]). `0` restores the old fail-immediately behavior.
    pub fn with_max_submit_nudges(mut self, max_submit_nudges: u32) -> Self {
        self.max_submit_nudges = max_submit_nudges;
        self
    }

    /// Override the default step limit (mainly for tests that want to force
    /// [`AgentOutcome::Failed`] quickly).
    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// Override the default per-ticket event backstop (see [`DEFAULT_MAX_EVENTS_PER_TICKET`]) —
    /// mainly for tests that want to force it to trip quickly, or a caller that has derived a
    /// project-specific ceiling (e.g. from `tm_scheduler::policy::SchedulingPolicy::
    /// max_events_per_ticket`).
    pub fn with_max_events_per_ticket(mut self, max_events_per_ticket: u32) -> Self {
        self.max_events_per_ticket = max_events_per_ticket;
        self
    }

    /// Override the default [`Oversight::autonomous`] policy — the caller-facing seam
    /// `crates/tm-cli/src/dispatch.rs`'s `load_oversight` feeds a project's parsed
    /// `oversight.toml` (or [`Oversight::default`] when absent) through.
    pub fn with_oversight(mut self, oversight: Oversight) -> Self {
        self.oversight = oversight;
        self
    }

    /// Stop asking about `class` (an action class such as `shell.run`) for the rest of this
    /// loop's life: the human answered "Yes, and don't ask again".
    pub fn approve_for_session(&mut self, class: String) {
        self.oversight.approved_for_session.insert(class);
    }

    /// Point every tool call this loop dispatches (`fs.*`/`edit.*`/`git.*`/`shell.*`) at `root`
    /// instead of the default [`project_root`] (the process's own current directory) — the seam
    /// `crate::executor::BuiltinExecutor::with_root` uses to run a ticket against an isolated
    /// `git worktree` checkout rather than the main working tree (`tm run <ticket> --worktree`,
    /// `docs/decisions/D-012-run-worktree-isolation.md`). Deliberately *not* wired into
    /// `AgentTask`/`ExecutorTask` (`tm-core`, shared by every [`tm_core::Executor`] impl):
    /// this loop already owns `project_root()`'s fallback, so overriding it here composes with
    /// the existing [`crate::executor::BuiltinExecutor`]/`AgentLoop` construction instead of
    /// widening a trait every executor implements.
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root_override = Some(root);
        self
    }

    /// Replay recorded tool-call resolutions instead of dispatching real tool calls for the rest
    /// of this loop's life (`replay-tool-replay-mode-design`;
    /// `docs/decisions/D-028-record-replay-harness.md`'s "Tool replay" section) — the tool-level
    /// analog of `tm run <ticket> --replay <path>`'s provider-level cassette replay
    /// (`RunArgs::replay`/`tm_provider::MockProvider::script_from_cassette`), applied to this
    /// crate's own [`ToolCallRecord`]s rather than `tm-provider`'s `CompletionRequest`s. Every
    /// dispatched call is matched against `source` and its recorded resolution returned verbatim
    /// (see [`ReplayToolSource::take`]); a call with no match ends the run with
    /// [`is_replay_divergence`]'s failure rather than falling through to a real dispatch. This
    /// does not touch the authority/oversight gate `drive` applies before every dispatch — see
    /// that gate's own call site for why a replay under a different (e.g. stricter) authority
    /// ceiling than the recording still suspends/denies exactly as a live run would.
    pub fn with_replay_tool_source(mut self, source: ReplayToolSource) -> Self {
        self.replay_tool_source = Some(source);
        self
    }

    /// Render every request's system prompt from `fragments` (see
    /// [`crate::prompt::render_system_prompt`]) instead of the empty default — e.g. an interactive
    /// session's [`crate::prompt::chat_fragments`].
    pub fn with_prompt_fragments(mut self, fragments: PromptFragments) -> Self {
        self.prompt_fragments = fragments;
        self
    }

    /// Send every step to `sender` as soon as it is recorded (a tool round finished, or the final
    /// reply arrived), so a front end can show progress while the turn is still running. The
    /// outcome still carries the full transcript; a closed receiver is ignored.
    pub fn with_step_sender(
        mut self,
        sender: tokio::sync::mpsc::UnboundedSender<StepRecord>,
    ) -> Self {
        self.step_sender = Some(sender);
        self
    }

    /// Record `step` in `steps` and report it to [`AgentLoop::with_step_sender`]'s receiver.
    fn push_step(&self, steps: &mut Vec<StepRecord>, step: StepRecord) {
        if let Some(sender) = &self.step_sender {
            let _ = sender.send(step.clone());
        }
        steps.push(step);
    }

    /// Look up whether tool-replay mode is active and, if so, resolve one already-authority-gated
    /// tool call from it — the one seam both [`AgentLoop::drive`]'s main loop and
    /// [`AgentLoop::resume_with`]'s resumed dispatch share, so the two can never diverge on how
    /// they decide "replay or really run it". `None` means [`AgentLoop::with_replay_tool_source`]
    /// was never called for this loop — the caller falls through to a real
    /// [`ToolRegistry::dispatch`]. `Some(Err(detail))` is a tool-replay miss: `detail` is
    /// [`REPLAY_DIVERGENCE_PREFIX`]-tagged plain text for the caller to fold into an
    /// [`AgentOutcome::Failed`] the same way every other in-band failure this loop produces is —
    /// never a real `Err` out of `drive`/`resume_with` themselves (see [`AgentLoop::run`]'s own
    /// doc comment on that invariant).
    ///
    /// Takes the call's own fields rather than a [`tm_types::CallContext`], deliberately: this
    /// mutable borrow of `self.replay_tool_source` must never overlap the immutable borrows of
    /// `self.actor`/`self.clock`/`self.ids` a `CallContext` for the real-dispatch branch holds —
    /// see both call sites, which build that `CallContext` only in the `None` arm, after this
    /// call (and its mutable borrow of `self`) has already returned.
    fn replay_resolution(
        &mut self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
        root: &Path,
    ) -> Option<std::result::Result<ToolOutcome, String>> {
        let source = self.replay_tool_source.as_mut()?;
        Some(source.take(id, name, input, root).ok_or_else(|| {
            format!(
                "{REPLAY_DIVERGENCE_PREFIX}no recorded resolution for tool `{name}` (call {id}); \
                 replay stops instead of dispatching it for real"
            )
        }))
    }

    /// The root every tool call this loop dispatches resolves paths against: [`AgentLoop::with_root`]'s
    /// override if set, else [`project_root`]. Exposed (mirroring [`AgentLoop::authority`]/
    /// [`AgentLoop::oversight`]) so a caller constructing the loop can be tested for *what* root
    /// it actually passed, not just inferred from where files land.
    pub fn root(&self) -> PathBuf {
        self.root_override.clone().unwrap_or_else(project_root)
    }

    /// Whether [`AgentLoop::with_replay_tool_source`] set a tool-replay source on this loop.
    /// Exposed for the same reason [`AgentLoop::root`] is: so a caller constructing the loop
    /// (e.g. `crate::executor::BuiltinExecutor::build`) can be tested for *what* it passed, not
    /// just inferred from run-time dispatch behavior.
    pub fn has_replay_tool_source(&self) -> bool {
        self.replay_tool_source.is_some()
    }

    /// This loop's own authority ceiling, as constructed via [`AgentLoop::new`] — i.e. before
    /// intersecting with any particular [`AgentTask::authority`]. Exposed so a caller
    /// constructing the loop (e.g. `BuiltinExecutor`) can be tested for *what* it passed as the
    /// ceiling, not just inferred from run-time behaviour.
    pub fn authority(&self) -> &Authority {
        &self.authority
    }

    /// This loop's own oversight policy, as constructed via [`AgentLoop::new`]/
    /// [`AgentLoop::with_oversight`]. See [`AgentLoop::authority`].
    pub fn oversight(&self) -> &Oversight {
        &self.oversight
    }

    /// This loop's own budget ceiling, as constructed via [`AgentLoop::new`]. See
    /// [`AgentLoop::authority`].
    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    /// The tool registry this loop dispatches through, as constructed via [`AgentLoop::new`] —
    /// i.e. every [`tm_types::CapabilityProvider`] the caller assembled it from. Exposed so a
    /// caller constructing the loop (e.g. `BuiltinExecutor`) can assert *which* capabilities got
    /// registered, the same reason [`AgentLoop::authority`]/[`AgentLoop::budget`] exist.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Run `task` to completion (or suspension), driving the provider fabric and tool registry.
    ///
    /// # Errors
    /// Returns `Err` only for an infrastructure failure that makes it impossible to produce any
    /// `AgentOutcome` at all (e.g. the harness epoch's prompt fragments can't be resolved).
    /// Every in-band failure — a denied tool, a budget exhaustion, a provider error, a step
    /// limit — is represented as an `Ok(AgentOutcome::Failed { .. })` or the matching variant,
    /// never as an `Err`.
    pub async fn run(&mut self, task: AgentTask) -> Result<AgentOutcome> {
        self.store.append(vec![self.session_started_draft(&task)])?;
        let outcome = self.drive(&task, Vec::new()).await;
        self.finish_session(&task, &outcome)?;
        outcome
    }

    /// Resume a previously suspended run after `approval.decided` landed.
    ///
    /// # Errors
    /// Returns `Err` under the same infrastructure-failure conditions as [`AgentLoop::run`].
    pub async fn resume(
        &mut self,
        task: AgentTask,
        steps_so_far: Vec<crate::outcome::StepRecord>,
        pending: crate::outcome::PendingApproval,
        approved: bool,
    ) -> Result<AgentOutcome> {
        let denial = (!approved).then(|| "approval declined".to_string());
        self.resume_with(task, steps_so_far, pending, denial).await
    }

    /// [`AgentLoop::resume`], where a denial can carry the human's own words ("No, and tell tm
    /// what to do differently"): `denial` is `None` to approve, or `Some(reason)` to deny, and
    /// `reason` is exactly what the model sees as the tool result.
    ///
    /// # Errors
    /// Returns `Err` under the same infrastructure-failure conditions as [`AgentLoop::run`].
    pub async fn resume_with(
        &mut self,
        task: AgentTask,
        steps_so_far: Vec<crate::outcome::StepRecord>,
        pending: crate::outcome::PendingApproval,
        denial: Option<String>,
    ) -> Result<AgentOutcome> {
        let approved = denial.is_none();
        let effective_authority = self.authority.intersect(&task.authority);
        let root = self.root();

        // The decision point itself: a human (or whatever resumed this suspended run) has
        // already decided `approved` by the time `resume` is called, independent of how the
        // dispatch below resolves — see `docs/audit-2026-09-18-fable.md` B-07.
        self.store.append(vec![EventDraft::new(
            self.actor.clone(),
            task.subject(),
            Payload::from(ApprovalDecidedPayload {
                ticket: task.ticket.clone(),
                decided_by: self.actor.clone(),
                approved,
                note: None,
            }),
        )
        .with_session(task.session.clone())])?;

        let call = ToolCall {
            id: pending.tool_use_id.clone(),
            name: pending.tool_name.clone(),
            input: pending.input.clone(),
        };

        let dispatch_started_at = self.clock.now();
        let resolution = if approved {
            // `goal.claimed_complete` before dispatch, not after (`SPEC.md` §29,
            // `docs/audit-2026-09-18-fable.md` B-09) — see `drive`'s matching call site for the
            // full rationale: the claim records the loop's belief at the moment it decided to
            // submit, independent of whether the dispatch below actually succeeds.
            if let (TICKET_SUBMIT, Some(ticket)) = (pending.tool_name.as_str(), &task.ticket) {
                self.store.claim_goal_complete(
                    ticket,
                    submit_summary(&pending.input),
                    self.actor.clone(),
                )?;
            }
            match self.replay_resolution(&call.id, &call.name, &call.input, &root) {
                Some(Ok(resolution)) => resolution,
                Some(Err(detail)) => {
                    return Ok(AgentOutcome::Failed {
                        steps: steps_so_far,
                        class: FailureClass::Other,
                        detail,
                    });
                }
                None => {
                    let ctx = CallContext {
                        authority: &effective_authority,
                        ticket: task.ticket.as_ref(),
                        session: &task.session,
                        actor: &self.actor,
                        clock: self.clock.as_ref(),
                        ids: self.ids.as_ref(),
                        root: &root,
                    };
                    self.tools.dispatch(&call, &ctx).await
                }
            }
        } else {
            ToolOutcome::Denied {
                reason: denial.unwrap_or_else(|| "approval declined".to_string()),
            }
        };
        // Same `tool_call.completed` bookkeeping as `AgentLoop::drive`'s own loop
        // (`tel-tool-call-event-kind`): `duration_ms` is `0` for the declined-without-dispatch
        // path (`approved` false), since nothing ran to time.
        let duration_ms = self.clock.now().millis_since(dispatch_started_at).max(0) as u64;
        self.record_tool_call_events(
            &task,
            vec![ToolCallCompletedPayload {
                ticket: task.ticket.clone(),
                session: Some(task.session.clone()),
                tool_name: pending.tool_name.clone(),
                duration_ms,
                outcome: match &resolution {
                    ToolOutcome::Completed { .. } => "completed",
                    ToolOutcome::Denied { .. } => "denied",
                    ToolOutcome::Errored { .. } => "error",
                }
                .to_string(),
                result_bytes: outcome_result_bytes(&resolution),
                truncated: outcome_truncated(&resolution),
            }],
        )?;

        let mut steps = steps_so_far;
        let step_index = steps.len() as u32 + 1;
        let tool_call_record = ToolCallRecord {
            tool_use_id: pending.tool_use_id.clone(),
            tool_name: pending.tool_name.clone(),
            input: pending.input.clone(),
            resolution: resolution.clone(),
        };
        let step = StepRecord {
            index: step_index,
            served_by: "resumed".to_string(),
            assistant_text: None,
            tool_calls: vec![tool_call_record],
            spend: Spend::default(),
            at: self.clock.now(),
        };

        if let (TICKET_SUBMIT, ToolOutcome::Completed { .. }, Some(ticket)) =
            (pending.tool_name.as_str(), &resolution, &task.ticket)
        {
            {
                self.push_step(&mut steps, step);
                let evidence = self.build_evidence(&task, ticket, &steps, &pending.input);
                let outcome = Ok(AgentOutcome::Submitted { evidence, steps });
                self.finish_session(&task, &outcome)?;
                return outcome;
            }
        }

        self.push_step(&mut steps, step);
        let outcome = self.drive(&task, steps).await;
        self.finish_session(&task, &outcome)?;
        outcome
    }

    /// The loop's current prompt-cache bookkeeping, for diagnostics.
    pub fn cache_state(&self) -> &PromptCacheState {
        &self.cache
    }

    /// The `session.started` draft that brackets a fresh [`AgentLoop::run`], keyed on
    /// [`AgentTask::session`] — the identity every tool call and evidence artifact this run
    /// produces is already attributed to, so the store's session bookkeeping uses the same id
    /// rather than minting a second, unrelated one.
    fn session_started_draft(&self, task: &AgentTask) -> EventDraft {
        EventDraft::new(
            self.actor.clone(),
            Id::from(task.session.clone()),
            Payload::from(SessionStartedPayload {
                session: task.session.clone(),
                participant: self.actor.clone(),
            }),
        )
        .with_session(task.session.clone())
    }

    /// The `session.ended` draft that closes out [`AgentTask::session`] once a run reaches a
    /// terminal [`AgentOutcome`] (see [`AgentLoop::finish_session`]).
    fn session_ended_draft(&self, task: &AgentTask) -> EventDraft {
        EventDraft::new(
            self.actor.clone(),
            Id::from(task.session.clone()),
            Payload::from(SessionEndedPayload {
                session: task.session.clone(),
            }),
        )
        .with_session(task.session.clone())
    }

    /// Append `session.ended` once `outcome` is a terminal [`AgentOutcome`] (every variant except
    /// [`AgentOutcome::AwaitingApproval`] — success, failure, or budget exhaustion all close the
    /// session; only a suspension leaves it open for a subsequent [`AgentLoop::resume`]).
    /// A `outcome` that is itself `Err` (an infrastructure failure) closes nothing, since no
    /// `AgentOutcome` was produced to be terminal or not.
    fn finish_session(&self, task: &AgentTask, outcome: &Result<AgentOutcome>) -> Result<()> {
        if let Ok(o) = outcome {
            if o.is_terminal() {
                self.store.append(vec![self.session_ended_draft(task)])?;
            }
        }
        Ok(())
    }

    /// Debit `spend` against `task.ticket` (and its ancestor scopes) via
    /// [`tm_core::Store::record_usage_attributed`], and record it durably as `usage.recorded`,
    /// attributed to `served_by` (`tel-usage-payload-model-field`).
    ///
    /// A [`TmError::BudgetExhausted`] from the store is deliberately swallowed here rather than
    /// propagated: [`AgentLoop::drive`]'s own `effective_budget` tracking (updated by the caller
    /// right alongside this call) already reflects the same overspend, and
    /// [`first_exhausted_dimension`] at the top of the *next* loop iteration is what actually
    /// produces `AgentOutcome::BudgetExhausted` — preserving the "never mid-edit" contract that
    /// method's doc comment promises (this step's tool calls still need to run to completion).
    ///
    /// The reconciliation this doc comment used to defer to a future item
    /// (`docs/audit-2026-09-18-fable.md` B-10) now exists on the *store* side of this same call:
    /// when the overspend crosses the ticket's own durable budget and it is still `Leased`/
    /// `Running`, [`tm_core::Store::record_usage`]'s handoff-eligible branch has already released
    /// its lease and transitioned it `-> Ready` via `Trigger::BudgetHandoff` by the time this
    /// method returns — swallowing the error here does not swallow that transition, only the
    /// loop's own notification of it (which [`first_exhausted_dimension`]/[`AgentLoop::
    /// can_afford`] still surface as `AgentOutcome::BudgetExhausted`, now itself backed by a
    /// second, defensive call to [`tm_core::Store::budget_handoff`] — see
    /// [`AgentLoop::budget_handoff_outcome`]).
    fn record_usage(
        &self,
        task: &AgentTask,
        spend: Spend,
        served_by: &tm_provider::ModelId,
    ) -> Result<()> {
        match self.store.record_usage_attributed(
            task.ticket.as_ref(),
            Some(&task.session),
            spend,
            self.actor.clone(),
            Some((served_by.provider.clone(), served_by.model.clone())),
        ) {
            Ok(_events) => Ok(()),
            Err(TmError::BudgetExhausted(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether `effective_budget` can still afford `estimated_cost` in every dimension it
    /// tracks (`SPEC.md` §31.2 "refuse to start what it cannot finish",
    /// `docs/audit-2026-09-18-fable.md` B-10). A conservative admission check, not a prediction
    /// of the provider's actual bill: [`AgentLoop::drive`] calls this before every
    /// `fabric.execute`, and `false` means attempting the call would almost certainly overspend,
    /// not merely that it might — see [`first_unaffordable_dimension`], which this delegates to.
    fn can_afford(&self, effective_budget: &Budget, estimated_cost: Spend) -> bool {
        first_unaffordable_dimension(effective_budget, estimated_cost).is_none()
    }

    /// Hand `task.ticket` off cleanly via [`tm_core::Store::budget_handoff`] and report
    /// [`AgentOutcome::BudgetExhausted`] (`SPEC.md` §31.3, `docs/audit-2026-09-18-fable.md`
    /// B-10) — the shared tail of both places [`AgentLoop::drive`] discovers it cannot proceed on
    /// budget: reactively (a dimension already at zero) and proactively (`AgentLoop::can_afford`
    /// says the next call would overrun one). A store-side failure here is logged, not
    /// propagated: the loop still owes its caller a terminal `AgentOutcome`, and a worker that
    /// merely lost a race with the store's own bookkeeping (e.g. `Store::record_usage`'s
    /// reactive handoff already ran for the same reason) must not fail the whole run over it —
    /// the ticket's lease still expires on its own if nothing else clears it.
    fn budget_handoff_outcome(
        &self,
        task: &AgentTask,
        steps: Vec<StepRecord>,
        exhausted: BudgetDimension,
    ) -> Result<AgentOutcome> {
        let Some(ticket) = &task.ticket else {
            // No ticket, no lease to hand back: a ticketless chat turn just ends here.
            return Ok(AgentOutcome::BudgetExhausted { steps, exhausted });
        };
        if let Err(e) =
            self.store
                .budget_handoff(ticket, format!("{exhausted:?}"), self.actor.clone())
        {
            tracing::warn!(
                ticket = %ticket,
                error = %e,
                "budget handoff failed; the ticket's lease will still expire on its own"
            );
        }
        Ok(AgentOutcome::BudgetExhausted { steps, exhausted })
    }

    /// [`Fabric::execute`] `request`, waiting for capacity instead of failing when the fabric has
    /// none right now (`docs/decisions/D-023-capacity-wait-is-not-a-failed-attempt.md`).
    ///
    /// A call is retried only when the fabric refused to route it ([`is_capacity_refusal`]: no
    /// provider was called), never after a provider was called and failed. Before each try the
    /// loop asks [`Fabric::route`] whether a candidate is free; the check after a refusal is what
    /// matters, because two loops dispatched in the same scheduler tick routinely both see a free
    /// slot and one of them loses it. The wait is bounded by [`AgentLoop::with_capacity_wait`],
    /// counted as time actually slept (an injected clock need not advance), and a `Wait` whose
    /// `until` lies beyond what is left of that allowance fails at once. Nothing is appended to the
    /// log while waiting; the caller records provider events once, for the final result.
    async fn execute_with_capacity_wait(
        &self,
        request: CompletionRequest,
    ) -> Result<(tm_provider::Completion, Option<u64>)> {
        let mut waited = std::time::Duration::ZERO;
        loop {
            // `Fabric::route` doesn't roll quota windows forward (only `execute` does), so a
            // `Wait` already due may be stale: let `execute` decide those.
            if let tm_provider::RouteDecision::Wait(until) = self.route_now(&request) {
                let pause = (until > self.clock.now())
                    .then(|| self.capacity_pause(until, waited))
                    .flatten();
                if let Some(pause) = pause {
                    tokio::time::sleep(pause).await;
                    waited += pause;
                    continue;
                }
            }
            // `execute_priced` (not `execute`) so the real per-call cost, when the served
            // candidate has a `Price` configured, reaches the caller instead of being dropped
            // (`tel-completion-cost-field`).
            let err = match self.fabric.execute_priced(self.role, request.clone()).await {
                Ok((completion, cost_micros)) => return Ok((completion, cost_micros)),
                Err(err) => err,
            };
            if !is_capacity_refusal(&err) {
                return Err(err);
            }
            let pause = match self.route_now(&request) {
                tm_provider::RouteDecision::Wait(until) => self.capacity_pause(until, waited),
                // The slot freed between the refusal and this check: try again, after the
                // minimum pause so a slot other callers keep winning can't make this spin.
                tm_provider::RouteDecision::Use(_) | tm_provider::RouteDecision::Degrade(..) => {
                    self.capacity_pause(self.clock.now(), waited)
                }
                tm_provider::RouteDecision::Exhausted => None,
            };
            let Some(pause) = pause else {
                return Err(err);
            };
            tracing::debug!(
                role = self.role.as_str(),
                waited_ms = waited.as_millis() as u64,
                "provider at capacity; waiting instead of failing the attempt"
            );
            tokio::time::sleep(pause).await;
            waited += pause;
        }
    }

    /// The fabric's routing decision for `request` right now, built from the same [`Need`] shape
    /// [`Fabric::execute`] itself routes with.
    ///
    /// [`Need`]: tm_provider::Need
    fn route_now(&self, request: &CompletionRequest) -> tm_provider::RouteDecision {
        let need = tm_provider::Need {
            tolerance: self.role.default_tolerance(),
            estimated_tokens: request.max_tokens,
            max_cost_micros: None,
        };
        self.fabric.route(self.role, &need, self.clock.now())
    }

    /// How long to sleep before re-checking capacity that frees up at `until`, having already
    /// waited `waited`; `None` when the wait allowance is spent or `until` lies beyond it.
    fn capacity_pause(
        &self,
        until: tm_types::Timestamp,
        waited: std::time::Duration,
    ) -> Option<std::time::Duration> {
        let remaining = self.capacity_wait.checked_sub(waited)?;
        if remaining.is_zero() {
            return None;
        }
        let due_in =
            std::time::Duration::from_millis(until.millis_since(self.clock.now()).max(0) as u64);
        if due_in > remaining {
            return None;
        }
        Some(
            due_in
                .clamp(CAPACITY_POLL_MIN, CAPACITY_POLL_MAX)
                .min(remaining),
        )
    }

    /// Translate every [`FabricRecord`] the most recent [`tm_provider::fabric::Fabric::execute`]
    /// call produced into the matching already-closed `provider.*` [`tm_events::EventKind`]
    /// (`docs/audit-2026-09-18-fable.md` B-07) and append them together. `FabricRecord::Degraded`
    /// and `FabricRecord::Recovered` map onto payloads with a single `provider: String` slot (no
    /// separate `model` field, unlike `provider.selected`'s), so the candidate's
    /// `{provider}/{model}` `Display` form is used there to avoid losing which model was
    /// involved.
    fn record_provider_events(&self, task: &AgentTask) -> Result<()> {
        let records = self.fabric.last_records();
        if records.is_empty() {
            return Ok(());
        }
        let drafts: Vec<EventDraft> = records
            .into_iter()
            .map(|record| {
                let payload = match record {
                    FabricRecord::Selected { role, candidate } => {
                        Payload::from(ProviderSelectedPayload {
                            role: role.as_str().to_string(),
                            provider: candidate.provider,
                            model: candidate.model,
                        })
                    }
                    FabricRecord::Degraded {
                        candidate, reason, ..
                    } => Payload::from(ProviderDegradedPayload {
                        provider: candidate.to_string(),
                        reason,
                    }),
                    FabricRecord::Exhausted { role } => Payload::from(ProviderExhaustedPayload {
                        role: role.as_str().to_string(),
                        provider: "none".to_string(),
                        reason: "no candidate available for this role".to_string(),
                    }),
                    FabricRecord::Recovered { candidate } => {
                        Payload::from(ProviderRecoveredPayload {
                            provider: candidate.to_string(),
                        })
                    }
                };
                EventDraft::new(self.actor.clone(), task.subject(), payload)
                    .with_session(task.session.clone())
            })
            .collect();
        self.store.append(drafts)?;
        Ok(())
    }

    /// Append one `tool_call.completed` event per already-built `payloads`
    /// (`tel-tool-call-event-kind`), batched together in a single [`tm_core::Store::append`] call
    /// like [`AgentLoop::record_provider_events`] above — one event per dispatched tool call in
    /// the step just finished, not per step. Building each [`ToolCallCompletedPayload`] (tool
    /// name, measured duration, and an `outcome` of `"completed"`/`"denied"`/`"error"` mirroring
    /// [`crate::outcome::ToolCallResolution`]'s three arms) is the caller's job, since that is
    /// also where the call's wall time is actually measured — [`crate::outcome::ToolCallRecord`]
    /// itself carries no timing field to read it back from afterward.
    fn record_tool_call_events(
        &self,
        task: &AgentTask,
        payloads: Vec<ToolCallCompletedPayload>,
    ) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }
        let drafts: Vec<EventDraft> = payloads
            .into_iter()
            .map(|payload| {
                EventDraft::new(self.actor.clone(), task.subject(), Payload::from(payload))
                    .with_session(task.session.clone())
            })
            .collect();
        self.store.append(drafts)?;
        Ok(())
    }

    /// Seed `task.ticket`'s durable goal from its own `objective` field, if no goal has ever been
    /// set for it (`SPEC.md` §29, `docs/audit-2026-09-18-fable.md` B-09's "step 0").
    ///
    /// Best-effort, not infrastructure-fatal: a store read failure or a ticket that doesn't exist
    /// in the store at all (many of this module's own tests construct an [`AgentTask`] against a
    /// ticket id without ever creating it in `self.store` — deliberately, since they only care
    /// about the loop's provider/tool-dispatch behavior) is treated as "nothing to seed a goal
    /// from" rather than propagated, so this call can sit unconditionally at the top of every
    /// [`AgentLoop::drive`] call without becoming a new way for an existing, unrelated test to
    /// fail.
    fn ensure_goal_set(&self, task: &AgentTask) -> Result<()> {
        // A ticketless chat turn has no ticket objective to seed a durable goal from.
        let Some(ticket_id) = &task.ticket else {
            return Ok(());
        };
        if self.store.goal_state(ticket_id)?.is_some() {
            return Ok(());
        }
        let Ok(view) = self.store.view() else {
            return Ok(());
        };
        let Some(ticket) = view.tickets.get(ticket_id) else {
            return Ok(());
        };
        self.store
            .set_goal(ticket_id, ticket.objective.clone(), self.actor.clone())?;
        Ok(())
    }

    /// Re-orientation (`SPEC.md` §29): re-read `task.ticket`'s current goal state from the store
    /// and, if one exists, durably record that this step re-grounded against it via
    /// `goal.reoriented`, returning the state that was read. A ticket with no goal set yet (e.g.
    /// `ensure_goal_set` found nothing to seed from) has nothing to reorient against, so this is
    /// a no-op rather than an error.
    ///
    /// The returned state is not fed into the provider request today — `drive`'s call site logs
    /// it instead (see that call site's own comment for why widening what the request carries is
    /// out of this item's scope) — but a real caller a future item builds (e.g. a
    /// `SectionKind::Goal` in the context pack) has a typed value to work from rather than needing
    /// to re-derive this same read.
    fn reorient_goal(&self, task: &AgentTask, at_step: u32) -> Result<Option<tm_core::GoalState>> {
        let Some(ticket) = &task.ticket else {
            return Ok(None);
        };
        let Some(state) = self.store.goal_state(ticket)? else {
            return Ok(None);
        };
        self.store
            .reorient_goal(ticket, at_step, self.actor.clone())?;
        Ok(Some(state))
    }

    /// The project-wide event backstop (see [`DEFAULT_MAX_EVENTS_PER_TICKET`]): `Some(detail)`
    /// once `task.ticket`'s total recorded event count has reached [`AgentLoop::
    /// max_events_per_ticket`], naming the exact counts in the returned detail string for
    /// [`AgentOutcome::Failed::detail`]; `None` while there is still room.
    ///
    /// A ticketless chat turn is exempt: the backstop guards runaway accumulation on one ticket
    /// across many resumes, while a chat session legitimately accumulates events for as long as a
    /// human keeps talking, and each of its turns is already bounded by [`AgentLoop::max_steps`].
    fn event_backstop_tripped(&self, task: &AgentTask) -> Result<Option<String>> {
        let Some(ticket) = &task.ticket else {
            return Ok(None);
        };
        let count = self.store.event_count_for(&Id::from(ticket.clone()))?;
        if count >= u64::from(self.max_events_per_ticket) {
            return Ok(Some(format!(
                "event backstop tripped: {count} events already recorded against {ticket} (ceiling {})",
                self.max_events_per_ticket
            )));
        }
        Ok(None)
    }

    /// Drive the step loop starting from `steps` already recorded (empty for a fresh
    /// [`AgentLoop::run`], non-empty when continuing after [`AgentLoop::resume`] processed a
    /// pending call). The conversation sent to the provider is rebuilt from `task` and `steps`
    /// alone, so a suspended run never needs to carry a live message buffer across the
    /// suspension boundary.
    async fn drive(
        &mut self,
        task: &AgentTask,
        mut steps: Vec<StepRecord>,
    ) -> Result<AgentOutcome> {
        let effective_authority = self.authority.intersect(&task.authority);
        let mut effective_budget = self.budget.intersect(&task.budget);
        for step in &steps {
            if effective_budget.try_spend(step.spend).is_err() {
                effective_budget.spent = effective_budget.spent.plus(step.spend);
            }
        }

        let root = self.root();
        let rendered = crate::prompt::render(
            task.ticket.as_ref(),
            &task.context_pack,
            &self.prompt_fragments,
        );

        // Step 0 (`SPEC.md` §29, `docs/audit-2026-09-18-fable.md` B-09): seed the durable goal
        // from the ticket's own `objective` if this ticket has never had one set. Keyed off
        // durable store state (`goal_state(..).is_none()`), not `steps.is_empty()`, so this is
        // correctly a no-op on every `drive` call after the first — including every call reached
        // via `AgentLoop::resume` — rather than re-seeding (and clobbering step progress) on
        // every resumption.
        self.ensure_goal_set(task)?;

        // How many of `steps`' *already-recorded* text-only turns were nudges
        // (`u1-worker-submit-nudge`): a text-only step (no tool calls) could only have ended up
        // in `steps` non-terminally because it was nudged and the run kept going (an un-nudged
        // one returns `AgentOutcome::Failed` right there — see below), so this count is exact
        // even across a suspend/resume boundary, without any extra state of its own.
        let mut nudges_sent: u32 = steps.iter().filter(|s| s.tool_calls.is_empty()).count() as u32;
        let mut exploration_nudge_sent = false;

        loop {
            if steps.len() as u32 >= self.max_steps {
                let detail = step_limit_detail(self.max_steps, &steps);
                return Ok(AgentOutcome::Failed {
                    steps,
                    class: FailureClass::Other,
                    detail,
                });
            }
            if let Some(exhausted) = first_exhausted_dimension(&effective_budget) {
                return self.budget_handoff_outcome(task, steps, exhausted);
            }
            if let Some(detail) = self.event_backstop_tripped(task)? {
                return Ok(AgentOutcome::Failed {
                    steps,
                    class: FailureClass::Other,
                    detail,
                });
            }

            // Re-orientation (`SPEC.md` §29): re-read the current goal state from the store at
            // the start of every step, rather than trusting only this loop's own in-memory
            // `steps` — the mechanism that keeps a long run on target across turns and across a
            // suspend/resume boundary, per that section's "re-orientation is explicit". A
            // `goal.reoriented` event durably records that this happened. Feeding the read-back
            // state into the provider request itself (so the *model* also re-grounds, not just
            // the loop) is future work out of this item's scope — every existing scripted test in
            // this module predicts `AgentLoop::drive`'s request byte-for-byte via
            // `expected_request`/`expected_request_after`, which know nothing about goal state,
            // so widening what `system`/`messages` carry here would need those helpers rebuilt in
            // lockstep rather than being a narrow addition.
            if let Some(goal) = self.reorient_goal(task, steps.len() as u32 + 1)? {
                let done = goal.steps.iter().filter(|s| s.done).count();
                tracing::debug!(
                    step = steps.len() as u32 + 1,
                    goal = %goal.text,
                    steps_done = done,
                    steps_total = goal.steps.len(),
                    claimed_complete = goal.claimed_complete,
                    "reoriented against durable goal state"
                );
            }

            // Rebuilt from `steps` fresh every turn (`SPEC.md` §30.4,
            // `docs/audit-2026-09-18-fable.md` B-08) rather than carried as a live buffer
            // mutated in place: that is what lets a superseded tool result (a re-read path, a
            // re-run query, a read whose file was since edited) drop out of what gets re-sent on
            // *this* turn even when it became stale earlier in the same `drive` call, not just
            // across a suspend/resume boundary.
            let pruning_stats = pruning::working_set(&steps);
            if pruning_stats.bytes_pruned > 0 {
                tracing::debug!(
                    next_step = steps.len() as u32 + 1,
                    bytes_pruned = pruning_stats.bytes_pruned,
                    tokens_pruned = pruning_stats.tokens_pruned,
                    "context working set pruned stale tool results before this turn's request"
                );
            }
            let messages = match &task.conversation {
                None => rebuild_messages(&rendered.task, &steps),
                Some(conversation) => {
                    rebuild_conversation(&rendered.task, Some(conversation), &steps)
                }
            };

            let cumulative_tokens = steps
                .iter()
                .fold(0u64, |total, step| total.saturating_add(step.spend.tokens));

            // Refuse to start an effect this loop cannot afford to finish (`SPEC.md` §31.2,
            // `docs/audit-2026-09-18-fable.md` B-10), rather than issuing the call and
            // discovering the shortfall only after it returns. `MAX_TOKENS_PER_STEP` — the same
            // ceiling sent below as `CompletionRequest::max_tokens` — is the most this step could
            // ever be charged in tokens, so it doubles as a conservative pre-call estimate for
            // that dimension; it does not need to predict the model's actual output length, only
            // bound the worst case. The dollar dimension needs its own estimate, priced per
            // candidate rather than assumed zero (`budget-affordability-menu-in-context-pack`):
            // `estimate_prompt_tokens` bounds this step's input size, and
            // [`Fabric::affordable_candidates`] prices every one of `self.role`'s candidates
            // against it (input tokens times that candidate's input price, plus
            // `MAX_TOKENS_PER_STEP` output tokens times its output price) to see which still fit
            // what's left of the ticket's dollar budget.
            let prompt_tokens = estimate_prompt_tokens(Some(rendered.system.as_str()), &messages);
            let projected_tokens = cumulative_tokens.saturating_add(prompt_tokens);
            let token_limit = effective_budget.tokens;
            let warning_percent = std::env::var("TM_AGENT_TOKEN_WARNING_PERCENT")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| (1..100).contains(value))
                .unwrap_or(80);
            if token_limit != u64::MAX
                && projected_tokens.saturating_mul(100)
                    >= token_limit.saturating_mul(warning_percent)
            {
                tracing::warn!(
                    projected_tokens,
                    token_limit,
                    warning_percent,
                    "attempt is approaching its token budget"
                );
            }
            if token_limit != u64::MAX && projected_tokens > token_limit {
                tracing::warn!(
                    projected_tokens,
                    token_limit,
                    "attempt stopped before the next provider call because its token budget would be exceeded"
                );
                return self.budget_handoff_outcome(task, steps, BudgetDimension::Tokens);
            }
            let remaining_dollars_micros = effective_budget.remaining().dollars_micros;
            let affordable = self.fabric.affordable_candidates(
                self.role,
                remaining_dollars_micros,
                prompt_tokens,
                MAX_TOKENS_PER_STEP,
            );
            let primary = self.fabric.candidates(self.role).into_iter().next();
            let primary_affordable = primary
                .as_ref()
                .is_some_and(|p| affordable.iter().any(|a| &a.model == p));
            // The primary can't afford this step but a cheaper tier in the same role can: route
            // this one step to it instead of handing the ticket off (SPEC.md §31.2's "tier down
            // before refusing"). `affordable` is already in table order, so its first entry is
            // the least-degraded affordable candidate, not merely the cheapest.
            let tier_down = (!primary_affordable)
                .then(|| affordable.first().cloned())
                .flatten();

            let estimated_dollars_micros = match (&tier_down, primary_affordable) {
                (Some(candidate), _) => candidate.estimated_cost_micros.unwrap_or(0),
                (None, true) => affordable
                    .iter()
                    .find(|a| Some(&a.model) == primary.as_ref())
                    .and_then(|a| a.estimated_cost_micros)
                    .unwrap_or(0),
                // No candidate configured for this role affords the step: name the dollar
                // dimension explicitly (a strictly-over-remaining value) so
                // `first_unaffordable_dimension` reports `Dollars` rather than falling through to
                // `Tokens` by default.
                (None, false) => remaining_dollars_micros.saturating_add(1),
            };
            let estimated_cost = Spend {
                tokens: u64::from(MAX_TOKENS_PER_STEP),
                dollars_micros: estimated_dollars_micros,
                wall_seconds: 0,
            };
            if !self.can_afford(&effective_budget, estimated_cost) {
                let exhausted = first_unaffordable_dimension(&effective_budget, estimated_cost)
                    .unwrap_or(BudgetDimension::Tokens);
                return self.budget_handoff_outcome(task, steps, exhausted);
            }

            let mut system = rendered.system.clone();
            if !exploration_nudge_sent {
                if let Some(nudge) = repeated_exploration_nudge(&steps) {
                    system.push_str("\n\n");
                    system.push_str(&nudge);
                    exploration_nudge_sent = true;
                }
            }
            let remaining = self.max_steps.saturating_sub(steps.len() as u32);
            if task.conversation.is_none() && !steps.is_empty() && remaining <= 3 {
                system.push_str("\n\nThis attempt is close to its step limit. Prioritize finishing verification and calling ticket.submit with the available completion evidence now. Do not repeat completed investigation. If submission fails, use the error to make one focused correction before the limit.");
            }
            let request = CompletionRequest {
                system: Some(system),
                messages,
                tools: self.tools.tool_defs_for(&effective_authority),
                max_tokens: MAX_TOKENS_PER_STEP,
                temperature: Some(0.0),
                stop_sequences: Vec::new(),
                stream: false,
                n: 1,
                model: None,
            };

            let call_result = match &tier_down {
                Some(candidate) => {
                    self.fabric
                        .execute_priced_tier_down(self.role, candidate.model.clone(), request)
                        .await
                }
                None => self.execute_with_capacity_wait(request).await,
            };
            let (completion, cost_micros) = match call_result {
                Ok(result) => result,
                Err(e) => {
                    self.record_provider_events(task)?;
                    return Ok(AgentOutcome::Failed {
                        steps,
                        class: FailureClass::ProviderUnavailable,
                        detail: e.to_string(),
                    });
                }
            };
            self.record_provider_events(task)?;

            let Some(candidate) = completion.candidates.into_iter().next() else {
                return Ok(AgentOutcome::Failed {
                    steps,
                    class: FailureClass::Other,
                    detail: "provider returned no candidates".to_string(),
                });
            };

            let usage = completion.usage;
            let step_spend = Spend {
                tokens: u64::from(usage.input_tokens)
                    + u64::from(usage.output_tokens)
                    + u64::from(usage.cache_read_tokens)
                    + u64::from(usage.cache_write_tokens),
                dollars_micros: cost_micros.unwrap_or(0),
                wall_seconds: completion.latency.as_secs(),
            };
            if effective_budget.try_spend(step_spend).is_err() {
                effective_budget.spent = effective_budget.spent.plus(step_spend);
            }
            self.record_usage(task, step_spend, &completion.model)?;

            let assistant_text = {
                let texts: Vec<&str> = candidate
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if texts.is_empty() {
                    None
                } else {
                    Some(texts.join("\n"))
                }
            };

            let tool_uses: Vec<(String, String, serde_json::Value)> = candidate
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            let step_index = steps.len() as u32 + 1;
            let served_by = completion.model.to_string();

            if tool_uses.is_empty() {
                let reply = assistant_text.clone();
                self.push_step(
                    &mut steps,
                    StepRecord {
                        index: step_index,
                        served_by,
                        assistant_text,
                        tool_calls: Vec::new(),
                        spend: step_spend,
                        at: self.clock.now(),
                    },
                );
                if task.conversation.is_some() {
                    return Ok(AgentOutcome::Replied {
                        text: reply.unwrap_or_default(),
                        steps,
                    });
                }
                // A ticketed task (no `conversation`) that ends a turn with plain text and no
                // tool call gets one nudge — a fresh user message asking it to either submit
                // with evidence or keep working — before this loop gives up on the attempt
                // (`u1-worker-submit-nudge`). Bounded by `max_submit_nudges` (default
                // [`DEFAULT_MAX_SUBMIT_NUDGES`]) so a model that just keeps talking still fails
                // the attempt instead of looping forever.
                // Also refuse to nudge into a step that would immediately hit the step limit
                // (`steps` already includes the text-only step just pushed above): otherwise the
                // next loop iteration would report "step limit reached" instead of the more
                // honest "model ended turn without submitting" for a run that was already out of
                // room.
                if nudges_sent < self.max_submit_nudges && (steps.len() as u32) < self.max_steps {
                    nudges_sent += 1;
                    tracing::debug!(
                        step = step_index,
                        nudges_sent,
                        max_submit_nudges = self.max_submit_nudges,
                        "nudged a text-only turn to call ticket.submit instead of failing the attempt"
                    );
                    continue;
                }
                return Ok(AgentOutcome::Failed {
                    steps,
                    class: FailureClass::Other,
                    detail: NO_SUBMIT_DETAIL.to_string(),
                });
            }

            let mut tool_call_records: Vec<ToolCallRecord> = Vec::new();
            let mut tool_call_events: Vec<ToolCallCompletedPayload> = Vec::new();

            for (id, name, input) in &tool_uses {
                if let Ok(action) = self.tools.to_action(name, input) {
                    // `Authority::permits` alone never yields `NeedsApproval` (only `Allow`/
                    // `Deny` — see its own doc comment); `Oversight::review` is the layer that
                    // escalates an `Allow` to `NeedsApproval` per `oversight.toml` (or leaves an
                    // already-terminal decision untouched, never softening a `Deny` — see
                    // `Oversight::review`'s own doc comment). This is the real effect boundary
                    // `docs/audit-2026-09-18-fable.md` M-16 asked for: before any tool call
                    // dispatches, not after.
                    let decision = self
                        .oversight
                        .review(&action, effective_authority.permits(&action));
                    if let Decision::NeedsApproval(reason) = decision {
                        self.store.append(vec![EventDraft::new(
                            self.actor.clone(),
                            task.subject(),
                            Payload::from(ApprovalRequestedPayload {
                                ticket: task.ticket.clone(),
                                requested_of: self.actor.clone(),
                                note: reason.clone(),
                            }),
                        )
                        .with_session(task.session.clone())])?;
                        // Steps completed before suspension only; this in-progress step
                        // (including any calls already dispatched within it) is not
                        // committed, per `AgentOutcome::AwaitingApproval`'s contract.
                        return Ok(AgentOutcome::AwaitingApproval {
                            steps,
                            pending_call: PendingApproval {
                                tool_use_id: id.clone(),
                                tool_name: name.clone(),
                                input: input.clone(),
                                reason,
                                requested_at: self.clock.now(),
                            },
                        });
                    }
                }

                // `goal.claimed_complete` before dispatch, not after (`SPEC.md` §29,
                // `docs/audit-2026-09-18-fable.md` B-09): the loop is about to *attempt* a
                // submission, which is the moment it believes the goal is met — recording the
                // claim here, ahead of `self.tools.dispatch` actually calling through to
                // `Store::submit` (never `Store::verify`; `SPEC.md` §0/§4.3's verification
                // separation means a worker never marks its own work verified), means the claim
                // stands even if the submission attempt itself then fails (e.g. empty evidence),
                // which is honest: a claim is a claim, independent of whether it was accepted.
                if let (TICKET_SUBMIT, Some(ticket)) = (name.as_str(), &task.ticket) {
                    self.store.claim_goal_complete(
                        ticket,
                        submit_summary(input),
                        self.actor.clone(),
                    )?;
                }

                let call = ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                };
                let dispatch_started_at = self.clock.now();
                let resolution = match self.replay_resolution(id, name, input, &root) {
                    Some(Ok(resolution)) => resolution,
                    Some(Err(detail)) => {
                        // Steps completed before this call only — the in-progress step
                        // (including any calls already dispatched within it) is not committed,
                        // matching `AgentOutcome::AwaitingApproval`'s own contract just above.
                        return Ok(AgentOutcome::Failed {
                            steps,
                            class: FailureClass::Other,
                            detail,
                        });
                    }
                    None => {
                        let ctx = CallContext {
                            authority: &effective_authority,
                            ticket: task.ticket.as_ref(),
                            session: &task.session,
                            actor: &self.actor,
                            clock: self.clock.as_ref(),
                            ids: self.ids.as_ref(),
                            root: &root,
                        };
                        self.tools.dispatch(&call, &ctx).await
                    }
                };
                // Millis since `dispatch_started_at`, clamped to 0: the injected `Clock` need not
                // advance (many scripted tests use a fixed clock), and `millis_since` can go
                // negative if it somehow ran backwards — `tool_call.completed`'s `duration_ms` is
                // `u64`, so either case saturates to `0` rather than panicking on the cast.
                let duration_ms = self.clock.now().millis_since(dispatch_started_at).max(0) as u64;
                tool_call_events.push(ToolCallCompletedPayload {
                    ticket: task.ticket.clone(),
                    session: Some(task.session.clone()),
                    tool_name: name.clone(),
                    duration_ms,
                    outcome: match &resolution {
                        ToolOutcome::Completed { .. } => "completed",
                        ToolOutcome::Denied { .. } => "denied",
                        ToolOutcome::Errored { .. } => "error",
                    }
                    .to_string(),
                    result_bytes: outcome_result_bytes(&resolution),
                    truncated: outcome_truncated(&resolution),
                });

                tool_call_records.push(ToolCallRecord {
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    input: input.clone(),
                    resolution: resolution.clone(),
                });

                if let (TICKET_SUBMIT, ToolOutcome::Completed { .. }, Some(ticket)) =
                    (name.as_str(), &resolution, &task.ticket)
                {
                    {
                        self.record_tool_call_events(task, tool_call_events)?;
                        self.push_step(
                            &mut steps,
                            StepRecord {
                                index: step_index,
                                served_by,
                                assistant_text,
                                tool_calls: tool_call_records,
                                spend: step_spend,
                                at: self.clock.now(),
                            },
                        );
                        let evidence = self.build_evidence(task, ticket, &steps, input);
                        return Ok(AgentOutcome::Submitted { evidence, steps });
                    }
                }
            }
            self.record_tool_call_events(task, tool_call_events)?;

            // No live `messages` buffer to append to: the next loop iteration rebuilds the whole
            // conversation from `steps` (now including the step pushed below) via
            // `pruning::working_set`, which is what lets this turn's own tool results become
            // prunable on a later turn within this same `drive` call, not just after a
            // suspend/resume. `candidate.content` itself (the raw assistant turn straight off
            // the wire, unlike the promoted `StepRecord` shape) is intentionally discarded here
            // rather than folded into `steps` — `assistant_text`/`tool_call_records` already
            // carry everything `rebuild_messages` needs to reconstruct an equivalent assistant
            // turn on replay. This was already true for a resumed run before this change (a
            // suspend/resume boundary only ever carried `steps`, never a live `messages`
            // buffer); this only extends the same reconstruction to every turn, not just a
            // resumed one. It is lossless for `tm_provider::ContentBlock`'s current three
            // variants (`Text`/`ToolUse`/`ToolResult` — no `thinking`/`redacted_thinking`
            // variant exists here), collapsing only benign, order-preserving shape: multiple
            // `Text` blocks in one turn join with `\n` into `assistant_text`, and interleaving
            // between `Text` and `ToolUse` blocks is not preserved (all text renders before all
            // tool uses on reconstruction). If a `thinking`-shaped variant is ever added to
            // `ContentBlock`, this reconstruction needs a matching field on `StepRecord`/
            // `ToolCallRecord` before that content can survive a rebuild.
            self.push_step(
                &mut steps,
                StepRecord {
                    index: step_index,
                    served_by,
                    assistant_text,
                    tool_calls: tool_call_records,
                    spend: step_spend,
                    at: self.clock.now(),
                },
            );
            if task.ticket.is_some()
                && repeated_exploration_count(&steps) >= REPEATED_EXPLORATION_LIMIT
            {
                let summary = InvestigationSummary::from_steps(&steps).render();
                let totals = run_totals(&steps);
                return Ok(AgentOutcome::Failed {
                    steps,
                    class: FailureClass::Other,
                    detail: format!(
                        "stopped after {REPEATED_EXPLORATION_LIMIT} repeated inspections without a repository change or submitted evidence. Last useful work: {summary}. No patch or evidence was submitted. Usage: {} tokens, {} tool calls, {} seconds. Continue the saved session with `tm --resume {}` and make one focused change, run the relevant checks, then submit with evidence.",
                        totals.tokens, totals.tool_calls, totals.wall_seconds, task.session
                    ),
                });
            }
            if task.ticket.is_some() && no_progress_steps(&steps) >= NO_PROGRESS_STEP_LIMIT {
                let summary = InvestigationSummary::from_steps(&steps).render();
                let totals = run_totals(&steps);
                return Ok(AgentOutcome::Failed {
                    steps,
                    class: FailureClass::Other,
                    detail: format!(
                        "stopped after {NO_PROGRESS_STEP_LIMIT} steps without a repository change or submitted evidence. Last useful work: {summary}. No patch or evidence was submitted. Usage: {} tokens, {} tool calls, {} seconds. Continue the saved session with `tm --resume {}` and make a focused change, run the relevant checks, then submit with evidence.",
                        totals.tokens, totals.tool_calls, totals.wall_seconds, task.session
                    ),
                });
            }
            if let Some((dependency, command, output, path)) = missing_toolchain_dependency(&steps)
            {
                return Ok(AgentOutcome::Failed {
                    steps,
                    class: FailureClass::Other,
                    detail: format!("Required toolchain dependency is unavailable: {dependency}. Install or configure it, then retry the task. Failed command: {command}. Output: {output}. PATH: {path}"),
                });
            }
        }
    }

    /// Promote everything durable out of `steps` and wrap it into the [`crate::EvidenceBundle`]
    /// an [`AgentOutcome::Submitted`] carries, pulling the human-readable summary out of the
    /// `ticket.submit` call's own arguments.
    fn build_evidence(
        &self,
        task: &AgentTask,
        ticket: &TicketId,
        steps: &[StepRecord],
        submit_input: &serde_json::Value,
    ) -> crate::outcome::EvidenceBundle {
        let mut session = Session::new(
            task.session.clone(),
            ticket.clone(),
            task.harness_epoch,
            self.clock.now(),
        );
        session.transcript = steps.to_vec();
        let promotion = session.promote();
        let summary = submit_summary(submit_input);
        crate::outcome::EvidenceBundle {
            ticket: ticket.clone(),
            artifacts: promotion.artifacts,
            decisions: promotion.decisions,
            summary,
        }
    }
}

/// Count the trailing steps that contain no successful repository or artifact write. Reads,
/// searches, and commands remain useful investigation, but cannot alone justify consuming the
/// entire step budget on a ticketed run.
fn no_progress_steps(steps: &[StepRecord]) -> usize {
    steps
        .iter()
        .rev()
        .take_while(|step| !step_made_progress(step))
        .count()
}

fn step_made_progress(step: &StepRecord) -> bool {
    step.tool_calls.iter().any(|call| match &call.resolution {
        crate::outcome::ToolCallResolution::Completed { result, .. }
            if call.tool_name.starts_with("edit.") =>
        {
            result.get("applied").and_then(serde_json::Value::as_bool) == Some(true)
        }
        crate::outcome::ToolCallResolution::Completed { .. }
            if call.tool_name == "git.commit" || call.tool_name == "artifact.store" =>
        {
            true
        }
        _ => false,
    })
}

/// Pull the human-readable summary out of a `ticket.submit` tool call's own arguments, the same
/// extraction [`AgentLoop::build_evidence`] performs — shared so the `goal.claimed_complete`
/// call sites in [`AgentLoop::drive`]/[`AgentLoop::resume`] record the same text a successful
/// dispatch would also attach as evidence.
fn submit_summary(input: &serde_json::Value) -> String {
    input
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("submitted")
        .to_string()
}

/// Describe a hard step-limit stop without hiding a failed final submission. The complete steps
/// remain attached to `AgentOutcome::Failed`, preserving command output and other review evidence.
fn step_limit_detail(max_steps: u32, steps: &[StepRecord]) -> String {
    let failed_submit = steps.last().and_then(|step| {
        step.tool_calls.iter().find_map(|call| {
            if call.tool_name == TICKET_SUBMIT {
                if let crate::outcome::ToolCallResolution::Errored { detail } = &call.resolution {
                    return Some(detail.as_str());
                }
            }
            None
        })
    });
    if let Some(cause) = failed_submit {
        format!(
            "step limit ({max_steps}) reached after ticket submission failed: {cause}. The work and verification results from this attempt are retained; review the submit error and retry the ticket without repeating completed work. The ticket was not submitted."
        )
    } else {
        format!(
            "step limit ({max_steps}) reached without submitting. Review the retained work and verification results, then retry the ticket and submit it with evidence. The ticket was not submitted."
        )
    }
}

/// The project root threaded through [`tm_types::CallContext::root`] — a `PatchEngine` applies
/// edits beneath it, and any future filesystem-touching capability would use it the same way.
///
/// `AgentLoop`'s fixed constructor signature has no channel to carry a root path in, so this
/// resolves it the only way available without touching the wall clock or randomness: the
/// process's current working directory, which every `tm` invocation already runs from the
/// project root.
fn project_root() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Rebuild the provider-facing conversation from `task_prompt` (the rendered initial user turn)
/// and `steps` alone, so a resumed run never needs a live message buffer carried across a
/// suspension boundary: everything the provider needs to see is already durable in the
/// transcript.
///
/// Renders `pruning::working_set(steps)` rather than `steps` directly (`SPEC.md` §30.4,
/// `docs/audit-2026-09-18-fable.md` B-08): a `Superseded` verdict swaps that call's result for a
/// short `[superseded by step N]` stub instead of dropping it outright, since the wire format
/// requires every `ToolResult` to stay paired with the `ToolUse` that requested it — the full
/// original content is never lost, it just stops being what a *future* turn's request re-sends;
/// it stays durable wherever `steps` itself came from (the event log, per B-07).
///
/// [`AgentLoop::drive`] separately calls `pruning::working_set` again right before this, purely
/// to log `bytes_pruned`/`tokens_pruned` for the turn's trace line — a second call to a pure,
/// small (at most [`DEFAULT_MAX_STEPS`] steps) function, traded deliberately for keeping this a
/// single testable entry point with the exact name/shape `docs/audit-2026-09-18-fable.md` B-08
/// asks for, rather than threading a working set through as an extra parameter.
pub(crate) fn rebuild_messages(task_prompt: &str, steps: &[StepRecord]) -> Vec<Message> {
    rebuild_conversation(task_prompt, None, steps)
}

/// [`rebuild_messages`] generalized over a chat [`Conversation`]. With `None` it is exactly
/// [`rebuild_messages`]: the task prompt, then this run's steps. With `Some`, every earlier turn is
/// replayed verbatim first (its user message, then its steps), then this turn's user message with
/// the freshly compiled context (`task_prompt`) in front of it, then `steps`. Earlier turns are
/// replayed without the context they were sent with: the history stays byte-stable from turn to
/// turn (a cacheable prefix), and the context always matches what was just asked.
pub(crate) fn rebuild_conversation(
    task_prompt: &str,
    conversation: Option<&Conversation>,
    steps: &[StepRecord],
) -> Vec<Message> {
    let mut messages = Vec::new();
    match conversation {
        // A ticketed task (`u1-worker-submit-nudge`): a text-only step anywhere in `steps` can
        // only mean `AgentLoop::drive` nudged it and kept going (a text-only step that wasn't
        // nudged ends the run terminally right there, so it could never end up embedded inside a
        // later turn's rebuilt `steps`) -- so replaying it re-derives the same nudge instead of
        // needing it carried separately. See `push_step_messages`'s own doc comment.
        None => {
            push_user_text(&mut messages, task_prompt.to_string());
            push_step_messages(&mut messages, steps, true);
        }
        Some(conversation) => {
            for turn in &conversation.prior_turns {
                push_user_text(&mut messages, turn.user_message.clone());
                push_step_messages(&mut messages, &turn.steps, false);
            }
            let current = if task_prompt.trim().is_empty() {
                conversation.user_message.clone()
            } else {
                format!("{task_prompt}\n# Message\n{}", conversation.user_message)
            };
            push_user_text(&mut messages, current);
            push_step_messages(&mut messages, steps, false);
        }
    }
    messages
}

/// Append a user text block, folding it into the previous message when that one is already a user
/// message (a prior turn that ended on tool results) so roles keep strictly alternating on the wire.
fn push_user_text(messages: &mut Vec<Message>, text: String) {
    let block = ContentBlock::Text { text };
    if let Some(last) = messages.last_mut() {
        if last.role == MessageRole::User {
            last.content.push(block);
            return;
        }
    }
    messages.push(Message {
        role: MessageRole::User,
        content: vec![block],
    });
}

/// Append the assistant/tool-result message pairs for `steps`, rendered from
/// `pruning::working_set(steps)` (see [`rebuild_messages`]'s doc comment for why).
///
/// `nudge_text_only_steps` is `true` only for a ticketed task's own steps
/// (`rebuild_conversation`'s `None` branch): when set, a text-only step (assistant text, no tool
/// calls) gets [`SUBMIT_NUDGE_TEXT`] appended as a trailing user message right after it, exactly
/// reproducing what `AgentLoop::drive` sent the model on the turn that followed
/// (`u1-worker-submit-nudge`) -- derived fresh from `steps` on every rebuild rather than carried
/// as separate state, so a request built for turn N always matches the request `drive` actually
/// sent for turn N, including across a suspend/resume boundary.
fn push_step_messages(
    messages: &mut Vec<Message>,
    steps: &[StepRecord],
    nudge_text_only_steps: bool,
) {
    let working_set = pruning::working_set(steps);
    for step_ref in &working_set.steps {
        let step = step_ref.step;
        let mut assistant_content = Vec::new();
        if let Some(text) = &step.assistant_text {
            assistant_content.push(ContentBlock::Text { text: text.clone() });
        }
        for tc in &step.tool_calls {
            assistant_content.push(ContentBlock::ToolUse {
                id: tc.tool_use_id.clone(),
                name: tc.tool_name.clone(),
                input: tc.input.clone(),
            });
        }
        if assistant_content.is_empty() {
            // A completion with neither text nor a tool call (e.g. a reasoning-only reply) also
            // takes `AgentLoop::drive`'s "no tool call" nudge branch, with nothing to render as
            // an assistant message here -- but the nudge itself must still be replayed, folded
            // into the preceding user message via `push_user_text` (valid on the wire: two
            // consecutive user turns collapse into one, same as any other back-to-back user
            // text).
            if nudge_text_only_steps {
                push_user_text(messages, SUBMIT_NUDGE_TEXT.to_string());
            }
            continue;
        }
        messages.push(Message {
            role: MessageRole::Assistant,
            content: assistant_content,
        });

        if step.tool_calls.is_empty() {
            if nudge_text_only_steps {
                push_user_text(messages, SUBMIT_NUDGE_TEXT.to_string());
            }
            continue;
        }

        let result_blocks = step
            .tool_calls
            .iter()
            .zip(&step_ref.tool_call_states)
            .map(|(tc, state)| {
                let (text, is_error) = match state {
                    pruning::ToolCallState::Full => {
                        let (text, error) = tool_result_text(&tc.resolution);
                        (compact_tool_result(text), error)
                    }
                    pruning::ToolCallState::Superseded { by_step } => {
                        (format!("[superseded by step {by_step}]"), false)
                    }
                };
                ContentBlock::ToolResult {
                    tool_use_id: tc.tool_use_id.clone(),
                    content: vec![ContentBlock::Text { text }],
                    is_error,
                }
            })
            .collect();
        messages.push(Message {
            role: MessageRole::User,
            content: result_blocks,
        });
    }
}

/// Bound old tool output in provider requests while leaving the durable result untouched.
fn compact_tool_result(text: String) -> String {
    if text.len() <= TOOL_RESULT_COMPACTION_CHARS {
        return text;
    }
    let edge = TOOL_RESULT_COMPACTION_CHARS / 2;
    tracing::info!(
        original_bytes = text.len(),
        retained_bytes = TOOL_RESULT_COMPACTION_CHARS,
        "compacted tool result in request context; full output remains in the recorded transcript"
    );
    format!(
        "{}\n[context compacted: {} bytes omitted; full output retained in the run record]\n{}",
        &text[..(0..=edge)
            .rev()
            .find(|index| text.is_char_boundary(*index))
            .unwrap_or(0)],
        text.len().saturating_sub(TOOL_RESULT_COMPACTION_CHARS),
        &text[(text.len().saturating_sub(edge)..=text.len())
            .find(|index| text.is_char_boundary(*index))
            .unwrap_or(text.len())..]
    )
}

/// Render one [`crate::outcome::ToolCallResolution`] as the text (and error flag) a provider's
/// `ContentBlock::ToolResult` should carry. `pub(crate)` so [`crate::pruning::working_set`] can
/// reuse it to measure how many bytes a superseded result's full text would have cost, without
/// duplicating this match.
pub(crate) fn tool_result_text(resolution: &ToolOutcome) -> (String, bool) {
    match resolution {
        ToolOutcome::Completed { result, .. } => (result.to_string(), false),
        ToolOutcome::Denied { reason } => (format!("denied: {reason}"), true),
        ToolOutcome::Errored { detail } => (format!("error: {detail}"), true),
    }
}

/// Recognize a stalled streak of identical inspection calls from the durable transcript. The
/// same call appearing three times is enough to interrupt the cycle, while any distinct action
/// (including a write, verification, or new inspection) breaks the streak.
fn repeated_exploration_nudge(steps: &[StepRecord]) -> Option<String> {
    let mut counts = BTreeMap::<String, usize>::new();
    for call in steps.iter().flat_map(|step| &step.tool_calls) {
        if is_progress_call(call) {
            counts.clear();
            continue;
        }
        let Some(key) = exploration_target(call) else {
            continue;
        };
        let count = counts.entry(key.clone()).or_default();
        *count += 1;
        if *count >= 3 {
            return Some(format!("You have repeatedly explored the same source or history target without making progress. Already investigated: {key}. Summarize the findings already in the conversation and take one concrete next step: make a focused edit, run relevant verification, or state the specific blocker. Do not reopen or compare the same material unless new evidence makes a targeted reread necessary."));
        }
    }
    None
}

fn repeated_exploration_count(steps: &[StepRecord]) -> usize {
    let mut counts = BTreeMap::<String, usize>::new();
    let mut maximum = 0;
    for call in steps.iter().flat_map(|step| &step.tool_calls) {
        if is_progress_call(call) {
            counts.clear();
            maximum = 0;
            continue;
        }
        let Some(key) = exploration_target(call) else {
            continue;
        };
        let count = counts.entry(key).or_default();
        *count += 1;
        maximum = maximum.max(*count);
    }
    maximum
}

struct RunTotals {
    tokens: u64,
    tool_calls: usize,
    wall_seconds: u64,
}

fn run_totals(steps: &[StepRecord]) -> RunTotals {
    RunTotals {
        tokens: steps
            .iter()
            .fold(0u64, |total, step| total.saturating_add(step.spend.tokens)),
        tool_calls: steps.iter().map(|step| step.tool_calls.len()).sum(),
        wall_seconds: steps.iter().fold(0u64, |total, step| {
            total.saturating_add(step.spend.wall_seconds)
        }),
    }
}

fn is_progress_call(call: &ToolCallRecord) -> bool {
    call.tool_name.starts_with("edit.")
        || matches!(
            call.tool_name.as_str(),
            "git.commit" | "build.run" | "test.run"
        )
}

/// Normalize inspection identity to its target rather than exact arguments: a different line
/// range or historical revision is still reopening the same source/history subject. Distinct
/// range/line inputs remain visible in tool results, so an explicitly targeted reread is allowed.
fn exploration_target(call: &ToolCallRecord) -> Option<String> {
    let name = call.tool_name.as_str();
    if !matches!(
        name,
        "fs.read"
            | "fs.read_range"
            | "history.why"
            | "history.search"
            | "history.deleted"
            | "git.diff"
            | "git.log"
            | "search.exact"
            | "search.semantic"
            | "search.regex"
            | "search.hybrid"
            | "shell.run"
    ) {
        return None;
    }
    let target_key = if matches!(
        name,
        "fs.read" | "fs.read_range" | "history.why" | "git.diff" | "git.log"
    ) {
        "path"
    } else {
        "query"
    };
    let target = call
        .input
        .get(target_key)
        .and_then(serde_json::Value::as_str)
        .or_else(|| call.input.get("path").and_then(serde_json::Value::as_str))
        .or_else(|| call.input.get("query").and_then(serde_json::Value::as_str))
        .or_else(|| {
            call.input
                .get("revision")
                .and_then(serde_json::Value::as_str)
        })?;
    Some(format!("{name} {target}"))
}

/// Return the concrete missing executable when toolchain discovery output says the command could
/// not be found. This is deliberately limited to shell discovery calls, not ordinary source
/// search results that happen to mention an absent dependency.
fn missing_toolchain_dependency(steps: &[StepRecord]) -> Option<(String, String, String, String)> {
    missing_toolchain_dependency_on_path(steps, &std::env::var("PATH").unwrap_or_default())
}

fn missing_toolchain_dependency_on_path(
    steps: &[StepRecord],
    path: &str,
) -> Option<(String, String, String, String)> {
    for call in steps.iter().flat_map(|step| &step.tool_calls) {
        if !matches!(
            call.tool_name.as_str(),
            "shell.run" | "build.run" | "test.run"
        ) {
            continue;
        }
        let command = call
            .input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                call.input
                    .get("argv")
                    .and_then(serde_json::Value::as_array)
                    .map(|argv| {
                        argv.iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            })?;
        let result = match &call.resolution {
            crate::outcome::ToolCallResolution::Completed { result, .. } => result.to_string(),
            crate::outcome::ToolCallResolution::Errored { detail } => detail.clone(),
            crate::outcome::ToolCallResolution::Denied { .. } => continue,
        };
        let executable = command
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .next()?;
        if !matches!(
            executable,
            "go" | "rustc"
                | "cargo"
                | "node"
                | "npm"
                | "pnpm"
                | "python"
                | "python3"
                | "swift"
                | "make"
        ) {
            continue;
        }
        let lower = result.to_ascii_lowercase();
        let says_missing = lower.contains(&format!("{executable}: command not found"))
            || lower.contains(&format!("{executable}: not found"))
            || lower.contains(&format!("'{executable}' is not recognized"))
            || lower.contains(&format!("\"{executable}\" is not recognized"));
        if !says_missing {
            continue;
        }
        let found = std::env::split_paths(path).any(|dir| dir.join(executable).is_file());
        if found {
            continue;
        }
        return Some((executable.to_string(), command, result, path.to_string()));
    }
    None
}

fn outcome_truncated(resolution: &ToolOutcome) -> Option<bool> {
    match resolution {
        ToolOutcome::Completed { result, .. } => Some(
            result
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        ),
        ToolOutcome::Denied { .. } | ToolOutcome::Errored { .. } => Some(false),
    }
}

fn outcome_result_bytes(resolution: &ToolOutcome) -> Option<u64> {
    match resolution {
        ToolOutcome::Completed { result, .. } => Some(
            result
                .get("result_bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_else(|| {
                    serde_json::to_vec(result).map_or(0, |bytes| bytes.len() as u64)
                }),
        ),
        ToolOutcome::Denied { reason } => Some(reason.len() as u64),
        ToolOutcome::Errored { detail } => Some(detail.len() as u64),
    }
}

/// Pure helper: which [`BudgetDimension`] (if any) `budget` has nothing left in.
///
/// Separated from [`AgentLoop::run`] so the "stop cleanly between steps, never mid-edit"
/// contract is unit-testable against plain `Budget` values with no provider, store or clock in
/// the loop at all.
pub fn first_exhausted_dimension(budget: &Budget) -> Option<BudgetDimension> {
    let remaining = budget.remaining();
    if budget.tokens != u64::MAX && remaining.tokens == 0 {
        return Some(BudgetDimension::Tokens);
    }
    if budget.dollars_micros != u64::MAX && remaining.dollars_micros == 0 {
        return Some(BudgetDimension::Dollars);
    }
    if budget.wall_seconds != u64::MAX && remaining.wall_seconds == 0 {
        return Some(BudgetDimension::WallSeconds);
    }
    None
}

/// Pure helper: which [`BudgetDimension`] (if any) `estimated` would overrun in `budget` —
/// proactive, unlike [`first_exhausted_dimension`], which only reports a dimension that has
/// *already* hit zero. `SPEC.md` §31.2's "refuse to start what it cannot finish"
/// (`docs/audit-2026-09-18-fable.md` B-10): a worker should decline an effect it estimates it
/// cannot pay for *before* starting it, not discover the shortfall after the call returns.
///
/// Separated from [`AgentLoop::can_afford`] for the same reason [`first_exhausted_dimension`] is
/// separated from the loop: unit-testable against plain `Budget`/`Spend` values, no provider,
/// store or clock required.
pub fn first_unaffordable_dimension(budget: &Budget, estimated: Spend) -> Option<BudgetDimension> {
    let remaining = budget.remaining();
    if budget.tokens != u64::MAX && estimated.tokens > remaining.tokens {
        return Some(BudgetDimension::Tokens);
    }
    if budget.dollars_micros != u64::MAX && estimated.dollars_micros > remaining.dollars_micros {
        return Some(BudgetDimension::Dollars);
    }
    if budget.wall_seconds != u64::MAX && estimated.wall_seconds > remaining.wall_seconds {
        return Some(BudgetDimension::WallSeconds);
    }
    None
}

/// A conservative, provider-independent estimate of how many input tokens `system` plus
/// `messages` will cost as the next step's prompt, for pricing a candidate *before* the call
/// (`budget-affordability-menu-in-context-pack`, `SPEC.md` §31.2) — never sent to the provider
/// or billed itself; the real charge comes back on `Completion::usage` once the call returns.
/// Sums every text/tool-input/tool-result byte (the same shape
/// [`tm_provider::mock::MockProvider::deterministic_completion`] estimates `Usage` from for a
/// scripted response) and divides by four, rounding up — the prose chars-per-token constant
/// [`tm_context::tokens::estimate_tokens_prose`] uses for this workspace's other token-budgeting
/// estimates, applied here to the whole prompt at once rather than per rendered section.
fn estimate_prompt_tokens(system: Option<&str>, messages: &[Message]) -> u64 {
    fn block_chars(block: &ContentBlock) -> usize {
        match block {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolUse { input, .. } => input.to_string().len(),
            ContentBlock::ToolResult { content, .. } => content.iter().map(block_chars).sum(),
        }
    }
    let system_chars = system.map_or(0, str::len);
    let message_chars: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .map(block_chars)
        .sum();
    ((system_chars + message_chars) as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::ToolCallResolution;
    use tm_types::TicketId;

    #[test]
    fn unlimited_budget_is_never_exhausted() {
        assert_eq!(first_exhausted_dimension(&Budget::unlimited()), None);
    }

    #[test]
    fn zero_budget_reports_tokens_first() {
        assert_eq!(
            first_exhausted_dimension(&Budget::none()),
            Some(BudgetDimension::Tokens)
        );
    }

    #[test]
    fn dollars_exhausted_is_reported_when_tokens_still_have_room() {
        let mut budget = Budget::new(1_000, 0, u64::MAX);
        budget.spent = Spend::tokens(1);
        assert_eq!(
            first_exhausted_dimension(&budget),
            Some(BudgetDimension::Dollars)
        );
    }

    #[test]
    fn wall_seconds_exhausted_is_reported_last() {
        let mut budget = Budget::new(u64::MAX, u64::MAX, 10);
        budget.spent = Spend::seconds(10);
        assert_eq!(
            first_exhausted_dimension(&budget),
            Some(BudgetDimension::WallSeconds)
        );
    }

    #[test]
    fn budget_with_room_left_is_not_exhausted() {
        let budget = Budget::new(1_000, 1_000, 1_000);
        assert_eq!(first_exhausted_dimension(&budget), None);
    }

    // ---- first_unaffordable_dimension / AgentLoop::can_afford (SPEC.md §31.2, B-10) --------

    #[test]
    fn unlimited_budget_can_afford_anything() {
        assert_eq!(
            first_unaffordable_dimension(&Budget::unlimited(), Spend::tokens(1_000_000)),
            None
        );
    }

    #[test]
    fn a_budget_with_plenty_of_room_affords_a_conservative_estimate() {
        let budget = Budget::new(10_000, 10_000, 10_000);
        assert_eq!(
            first_unaffordable_dimension(&budget, Spend::tokens(4_096)),
            None
        );
    }

    #[test]
    fn a_nonzero_remaining_budget_can_still_be_unaffordable() {
        // The whole point of the proactive check: 500 tokens remain (not exhausted — see
        // `first_exhausted_dimension`, which would report `None` here), but the estimated cost
        // of the next step (4096, `MAX_TOKENS_PER_STEP`) would blow straight through it.
        let mut budget = Budget::new(1_000, u64::MAX, u64::MAX);
        budget.spent = Spend::tokens(500);
        assert_eq!(first_exhausted_dimension(&budget), None);
        assert_eq!(
            first_unaffordable_dimension(&budget, Spend::tokens(4_096)),
            Some(BudgetDimension::Tokens)
        );
    }

    #[test]
    fn first_unaffordable_dimension_checks_dollars_and_wall_seconds_too() {
        let mut dollars = Budget::new(u64::MAX, 100, u64::MAX);
        dollars.spent = Spend::dollars_micros(50);
        assert_eq!(
            first_unaffordable_dimension(&dollars, Spend::dollars_micros(51)),
            Some(BudgetDimension::Dollars)
        );

        let mut wall = Budget::new(u64::MAX, u64::MAX, 100);
        wall.spent = Spend::seconds(50);
        assert_eq!(
            first_unaffordable_dimension(&wall, Spend::seconds(51)),
            Some(BudgetDimension::WallSeconds)
        );
    }

    #[test]
    fn prompt_cache_state_starts_empty() {
        let cache = PromptCacheState::new();
        assert_eq!(cache.system_prompt_fingerprint, None);
    }

    #[test]
    fn rebuild_messages_with_no_steps_is_just_the_task_prompt() {
        let messages = rebuild_messages("# Ticket t-1", &[]);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(
            messages[0].content,
            vec![ContentBlock::Text {
                text: "# Ticket t-1".to_string()
            }]
        );
    }

    #[test]
    fn rebuild_messages_replays_a_completed_tool_call_as_a_result_pair() {
        let steps = vec![StepRecord {
            index: 1,
            served_by: "mock/mock".to_string(),
            assistant_text: Some("looking around".to_string()),
            tool_calls: vec![ToolCallRecord {
                tool_use_id: "call-1".to_string(),
                tool_name: "fs.read".to_string(),
                input: serde_json::json!({"path": "a.rs"}),
                resolution: ToolCallResolution::Completed {
                    result: serde_json::json!({"content": "fn main() {}"}),
                    artifact: None,
                },
            }],
            spend: Spend::tokens(10),
            at: tm_types::Timestamp::from_unix_nanos(0),
        }];
        let messages = rebuild_messages("task", &steps);
        // task prompt + assistant turn + tool-result turn.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].role, MessageRole::Assistant);
        assert!(matches!(messages[1].content[0], ContentBlock::Text { .. }));
        assert!(matches!(
            messages[1].content[1],
            ContentBlock::ToolUse { .. }
        ));
        match &messages[2].content[0] {
            ContentBlock::ToolResult { is_error, .. } => assert!(!is_error),
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_text_marks_denials_and_errors_as_errors() {
        let (_, is_error) = tool_result_text(&ToolCallResolution::Denied {
            reason: "no write authority".to_string(),
        });
        assert!(is_error);

        let (_, is_error) = tool_result_text(&ToolCallResolution::Errored {
            detail: "bad input".to_string(),
        });
        assert!(is_error);

        let (_, is_error) = tool_result_text(&ToolCallResolution::Completed {
            result: serde_json::json!({}),
            artifact: None,
        });
        assert!(!is_error);
    }

    #[test]
    fn compacted_tool_result_keeps_evidence_ends_and_reports_omission() {
        let original = format!("{}MIDDLE{}", "a".repeat(5_000), "z".repeat(5_000));
        let compacted = compact_tool_result(original);
        assert!(compacted.starts_with(&"a".repeat(2_000)));
        assert!(compacted.ends_with(&"z".repeat(2_000)));
        assert!(compacted.contains("context compacted"));
        assert!(compacted.len() < 5_000);
    }

    #[test]
    fn rebuild_messages_stubs_a_superseded_read_but_keeps_its_tool_use_block_paired() {
        // `docs/audit-2026-09-18-fable.md` B-08 item 3: a second `fs.read` of the same path
        // supersedes the first, but the wire format still requires every `ToolResult` to pair
        // with the `ToolUse` that requested it -- so the first call's `ToolUse` block must still
        // be present (with its original id/name/input), only its *result* content replaced.
        let steps = vec![
            StepRecord {
                index: 1,
                served_by: "mock/mock".to_string(),
                assistant_text: None,
                tool_calls: vec![ToolCallRecord {
                    tool_use_id: "call-1".to_string(),
                    tool_name: "fs.read".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    resolution: ToolCallResolution::Completed {
                        result: serde_json::json!({"content": "stale content from three turns ago"}),
                        artifact: None,
                    },
                }],
                spend: Spend::tokens(10),
                at: tm_types::Timestamp::from_unix_nanos(0),
            },
            StepRecord {
                index: 2,
                served_by: "mock/mock".to_string(),
                assistant_text: None,
                tool_calls: vec![ToolCallRecord {
                    tool_use_id: "call-2".to_string(),
                    tool_name: "fs.read".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    resolution: ToolCallResolution::Completed {
                        result: serde_json::json!({"content": "fresh content"}),
                        artifact: None,
                    },
                }],
                spend: Spend::tokens(10),
                at: tm_types::Timestamp::from_unix_nanos(0),
            },
        ];

        let messages = rebuild_messages("task", &steps);
        // task prompt, step 1 assistant+result, step 2 assistant+result.
        assert_eq!(messages.len(), 5);

        // Step 1's `ToolUse` block is still present and unchanged (messages[1] = step 1's
        // assistant turn).
        assert!(matches!(
            messages[1].content[0],
            ContentBlock::ToolUse { .. }
        ));
        assert_eq!(
            messages[1].content[0],
            ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "fs.read".to_string(),
                input: serde_json::json!({"path": "a.rs"}),
            }
        );

        // Step 1's `ToolResult` (messages[2]) is stubbed, not the real (stale) content.
        match &messages[2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(!is_error);
                assert_eq!(
                    *content,
                    vec![ContentBlock::Text {
                        text: "[superseded by step 2]".to_string()
                    }]
                );
            }
            other => panic!("expected a tool result, got {other:?}"),
        }

        // Step 2's `ToolResult` (messages[4]) is the real, current content.
        match &messages[4].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(
                    *content,
                    vec![ContentBlock::Text {
                        text: serde_json::json!({"content": "fresh content"}).to_string()
                    }]
                );
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[test]
    fn agent_outcome_steps_and_is_terminal_cover_every_variant() {
        let steps = vec![StepRecord {
            index: 1,
            served_by: "mock/mock".to_string(),
            assistant_text: None,
            tool_calls: Vec::new(),
            spend: Spend::default(),
            at: tm_types::Timestamp::from_unix_nanos(0),
        }];

        let awaiting = AgentOutcome::AwaitingApproval {
            steps: steps.clone(),
            pending_call: PendingApproval {
                tool_use_id: "call-1".to_string(),
                tool_name: "shell.run".to_string(),
                input: serde_json::json!({}),
                reason: "needs approval".to_string(),
                requested_at: tm_types::Timestamp::from_unix_nanos(0),
            },
        };
        assert!(!awaiting.is_terminal());
        assert_eq!(awaiting.steps().len(), 1);

        let failed = AgentOutcome::Failed {
            steps,
            class: FailureClass::Other,
            detail: "boom".to_string(),
        };
        assert!(failed.is_terminal());
    }

    #[test]
    fn agent_outcome_bytes_and_tokens_pruned_reflect_a_superseded_read() {
        // `docs/audit-2026-09-18-fable.md` B-08 item 5: `AgentOutcome` reports how much the
        // working set pruned, computed from `steps` via the exact same pure function
        // `rebuild_messages` renders from (`crate::pruning::working_set`), so the two can never
        // disagree.
        let steps = vec![
            StepRecord {
                index: 1,
                served_by: "mock/mock".to_string(),
                assistant_text: None,
                tool_calls: vec![ToolCallRecord {
                    tool_use_id: "call-1".to_string(),
                    tool_name: "fs.read".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    resolution: ToolCallResolution::Completed {
                        result: serde_json::json!({"content": "x".repeat(500)}),
                        artifact: None,
                    },
                }],
                spend: Spend::default(),
                at: tm_types::Timestamp::from_unix_nanos(0),
            },
            StepRecord {
                index: 2,
                served_by: "mock/mock".to_string(),
                assistant_text: None,
                tool_calls: vec![ToolCallRecord {
                    tool_use_id: "call-2".to_string(),
                    tool_name: "fs.read".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    resolution: ToolCallResolution::Completed {
                        result: serde_json::json!({"content": "fresh"}),
                        artifact: None,
                    },
                }],
                spend: Spend::default(),
                at: tm_types::Timestamp::from_unix_nanos(0),
            },
        ];
        let outcome = AgentOutcome::Failed {
            steps,
            class: FailureClass::Other,
            detail: "stopped for this test".to_string(),
        };

        assert!(
            outcome.bytes_pruned() > 400,
            "expected most of the ~500-byte first read to be pruned, got {}",
            outcome.bytes_pruned()
        );
        assert!(outcome.tokens_pruned() > 0);

        // A run with no superseded results prunes nothing.
        let clean = AgentOutcome::Failed {
            steps: vec![StepRecord {
                index: 1,
                served_by: "mock/mock".to_string(),
                assistant_text: Some("just text".to_string()),
                tool_calls: Vec::new(),
                spend: Spend::default(),
                at: tm_types::Timestamp::from_unix_nanos(0),
            }],
            class: FailureClass::Other,
            detail: "stopped for this test".to_string(),
        };
        assert_eq!(clean.bytes_pruned(), 0);
        assert_eq!(clean.tokens_pruned(), 0);
    }

    // Presence check only: `TicketId` must be constructible in this module's tests without
    // reaching into `tm-core`'s store, since the tool/session/prompt modules this loop depends
    // on are implemented by concurrently-written sibling files.
    #[test]
    fn ticket_id_round_trips_through_display() {
        let id = TicketId::new("T-1").expect("valid ticket id literal");
        assert_eq!(id.to_string(), "T-1");
    }

    /// A minimal, single-tool-call [`StepRecord`] for [`InvestigationSummary`] tests below —
    /// deliberately without a completed dispatch: `InvestigationSummary::from_steps` reads only
    /// `tool_name`/`input`/`assistant_text`, never `resolution`.
    fn step_with_tool_call(tool_name: &str, input: serde_json::Value) -> StepRecord {
        StepRecord {
            index: 1,
            served_by: "mock/mock".to_string(),
            assistant_text: None,
            tool_calls: vec![ToolCallRecord {
                tool_use_id: "call-1".to_string(),
                tool_name: tool_name.to_string(),
                input,
                resolution: crate::outcome::ToolCallResolution::Completed {
                    result: serde_json::json!({}),
                    artifact: None,
                },
            }],
            spend: Spend::default(),
            at: tm_types::Timestamp::from_unix_nanos(0),
        }
    }

    #[test]
    fn investigation_summary_treats_two_different_argv_only_shell_calls_as_not_overlapping() {
        // `shell.run`/`build.run`/`test.run` take an `argv` string array, not a `command`
        // string (`recover-without-repeating-agent-investigation`'s own fix for exactly this:
        // `tool_call_signature`'s original fallback silently skipped `argv` since
        // `serde_json::Value::as_str` returns `None` for an array, collapsing every distinct
        // shell command down to the bare `"shell.run"` signature). Two attempts running
        // genuinely different commands must not be flagged as the same investigation repeated.
        let cargo_test = InvestigationSummary::from_steps(&[step_with_tool_call(
            "shell.run",
            serde_json::json!({"argv": ["cargo", "test", "--workspace"]}),
        )]);
        let ls = InvestigationSummary::from_steps(&[step_with_tool_call(
            "shell.run",
            serde_json::json!({"argv": ["ls"]}),
        )]);
        assert_ne!(
            cargo_test.tool_signatures, ls.tool_signatures,
            "two different argv-only shell calls must not collapse to the same signature"
        );
        assert!(
            !cargo_test.mostly_repeats(&ls),
            "genuinely different shell commands must not be treated as a repeated investigation"
        );
    }

    #[test]
    fn investigation_summary_treats_the_same_argv_only_shell_call_as_a_repeat() {
        let a = InvestigationSummary::from_steps(&[step_with_tool_call(
            "shell.run",
            serde_json::json!({"argv": ["cargo", "test", "--workspace"], "cacheable": true}),
        )]);
        let b = InvestigationSummary::from_steps(&[step_with_tool_call(
            "shell.run",
            serde_json::json!({"argv": ["cargo", "test", "--workspace"], "cacheable": false}),
        )]);
        assert!(
            a.mostly_repeats(&b),
            "the same argv, with an unrelated flag differing, is still the same command repeated"
        );
    }

    #[test]
    fn investigation_summary_mostly_repeats_is_directional_not_a_symmetric_overlap() {
        // `mostly_repeats` compares against the *later* attempt's own total, not the smaller of
        // the two sets: a later attempt that does a lot of genuinely new work, alongside one
        // incidental re-read shared with the prior attempt, must not be flagged just because that
        // one shared read happens to be *all* the prior attempt did.
        let one_read = InvestigationSummary {
            tool_signatures: vec!["fs.read(walk.rs)".to_string()],
            conclusion: None,
            made_changes: false,
        };
        let lots_of_new_work = InvestigationSummary {
            tool_signatures: vec![
                "fs.read(walk.rs)".to_string(),
                "edit.apply_patch(walk.rs)".to_string(),
                "shell.run(cargo test)".to_string(),
            ],
            conclusion: None,
            made_changes: true,
        };
        assert!(
            !lots_of_new_work.mostly_repeats(&one_read),
            "one shared read out of three genuinely different calls must not count as a repeat"
        );
        assert!(
            one_read.mostly_repeats(&lots_of_new_work),
            "but an attempt that does nothing except repeat one call from a larger prior \
             investigation is still fully a repeat of it"
        );
    }

    #[test]
    fn investigation_summary_made_changes_is_true_only_once_a_mutating_tool_is_called() {
        let reads_only = InvestigationSummary::from_steps(&[step_with_tool_call(
            "fs.read",
            serde_json::json!({"path": "walk.rs"}),
        )]);
        assert!(!reads_only.made_changes);

        let with_an_edit = InvestigationSummary::from_steps(&[step_with_tool_call(
            "edit.apply_patch",
            serde_json::json!({"path": "walk.rs"}),
        )]);
        assert!(with_an_edit.made_changes);

        let with_a_commit = InvestigationSummary::from_steps(&[step_with_tool_call(
            "git.commit",
            serde_json::json!({"message": "fix"}),
        )]);
        assert!(with_a_commit.made_changes);
    }

    // -----------------------------------------------------------------------------------------
    // Live-loop tests (`docs/audit-2026-09-18-fable.md` B-07): drive a real `AgentLoop` over a
    // real `tm_core::Store` and a `tm_provider::MockProvider`-backed `Fabric`, then read the raw
    // event log back to assert on what actually landed durably.
    // -----------------------------------------------------------------------------------------

    use tempfile::TempDir;
    use tm_codeintel::CodeIntel;
    use tm_context::command::{CommandCache, CommandExecutor};
    use tm_context::{ContextPack, Section, SectionKind};
    use tm_core::{ExecutorRequirements, RetryPolicy, TicketKind, VerificationPolicy};
    use tm_events::{Event, EventKind, EventLog};
    use tm_provider::{Candidate, Completion, MockProvider, ModelId, RoleTable, StopReason, Usage};
    use tm_types::{CapabilityProvider, FixedClock, SessionId, TestIds};

    use crate::tools::BuiltinCapability;

    struct NoopCommandCache;
    impl CommandCache for NoopCommandCache {
        fn get(&self, _key: &str) -> Result<Option<tm_context::CommandResult>> {
            Ok(None)
        }
        fn put(
            &self,
            _key: &str,
            _argv: &[String],
            _exit_code: i32,
            _started: tm_types::Timestamp,
            _completed: tm_types::Timestamp,
            _stdout: &[u8],
            _stderr: &[u8],
        ) -> Result<tm_context::CommandResult> {
            Err(TmError::Provider("not used in this test".to_string()))
        }
        fn read_artifact(&self, _id: &tm_types::ArtifactId) -> Result<Vec<u8>> {
            Err(TmError::Provider("not used in this test".to_string()))
        }
    }

    struct NoopCommandExecutor;
    impl CommandExecutor for NoopCommandExecutor {
        fn execute(&self, _spec: &tm_context::CommandSpec) -> Result<tm_context::ExecutionOutcome> {
            Err(TmError::Provider("not used in this test".to_string()))
        }
    }

    /// Everything a live-loop test needs: an open `Store` with one seeded ticket, a
    /// `ToolRegistry` over it, and the actor/session identities to build an [`AgentTask`] with.
    struct LiveHarness {
        dir: TempDir,
        store: Arc<Store>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        actor: ParticipantId,
        ticket: tm_types::TicketId,
        session: SessionId,
    }

    impl LiveHarness {
        fn new() -> Self {
            let dir = TempDir::new().expect("tempdir");
            let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
            let ids: Arc<dyn IdSource> = Arc::new(TestIds::new());
            let store = Arc::new(
                tm_core::Store::open_with(dir.path(), clock.clone(), ids.clone())
                    .expect("open store"),
            );
            let actor: ParticipantId = "agent:test/worker".parse().expect("participant");
            let events = store
                .create_ticket(
                    TicketKind::Work,
                    "test objective".to_string(),
                    None,
                    None,
                    Authority::root(),
                    Vec::new(),
                    ExecutorRequirements {
                        role: Role::CoderFast,
                        human_required: false,
                        min_capability: tm_types::Tolerance::Preferred,
                    },
                    Vec::new(),
                    Vec::new(),
                    VerificationPolicy::None,
                    Budget::unlimited(),
                    RetryPolicy {
                        max_attempts: 3,
                        base_delay_seconds: 30,
                        backoff_multiplier: 2.0,
                        max_delay_seconds: 600,
                    },
                    0,
                    actor.clone(),
                )
                .expect("seed ticket");
            let ticket = events
                .iter()
                .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
                .expect("ticket.created payload");
            LiveHarness {
                dir,
                store,
                clock,
                ids,
                actor,
                ticket,
                session: SessionId::new("S-1").expect("session id"),
            }
        }

        fn tools(&self) -> ToolRegistry {
            let ci = Arc::new(CodeIntel::open(self.dir.path()).expect("codeintel"));
            let command_cache: Arc<dyn CommandCache + Send + Sync> = Arc::new(NoopCommandCache);
            let command_executor: Arc<dyn CommandExecutor + Send + Sync> =
                Arc::new(NoopCommandExecutor);
            ToolRegistry::standard(ci, self.store.clone(), command_cache, command_executor)
        }

        fn task(&self) -> AgentTask {
            let body = "do the thing".to_string();
            let section = Section {
                kind: SectionKind::Objective,
                title: "Task".to_string(),
                body: body.clone(),
                tokens: body.len() / 4,
                bytes: body.len(),
                provenance: Vec::new(),
            };
            AgentTask {
                ticket: Some(self.ticket.clone()),
                context_pack: ContextPack {
                    sections: vec![section],
                    tokens: body.len() / 4,
                    bytes: body.len(),
                    provenance: Vec::new(),
                    dropped: Vec::new(),
                },
                authority: Authority::root(),
                budget: Budget::unlimited(),
                harness_epoch: 0,
                session: self.session.clone(),
                conversation: None,
            }
        }

        /// Every event durably appended to this harness's log, oldest first — read through an
        /// independent [`EventLog`] handle, mirroring `crates/tm-cli/src/project.rs`'s
        /// `open_event_log`/`read_all_events` (`Store` itself exposes no read accessor for the
        /// raw log).
        fn all_events(&self) -> Vec<Event> {
            let db_path = self.dir.path().join(".tm").join("project.db");
            let log = EventLog::open_with_clock(&db_path, self.clock.clone()).expect("open log");
            let mut out = Vec::new();
            let mut seq = 1u64;
            loop {
                let batch = log.read_from(seq, 1024).expect("read_from");
                if batch.is_empty() {
                    break;
                }
                seq += batch.len() as u64;
                out.extend(batch);
            }
            out
        }
    }

    /// Build the exact [`CompletionRequest`] `AgentLoop::drive` will issue for `task`'s very
    /// first turn against a freshly built `AgentLoop` over `tools`/`fabric`, so a test can script
    /// [`MockProvider::script_response`]/`script_failure` against a request guaranteed to match
    /// (`MockProvider` keys scripts by an exact hash of the serialized request).
    fn expected_request(loop_: &AgentLoop, task: &AgentTask) -> CompletionRequest {
        expected_request_after(loop_, task, &[])
    }

    /// As [`expected_request`], but for the turn that follows `steps` already having happened —
    /// i.e. the same request [`AgentLoop::drive`]'s loop rebuilds (via `pruning::working_set`)
    /// once `steps` is what it has accumulated so far. Lets a multi-turn test script every turn
    /// of a run by exact request hash, not just the first.
    fn expected_request_after(
        loop_: &AgentLoop,
        task: &AgentTask,
        steps: &[StepRecord],
    ) -> CompletionRequest {
        let effective_authority = loop_.authority.intersect(&task.authority);
        let fragments = PromptFragments {
            system_preamble: String::new(),
            closing_reminder: String::new(),
            extra: BTreeMap::new(),
        };
        let rendered = crate::prompt::render(task.ticket.as_ref(), &task.context_pack, &fragments);
        let messages = rebuild_messages(&rendered.task, steps);
        CompletionRequest {
            system: Some(rendered.system),
            messages,
            tools: loop_.tools.tool_defs_for(&effective_authority),
            max_tokens: MAX_TOKENS_PER_STEP,
            temperature: Some(0.0),
            stop_sequences: Vec::new(),
            stream: false,
            n: 1,
            model: None,
        }
    }

    /// The [`tm_types::Spend`] `AgentLoop::drive` charges for one provider turn, mirroring its
    /// `step_spend` computation exactly so a test can predict a [`StepRecord`]'s `spend` before
    /// the real loop produces it.
    fn step_spend_of(completion: &Completion) -> Spend {
        Spend {
            tokens: u64::from(completion.usage.input_tokens)
                + u64::from(completion.usage.output_tokens)
                + u64::from(completion.usage.cache_read_tokens)
                + u64::from(completion.usage.cache_write_tokens),
            dollars_micros: 0,
            wall_seconds: completion.latency.as_secs(),
        }
    }

    /// A scripted single-tool-call turn: the model calls `tool_name(input)` and nothing else.
    fn tool_call_completion(
        model: ModelId,
        clock: &Arc<dyn Clock>,
        tool_use_id: &str,
        tool_name: &str,
        input: serde_json::Value,
    ) -> Completion {
        Completion {
            model,
            candidates: vec![Candidate {
                content: vec![ContentBlock::ToolUse {
                    id: tool_use_id.to_string(),
                    name: tool_name.to_string(),
                    input,
                }],
                stop_reason: StopReason::ToolUse,
            }],
            usage: Usage {
                input_tokens: 20,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: std::time::Duration::from_millis(0),
            received_at: clock.now(),
        }
    }

    fn text_only_completion(model: ModelId, clock: &Arc<dyn Clock>) -> Completion {
        Completion {
            model,
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: "looked around, not done yet".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: std::time::Duration::from_millis(0),
            received_at: clock.now(),
        }
    }

    fn find_events(events: &[Event], kind: EventKind) -> Vec<&Event> {
        events.iter().filter(|e| e.kind == kind).collect()
    }

    #[tokio::test]
    async fn run_records_usage_recorded_with_the_completions_actual_spend() {
        let h = LiveHarness::new();
        // Priced at $0.042/M input + $0.084/M output tokens (a sub-$1/M model, per
        // `tel-completion-cost-field`), so this test proves a real priced cost reaches
        // `usage.recorded`'s `dollars_micros`, not just that the field exists.
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1, price = { input_micros_per_token = 42, output_micros_per_token = 84 } }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            h.clock.clone(),
        ));
        fabric.register_provider(provider.clone());

        // `.with_max_submit_nudges(0)`: this test is about usage recording on a plain text-only
        // failure, not the `u1-worker-submit-nudge` retry itself (covered by its own tests
        // below) -- disabling the nudge keeps this exactly the single scripted request it always
        // was.
        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        .with_max_submit_nudges(0);
        let task = h.task();
        let request = expected_request(&agent_loop, &task);
        provider.script_response(
            &request,
            text_only_completion(ModelId::new("mock", "m1"), &h.clock),
        );

        let outcome = agent_loop.run(task).await.expect("run completes");
        assert!(
            matches!(outcome, AgentOutcome::Failed { .. }),
            "a text-only reply with no ticket.submit ends the run as Failed, not an infra error: {outcome:?}"
        );

        let events = h.all_events();
        let usage = find_events(&events, EventKind::UsageRecorded);
        assert_eq!(usage.len(), 1, "expected exactly one usage.recorded event");
        let payload = usage[0]
            .payload
            .as_usage_recorded()
            .expect("usage.recorded payload");
        // 100 input + 50 output tokens, per `text_only_completion`'s scripted `Usage`.
        assert_eq!(payload.tokens, 150);
        assert_eq!(payload.ticket, Some(h.ticket.clone()));
        assert_eq!(payload.session, Some(h.session.clone()));
        // 100 * 42 + 50 * 84 = 4200 + 4200 = 8400 micro-dollars -- the completion's actual
        // spend, priced, not the hardcoded 0 this test caught before `tel-completion-cost-field`.
        assert_eq!(payload.dollars_micros, 8400);
        // `tel-usage-payload-model-field`: attributed to the completion's actual served-by
        // model, `mock/m1`, not hardcoded or dropped.
        assert_eq!(payload.provider.as_deref(), Some("mock"));
        assert_eq!(payload.model.as_deref(), Some("m1"));
    }

    #[tokio::test]
    async fn drive_hands_off_cleanly_when_it_cannot_afford_the_next_step() {
        // `SPEC.md` §31.2/§31.3, `docs/audit-2026-09-18-fable.md` B-10: a worker whose remaining
        // budget cannot cover even a conservative estimate of the next call must hand off
        // *before* attempting it, not discover the shortfall mid-flight.
        let h = LiveHarness::new();

        // Put the ticket into a real Leased/Running state first, so the proactive handoff this
        // test exercises has a real lease to release and a real `Trigger::BudgetHandoff`
        // transition to make -- not a no-op against a ticket that was never leased.
        h.store
            .activate(&h.ticket, h.actor.clone())
            .expect("activate");
        h.store
            .acquire_lease(
                &h.ticket,
                h.actor.clone(),
                Authority::root(),
                Vec::new(),
                300,
                h.actor.clone(),
            )
            .expect("acquire lease");
        h.store
            .transition(&h.ticket, tm_core::Trigger::WorkStarted, h.actor.clone())
            .expect("work started");

        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            h.clock.clone(),
        ));
        fabric.register_provider(provider.clone());
        // Deliberately nothing scripted: if the loop ever reaches `fabric.execute` despite the
        // budget shortfall, `MockProvider` errors loudly rather than this test silently passing.

        // Far below `MAX_TOKENS_PER_STEP`, so `can_afford` must refuse before the first step.
        let starved_budget = Budget::new(10, u64::MAX, u64::MAX);

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            starved_budget,
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        );

        let outcome = agent_loop.run(h.task()).await.expect("run completes");
        match outcome {
            AgentOutcome::BudgetExhausted { steps, exhausted } => {
                assert!(
                    steps.is_empty(),
                    "must hand off before any step executes, not mid-flight"
                );
                assert_eq!(exhausted, BudgetDimension::Tokens);
            }
            other => {
                panic!("expected BudgetExhausted via the proactive can_afford check, got {other:?}")
            }
        }

        let ticket = h
            .store
            .view()
            .expect("view")
            .tickets
            .get(&h.ticket)
            .cloned()
            .expect("ticket still exists");
        assert_eq!(
            ticket.state,
            tm_core::TicketState::Ready,
            "the real Store::budget_handoff call must return the ticket to Ready"
        );
        assert_ne!(ticket.state, tm_core::TicketState::Recovery);
        assert!(
            h.store
                .view()
                .expect("view")
                .leases
                .values()
                .all(|l| l.ticket != h.ticket),
            "the lease must be released, not left dangling"
        );
    }

    // ---- budget-affordability-menu-in-context-pack (`SPEC.md` §31.2): dollar-aware tier-down
    // and hand-off ------------------------------------------------------------------------------

    #[tokio::test]
    async fn drive_tiers_down_to_a_cheaper_candidate_the_primary_cannot_afford() {
        // Two priced candidates for the same role: a wildly expensive primary and a genuinely
        // cheap fallback (not merely unpriced/unmetered, so this exercises picking a cheaper
        // *priced* tier, not just falling back to untracked cost). A dollar budget of 1,000
        // micro-dollars cannot afford even the primary's output tokens alone (4,096 tokens at
        // $1,000/M is already 4,096,000 micros), but the cheap fallback's estimate stays under
        // it for any realistic prompt size, so the step must route there instead of handing off.
        let h = LiveHarness::new();
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [\
             { provider = \"expensive\", model = \"big\", max_concurrency = 1, price = { input_micros_per_million_tokens = 1000000000, output_micros_per_million_tokens = 1000000000 } }, \
             { provider = \"cheap\", model = \"small\", max_concurrency = 1, degraded_ok = true, price = { input_micros_per_million_tokens = 1000, output_micros_per_million_tokens = 1000 } }\
             ]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let expensive_provider = Arc::new(MockProvider::new(
            "expensive",
            ModelId::new("expensive", "big"),
            h.clock.clone(),
        ));
        let cheap_provider = Arc::new(MockProvider::new(
            "cheap",
            ModelId::new("cheap", "small"),
            h.clock.clone(),
        ));
        fabric.register_provider(expensive_provider.clone());
        fabric.register_provider(cheap_provider.clone());

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::new(u64::MAX, 1_000, u64::MAX),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        .with_max_steps(1);
        let task = h.task();
        let request = expected_request(&agent_loop, &task);
        cheap_provider.script_response(
            &request,
            text_only_completion(ModelId::new("cheap", "small"), &h.clock),
        );
        // Deliberately nothing scripted on `expensive_provider`: if the loop ever calls it
        // despite the budget shortfall, `MockProvider` errors loudly rather than this test
        // silently passing.

        let outcome = agent_loop.run(task).await.expect("run completes");
        assert!(
            !matches!(outcome, AgentOutcome::BudgetExhausted { .. }),
            "an affordable fallback exists, so this must not hand off: {outcome:?}"
        );
        assert_eq!(
            expensive_provider.call_log().len(),
            0,
            "the unaffordable primary must never be called"
        );
        assert_eq!(
            cheap_provider.call_log().len(),
            1,
            "the affordable fallback must serve the step"
        );

        let events = h.all_events();
        let degraded = find_events(&events, EventKind::ProviderDegraded);
        assert_eq!(degraded.len(), 1);
        let payload = degraded[0]
            .payload
            .as_provider_degraded()
            .expect("provider.degraded payload");
        assert_eq!(payload.provider, "cheap/small");
        assert!(
            payload.reason.contains("budget tier-down"),
            "the degrade reason should name this as a budget tier-down: {}",
            payload.reason
        );
    }

    #[tokio::test]
    async fn drive_hands_off_when_no_candidate_for_the_role_can_afford_the_step() {
        // Both candidates are priced well beyond a near-exhausted dollar budget: unlike the
        // token-only shortfall `drive_hands_off_cleanly_when_it_cannot_afford_the_next_step`
        // covers, this must hand off naming `BudgetDimension::Dollars`, and never call either
        // provider.
        let h = LiveHarness::new();
        h.store
            .activate(&h.ticket, h.actor.clone())
            .expect("activate");
        h.store
            .acquire_lease(
                &h.ticket,
                h.actor.clone(),
                Authority::root(),
                Vec::new(),
                300,
                h.actor.clone(),
            )
            .expect("acquire lease");
        h.store
            .transition(&h.ticket, tm_core::Trigger::WorkStarted, h.actor.clone())
            .expect("work started");

        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [\
             { provider = \"mock\", model = \"m1\", max_concurrency = 1, price = { input_micros_per_million_tokens = 1000000000, output_micros_per_million_tokens = 1000000000 } }, \
             { provider = \"mock\", model = \"m2\", max_concurrency = 1, degraded_ok = true, price = { input_micros_per_million_tokens = 1000000000, output_micros_per_million_tokens = 1000000000 } }\
             ]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            h.clock.clone(),
        ));
        fabric.register_provider(provider.clone());
        // Deliberately nothing scripted: neither candidate can afford this step, so neither
        // should ever be called.

        let starved_dollars = Budget::new(u64::MAX, 1, u64::MAX);
        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            starved_dollars,
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        );

        let outcome = agent_loop.run(h.task()).await.expect("run completes");
        match outcome {
            AgentOutcome::BudgetExhausted { steps, exhausted } => {
                assert!(steps.is_empty(), "must hand off before any step executes");
                assert_eq!(exhausted, BudgetDimension::Dollars);
            }
            other => {
                panic!("expected BudgetExhausted with BudgetDimension::Dollars, got {other:?}")
            }
        }
        assert_eq!(
            provider.call_log().len(),
            0,
            "an unaffordable role must never reach the provider"
        );
    }

    #[tokio::test]
    async fn run_brackets_a_session_started_and_ended_pair() {
        let h = LiveHarness::new();
        let table =
            RoleTable::parse("[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n")
                .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let provider = Arc::new(MockProvider::new(
            "mock",
            ModelId::new("mock", "m1"),
            h.clock.clone(),
        ));
        fabric.register_provider(provider.clone());

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        // Not testing `u1-worker-submit-nudge` here: a text-only reply should end the run as
        // `Failed` directly, so the session-bracket assertions below are about exactly one
        // provider turn.
        .with_max_submit_nudges(0);
        let task = h.task();
        let request = expected_request(&agent_loop, &task);
        provider.script_response(
            &request,
            text_only_completion(ModelId::new("mock", "m1"), &h.clock),
        );

        let outcome = agent_loop.run(task.clone()).await.expect("run completes");
        assert!(
            outcome.is_terminal(),
            "a text-only reply is terminal (Failed)"
        );

        let events = h.all_events();
        let started = find_events(&events, EventKind::SessionStarted);
        let ended = find_events(&events, EventKind::SessionEnded);
        assert_eq!(started.len(), 1, "expected exactly one session.started");
        assert_eq!(ended.len(), 1, "expected exactly one session.ended");
        assert_eq!(
            started[0].payload.as_session_started().unwrap().session,
            task.session
        );
        assert_eq!(
            ended[0].payload.as_session_ended().unwrap().session,
            task.session
        );
        // The bracket is ordered: started strictly before ended.
        assert!(started[0].seq < ended[0].seq);
    }

    /// [`is_capacity_refusal`] matches the fabric's `Wait` refusal by text; this pins that text
    /// against the real fabric, and checks an `Exhausted` refusal is not mistaken for one.
    #[tokio::test]
    async fn capacity_refusal_text_matches_the_fabric() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1, limits = { requests_per_minute = 1 } }]\n",
        )
        .expect("role table parses");
        let fabric = Fabric::new(table, clock.clone());
        let provider = MockProvider::new("mock", ModelId::new("mock", "m1"), clock.clone());
        provider.script_default_response(text_only_completion(ModelId::new("mock", "m1"), &clock));
        fabric.register_provider(Arc::new(provider));
        let request = CompletionRequest {
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 16,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: false,
            n: 1,
            model: None,
        };

        fabric
            .execute(Role::CoderFast, request.clone())
            .await
            .expect("the first request fits the quota");
        let refused = fabric
            .execute(Role::CoderFast, request.clone())
            .await
            .expect_err("the second waits for the next minute");
        assert!(is_capacity_refusal(&refused), "{refused:?}");

        let unroutable = Fabric::new(RoleTable::parse("").expect("empty table"), clock);
        let exhausted = unroutable
            .execute(Role::CoderFast, request)
            .await
            .expect_err("nothing can serve the role");
        assert!(!is_capacity_refusal(&exhausted), "{exhausted:?}");
    }

    #[tokio::test]
    async fn run_records_provider_exhausted_when_no_candidate_can_serve_the_role() {
        let h = LiveHarness::new();
        // No candidates configured for `coder_fast` at all, so `Fabric::route` reports
        // `RouteDecision::Exhausted` and `execute` returns `Err` before ever calling a provider.
        let table = RoleTable::parse("").expect("empty table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        );
        let task = h.task();

        let outcome = agent_loop.run(task.clone()).await.expect("run completes");
        assert!(matches!(
            outcome,
            AgentOutcome::Failed {
                class: FailureClass::ProviderUnavailable,
                ..
            }
        ));

        let events = h.all_events();
        let exhausted = find_events(&events, EventKind::ProviderExhausted);
        assert_eq!(
            exhausted.len(),
            1,
            "expected exactly one provider.exhausted event"
        );
        let payload = exhausted[0]
            .payload
            .as_provider_exhausted()
            .expect("provider.exhausted payload");
        assert_eq!(payload.role, Role::CoderFast.as_str());

        // The run still closed its session even though it never reached the provider.
        let ended = find_events(&events, EventKind::SessionEnded);
        assert_eq!(ended.len(), 1);
    }

    #[tokio::test]
    async fn run_prunes_a_repeated_fs_stat_by_the_third_turn() {
        // `docs/audit-2026-09-18-fable.md` B-08, end to end: a real `AgentLoop::run` across three
        // turns, where turn 3's *scripted* request must already contain a
        // `[superseded by step 2]` stub in place of turn 1's `fs.stat` result. `MockProvider`
        // matches scripts by an exact hash of the serialized request, so if the loop's own
        // per-turn `pruning::working_set` rebuild ever disagreed with what this test predicts,
        // turn 3 would hit an unscripted request and the run would come back `Failed` with a
        // provider error instead of `Submitted` — the assertion at the bottom is a strong one,
        // not just a shape check.
        let h = LiveHarness::new();
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        // Not testing `u1-worker-submit-nudge` here: turn 3's text-only reply should end the run
        // as `Failed` directly, matching this test's own doc comment above about what an
        // unscripted turn 4 would mean.
        .with_max_submit_nudges(0);
        let task = h.task();
        // A path that does not exist under this test process's CWD, so `fs.stat` completes
        // deterministically with `exists: false` regardless of where `cargo test` runs from.
        let stat_input = serde_json::json!({"path": "nonexistent-b08-fixture.rs"});
        let root = project_root();
        let effective_authority = agent_loop.authority().intersect(&task.authority);
        let ctx = CallContext {
            authority: &effective_authority,
            ticket: task.ticket.as_ref(),
            session: &task.session,
            actor: &h.actor,
            clock: h.clock.as_ref(),
            ids: h.ids.as_ref(),
            root: &root,
        };

        // Turn 1: the model stats the path.
        let turn1_request = expected_request(&agent_loop, &task);
        let turn1_completion = tool_call_completion(
            model.clone(),
            &h.clock,
            "call-1",
            "fs.stat",
            stat_input.clone(),
        );
        provider.script_response(&turn1_request, turn1_completion.clone());
        let resolution1 = agent_loop
            .tools()
            .dispatch(
                &ToolCall {
                    id: "call-1".to_string(),
                    name: "fs.stat".to_string(),
                    input: stat_input.clone(),
                },
                &ctx,
            )
            .await;
        assert!(
            matches!(resolution1, ToolOutcome::Completed { .. }),
            "fs.stat always completes, even for a missing path: {resolution1:?}"
        );
        let step1 = StepRecord {
            index: 1,
            served_by: model.to_string(),
            assistant_text: None,
            tool_calls: vec![ToolCallRecord {
                tool_use_id: "call-1".to_string(),
                tool_name: "fs.stat".to_string(),
                input: stat_input.clone(),
                resolution: resolution1,
            }],
            spend: step_spend_of(&turn1_completion),
            at: h.clock.now(),
        };

        // Turn 2: the model stats the exact same path again.
        let turn2_request =
            expected_request_after(&agent_loop, &task, std::slice::from_ref(&step1));
        let turn2_completion = tool_call_completion(
            model.clone(),
            &h.clock,
            "call-2",
            "fs.stat",
            stat_input.clone(),
        );
        provider.script_response(&turn2_request, turn2_completion.clone());
        let resolution2 = agent_loop
            .tools()
            .dispatch(
                &ToolCall {
                    id: "call-2".to_string(),
                    name: "fs.stat".to_string(),
                    input: stat_input.clone(),
                },
                &ctx,
            )
            .await;
        let step2 = StepRecord {
            index: 2,
            served_by: model.to_string(),
            assistant_text: None,
            tool_calls: vec![ToolCallRecord {
                tool_use_id: "call-2".to_string(),
                tool_name: "fs.stat".to_string(),
                input: stat_input.clone(),
                resolution: resolution2,
            }],
            spend: step_spend_of(&turn2_completion),
            at: h.clock.now(),
        };

        // Turn 3's request is the actual assertion: step 1's `fs.stat` result must already be a
        // stub, checked directly on the request before the run ever gets to it.
        let steps_so_far = vec![step1, step2];
        let turn3_request = expected_request_after(&agent_loop, &task, &steps_so_far);
        let step1_result_text = match &turn3_request.messages[2].content[0] {
            ContentBlock::ToolResult { content, .. } => match &content[0] {
                ContentBlock::Text { text } => text.clone(),
                other => panic!("expected a text block, got {other:?}"),
            },
            other => panic!("expected a tool result, got {other:?}"),
        };
        assert_eq!(step1_result_text, "[superseded by step 2]");
        // Turn 2's own result (still the latest for this key) must NOT be stubbed.
        let step2_result_text = match &turn3_request.messages[4].content[0] {
            ContentBlock::ToolResult { content, .. } => match &content[0] {
                ContentBlock::Text { text } => text.clone(),
                other => panic!("expected a text block, got {other:?}"),
            },
            other => panic!("expected a tool result, got {other:?}"),
        };
        assert_ne!(step2_result_text, "[superseded by step 2]");

        // Turn 3: a plain text reply ends the run as `Failed` (no `ticket.submit` called) --
        // `ticket.submit` itself requires a real evidence artifact and a claimed ticket, which is
        // irrelevant setup for what this test checks (pruning across turns), so a text-only
        // ending is the simplest terminal turn available, matching
        // `run_records_usage_recorded_with_the_completions_actual_spend`'s idiom above.
        let turn3_completion = text_only_completion(model.clone(), &h.clock);
        provider.script_response(&turn3_request, turn3_completion);

        let outcome = agent_loop.run(task).await.expect("run completes");
        assert!(
            matches!(outcome, AgentOutcome::Failed { .. }),
            "a text-only turn 3 reply ends the run as Failed, not an infra error \
             (and if this is a provider error instead, an earlier turn's request didn't match \
             what was scripted -- the pruning prediction was wrong): {outcome:?}"
        );
        assert_eq!(
            outcome.steps().len(),
            3,
            "all three turns should be recorded"
        );
        assert!(
            outcome.bytes_pruned() > 0,
            "turn 1's stale fs.stat result should count toward AgentOutcome::bytes_pruned"
        );
    }

    // ---- goal loop (SPEC.md §29, docs/audit-2026-09-18-fable.md B-09) ----

    #[tokio::test]
    async fn drive_sets_the_goal_on_step_0_and_reorients_on_a_later_step() {
        let h = LiveHarness::new();
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        // Not testing `u1-worker-submit-nudge` here: a nudge would reorient a third time before
        // the (deliberately unscripted) turn-3 request errors, breaking this test's exact
        // `at_steps == vec![1, 2]` assertion below.
        .with_max_submit_nudges(0);
        let task = h.task();

        // Before the run: no goal exists yet.
        assert!(h.store.goal_state(&h.ticket).expect("goal_state").is_none());

        let stat_input = serde_json::json!({"path": "nonexistent-b09-fixture.rs"});
        let root = project_root();
        let effective_authority = agent_loop.authority().intersect(&task.authority);
        let ctx = CallContext {
            authority: &effective_authority,
            ticket: task.ticket.as_ref(),
            session: &task.session,
            actor: &h.actor,
            clock: h.clock.as_ref(),
            ids: h.ids.as_ref(),
            root: &root,
        };

        // Turn 1: a harmless read, so the run continues to a second (later) step.
        let turn1_request = expected_request(&agent_loop, &task);
        let turn1_completion = tool_call_completion(
            model.clone(),
            &h.clock,
            "call-1",
            "fs.stat",
            stat_input.clone(),
        );
        provider.script_response(&turn1_request, turn1_completion.clone());
        let resolution1 = agent_loop
            .tools()
            .dispatch(
                &ToolCall {
                    id: "call-1".to_string(),
                    name: "fs.stat".to_string(),
                    input: stat_input.clone(),
                },
                &ctx,
            )
            .await;
        let step1 = StepRecord {
            index: 1,
            served_by: model.to_string(),
            assistant_text: None,
            tool_calls: vec![ToolCallRecord {
                tool_use_id: "call-1".to_string(),
                tool_name: "fs.stat".to_string(),
                input: stat_input.clone(),
                resolution: resolution1,
            }],
            spend: step_spend_of(&turn1_completion),
            at: h.clock.now(),
        };

        // Turn 2 (the "later step"): a plain text reply ends the run as `Failed`.
        let turn2_request =
            expected_request_after(&agent_loop, &task, std::slice::from_ref(&step1));
        provider.script_response(
            &turn2_request,
            text_only_completion(model.clone(), &h.clock),
        );

        let outcome = agent_loop.run(task).await.expect("run completes");
        assert!(
            matches!(outcome, AgentOutcome::Failed { .. }),
            "expected a Failed (text-only turn 2) outcome, not an infra error \
             (an infra/provider error here means an earlier turn's request didn't match what \
             was scripted): {outcome:?}"
        );
        assert_eq!(outcome.steps().len(), 2, "both turns should be recorded");

        // Step 0: the goal was seeded from the ticket's own objective (`LiveHarness::new`'s
        // "test objective"), exactly once.
        let events = h.all_events();
        let set = find_events(&events, EventKind::GoalSet);
        assert_eq!(set.len(), 1, "goal.set must be recorded exactly once");
        assert_eq!(set[0].payload.as_goal_set().unwrap().text, "test objective");

        let state = h
            .store
            .goal_state(&h.ticket)
            .expect("goal_state")
            .expect("goal exists after the run");
        assert_eq!(state.text, "test objective");

        // Re-orientation: every step re-read the goal against durable state, including the
        // later (second) step, each durably recorded via `goal.reoriented`.
        let reoriented = find_events(&events, EventKind::GoalReoriented);
        let at_steps: Vec<u32> = reoriented
            .iter()
            .map(|e| e.payload.as_goal_reoriented().unwrap().at_step)
            .collect();
        assert_eq!(
            at_steps,
            vec![1, 2],
            "the loop must reorient at the start of every step, including the later, second one"
        );
    }

    #[tokio::test]
    async fn drive_terminates_via_the_event_backstop_rather_than_spinning_forever() {
        // A pathological case: the model calls the same harmless read tool forever and never
        // submits. With `max_steps` set far above anything this test could ever reach, only the
        // `max_events_per_ticket` backstop can end this run -- proving the backstop is a real,
        // independent bound, not just a restatement of the step limit.
        let h = LiveHarness::new();
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());
        provider.script_default_response(tool_call_completion(
            model.clone(),
            &h.clock,
            "call",
            "fs.stat",
            serde_json::json!({"path": "nonexistent-backstop-fixture.rs"}),
        ));

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        .with_max_steps(1_000_000)
        .with_max_events_per_ticket(10);

        let outcome = agent_loop.run(h.task()).await.expect("run completes");
        match outcome {
            AgentOutcome::Failed {
                steps,
                class,
                detail,
            } => {
                assert_eq!(class, FailureClass::Other);
                assert!(
                    detail.contains("event backstop tripped"),
                    "detail should name the backstop, got: {detail}"
                );
                assert!(
                    (steps.len() as u32) < 1_000_000,
                    "the run must have stopped long before the step limit ever bound it: {} steps",
                    steps.len()
                );
            }
            other => panic!("expected Failed via the event backstop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn goal_claimed_complete_is_recorded_before_submit_and_never_routes_through_verify() {
        let h = LiveHarness::new();
        // Drive the ticket to `Running`, the state `Store::submit` requires -- the same
        // activate/acquire_lease/WorkStarted sequence `tm-e2e`'s `common::close_ticket` helper
        // uses, reproduced here since that helper lives in a different crate's integration-test
        // module tm-agent's own unit tests cannot reach.
        h.store
            .activate(&h.ticket, h.actor.clone())
            .expect("activate");
        h.store
            .acquire_lease(
                &h.ticket,
                h.actor.clone(),
                Authority::none(),
                vec![],
                60,
                h.actor.clone(),
            )
            .expect("acquire_lease");
        h.store
            .transition(&h.ticket, tm_core::Trigger::WorkStarted, h.actor.clone())
            .expect("work started");

        let evidence_events = h
            .store
            .store_artifact(
                tm_core::ArtifactKind::Patch,
                "text/plain".to_string(),
                b"diff --git a/x b/x\n".to_vec(),
                serde_json::json!({}),
                None,
                h.actor.clone(),
            )
            .expect("store_artifact");
        let evidence_id = evidence_events[0]
            .payload
            .as_artifact_created()
            .expect("artifact.created payload")
            .artifact
            .clone();

        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        );
        let task = h.task();
        let request = expected_request(&agent_loop, &task);
        let submit_input = serde_json::json!({
            "summary": "goal met",
            "evidence": [evidence_id.as_str()],
        });
        provider.script_response(
            &request,
            tool_call_completion(
                model.clone(),
                &h.clock,
                "call-1",
                "ticket.submit",
                submit_input,
            ),
        );

        let outcome = agent_loop.run(task).await.expect("run completes");
        assert!(
            matches!(outcome, AgentOutcome::Submitted { .. }),
            "expected Submitted, got {outcome:?}"
        );

        let events = h.all_events();

        let claimed = find_events(&events, EventKind::GoalClaimedComplete);
        assert_eq!(
            claimed.len(),
            1,
            "goal.claimed_complete must be recorded exactly once"
        );
        assert_eq!(
            claimed[0]
                .payload
                .as_goal_claimed_complete()
                .unwrap()
                .summary,
            "goal met"
        );
        assert!(
            h.store
                .goal_state(&h.ticket)
                .expect("goal_state")
                .expect("goal exists")
                .claimed_complete
        );

        // Routes to `Store::submit`, never `Store::verify` (`SPEC.md` §0/§4.3's verification
        // separation): `ticket.submitted` was recorded, and no `ticket.verified`/
        // `ticket.verification_failed` event exists anywhere in this run's log -- this loop
        // never calls `Store::verify` at all, and this asserts the observable consequence of
        // that from outside the loop's own source.
        assert_eq!(find_events(&events, EventKind::TicketSubmitted).len(), 1);
        assert!(find_events(&events, EventKind::TicketVerified).is_empty());
        assert!(find_events(&events, EventKind::TicketVerificationFailed).is_empty());
    }

    // ---- submit nudge (`u1-worker-submit-nudge`) ----

    /// A ticketed run's turn 1 ends with plain text and no tool call; instead of failing right
    /// there, `AgentLoop::drive` appends [`SUBMIT_NUDGE_TEXT`] as a fresh user message and gives
    /// the model one more turn, which this test scripts as a real `ticket.submit` -- reaching
    /// `AgentOutcome::Submitted` where the pre-nudge behavior would have reached `Failed` on
    /// turn 1 alone. Turn 2's request is predicted by `expected_request_after`, which (via
    /// `rebuild_messages`) already derives the nudge from step 1 being text-only -- exactly what
    /// `AgentLoop::drive` itself rebuilds -- so an unscripted (mismatched) request here would
    /// fail this test with a provider error instead of quietly passing for the wrong reason.
    #[tokio::test]
    async fn drive_nudges_a_text_only_turn_then_reaches_submitted() {
        let h = LiveHarness::new();
        h.store
            .activate(&h.ticket, h.actor.clone())
            .expect("activate");
        h.store
            .acquire_lease(
                &h.ticket,
                h.actor.clone(),
                Authority::none(),
                vec![],
                60,
                h.actor.clone(),
            )
            .expect("acquire_lease");
        h.store
            .transition(&h.ticket, tm_core::Trigger::WorkStarted, h.actor.clone())
            .expect("work started");
        let evidence_events = h
            .store
            .store_artifact(
                tm_core::ArtifactKind::Patch,
                "text/plain".to_string(),
                b"diff --git a/x b/x\n".to_vec(),
                serde_json::json!({}),
                None,
                h.actor.clone(),
            )
            .expect("store_artifact");
        let evidence_id = evidence_events[0]
            .payload
            .as_artifact_created()
            .expect("artifact.created payload")
            .artifact
            .clone();

        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        );
        let task = h.task();

        // Turn 1: plain text, no tool call.
        let turn1_request = expected_request(&agent_loop, &task);
        let turn1_completion = text_only_completion(model.clone(), &h.clock);
        provider.script_response(&turn1_request, turn1_completion.clone());
        let step1 = StepRecord {
            index: 1,
            served_by: model.to_string(),
            assistant_text: Some("looked around, not done yet".to_string()),
            tool_calls: Vec::new(),
            spend: step_spend_of(&turn1_completion),
            at: h.clock.now(),
        };

        // Turn 2: the nudged request (step 1's transcript, with the nudge already derived by
        // `expected_request_after`/`rebuild_messages` since step 1 has no tool calls), scripted
        // with a real `ticket.submit`.
        let turn2_request =
            expected_request_after(&agent_loop, &task, std::slice::from_ref(&step1));
        let submit_input = serde_json::json!({
            "summary": "goal met",
            "evidence": [evidence_id.as_str()],
        });
        provider.script_response(
            &turn2_request,
            tool_call_completion(
                model.clone(),
                &h.clock,
                "call-2",
                "ticket.submit",
                submit_input,
            ),
        );

        let outcome = agent_loop.run(task).await.expect("run completes");
        assert!(
            matches!(outcome, AgentOutcome::Submitted { .. }),
            "a nudged text-only turn 1 followed by a real submit should reach Submitted, \
             not fail on turn 1 alone: {outcome:?}"
        );
        assert_eq!(
            outcome.steps().len(),
            2,
            "both the nudged text-only turn and the submitting turn should be recorded"
        );
    }

    /// Two consecutive text-only turns still fail with the original message: the nudge is a
    /// one-shot second chance (`DEFAULT_MAX_SUBMIT_NUDGES`), not an unbounded retry loop.
    #[tokio::test]
    async fn drive_fails_after_two_consecutive_text_only_turns() {
        let h = LiveHarness::new();
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        );
        let task = h.task();

        let turn1_request = expected_request(&agent_loop, &task);
        let turn1_completion = text_only_completion(model.clone(), &h.clock);
        provider.script_response(&turn1_request, turn1_completion.clone());
        let step1 = StepRecord {
            index: 1,
            served_by: model.to_string(),
            assistant_text: Some("looked around, not done yet".to_string()),
            tool_calls: Vec::new(),
            spend: step_spend_of(&turn1_completion),
            at: h.clock.now(),
        };

        let turn2_request =
            expected_request_after(&agent_loop, &task, std::slice::from_ref(&step1));
        provider.script_response(
            &turn2_request,
            text_only_completion(model.clone(), &h.clock),
        );

        let outcome = agent_loop.run(task).await.expect("run completes");
        match outcome {
            AgentOutcome::Failed {
                steps,
                class,
                detail,
            } => {
                assert_eq!(class, FailureClass::Other);
                assert_eq!(detail, "model ended turn without submitting");
                assert_eq!(
                    steps.len(),
                    2,
                    "both text-only turns (the nudged one and the one after it) should be recorded"
                );
            }
            other => panic!("expected Failed after two consecutive text-only turns: {other:?}"),
        }
    }

    /// The nudge is not just a one-off reply: after it, the model can keep doing real work
    /// (a tool call) before eventually submitting, and the transcript rebuilt for that later
    /// turn must still carry the nudge from step 1 -- proving [`push_step_messages`] re-derives
    /// it from `steps` correctly rather than only handling the immediate-next-turn case the two
    /// tests above cover.
    #[tokio::test]
    async fn drive_nudges_then_keeps_working_before_submitting() {
        let h = LiveHarness::new();
        h.store
            .activate(&h.ticket, h.actor.clone())
            .expect("activate");
        h.store
            .acquire_lease(
                &h.ticket,
                h.actor.clone(),
                Authority::none(),
                vec![],
                60,
                h.actor.clone(),
            )
            .expect("acquire_lease");
        h.store
            .transition(&h.ticket, tm_core::Trigger::WorkStarted, h.actor.clone())
            .expect("work started");
        let evidence_events = h
            .store
            .store_artifact(
                tm_core::ArtifactKind::Patch,
                "text/plain".to_string(),
                b"diff --git a/x b/x\n".to_vec(),
                serde_json::json!({}),
                None,
                h.actor.clone(),
            )
            .expect("store_artifact");
        let evidence_id = evidence_events[0]
            .payload
            .as_artifact_created()
            .expect("artifact.created payload")
            .artifact
            .clone();

        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        );
        let task = h.task();

        // Turn 1: plain text, no tool call -- gets nudged.
        let turn1_request = expected_request(&agent_loop, &task);
        let turn1_completion = text_only_completion(model.clone(), &h.clock);
        provider.script_response(&turn1_request, turn1_completion.clone());
        let step1 = StepRecord {
            index: 1,
            served_by: model.to_string(),
            assistant_text: Some("looked around, not done yet".to_string()),
            tool_calls: Vec::new(),
            spend: step_spend_of(&turn1_completion),
            at: h.clock.now(),
        };

        // Turn 2 (the nudged turn): a real tool call, not a submit -- the model keeps working.
        let stat_input =
            serde_json::json!({"path": "nonexistent-u1-worker-submit-nudge-fixture.rs"});
        let turn2_request =
            expected_request_after(&agent_loop, &task, std::slice::from_ref(&step1));
        let turn2_completion = tool_call_completion(
            model.clone(),
            &h.clock,
            "call-2",
            "fs.stat",
            stat_input.clone(),
        );
        provider.script_response(&turn2_request, turn2_completion.clone());

        let root = project_root();
        let effective_authority = agent_loop.authority().intersect(&task.authority);
        let ctx = CallContext {
            authority: &effective_authority,
            ticket: task.ticket.as_ref(),
            session: &task.session,
            actor: &h.actor,
            clock: h.clock.as_ref(),
            ids: h.ids.as_ref(),
            root: &root,
        };
        let resolution2 = agent_loop
            .tools()
            .dispatch(
                &ToolCall {
                    id: "call-2".to_string(),
                    name: "fs.stat".to_string(),
                    input: stat_input.clone(),
                },
                &ctx,
            )
            .await;
        let step2 = StepRecord {
            index: 2,
            served_by: model.to_string(),
            assistant_text: None,
            tool_calls: vec![ToolCallRecord {
                tool_use_id: "call-2".to_string(),
                tool_name: "fs.stat".to_string(),
                input: stat_input.clone(),
                resolution: resolution2,
            }],
            spend: step_spend_of(&turn2_completion),
            at: h.clock.now(),
        };

        // Turn 3: a real submit. Its request must still carry step 1's nudge -- the assertion
        // this test exists for.
        let turn3_request =
            expected_request_after(&agent_loop, &task, &[step1.clone(), step2.clone()]);
        let submit_input = serde_json::json!({
            "summary": "goal met",
            "evidence": [evidence_id.as_str()],
        });
        provider.script_response(
            &turn3_request,
            tool_call_completion(
                model.clone(),
                &h.clock,
                "call-3",
                "ticket.submit",
                submit_input,
            ),
        );

        let outcome = agent_loop.run(task).await.expect("run completes");
        assert!(
            matches!(outcome, AgentOutcome::Submitted { .. }),
            "nudge -> real tool call -> submit should reach Submitted, not an infra error \
             (an infra error here means turn 3's request didn't match what was scripted -- the \
             nudge wasn't re-derived correctly for a turn beyond the immediate next one): \
             {outcome:?}"
        );
        assert_eq!(
            outcome.steps().len(),
            3,
            "all three turns should be recorded"
        );
    }

    /// `tel-tool-call-event-kind`: a step that dispatches two tool calls -- one that completes
    /// (`fs.stat`, unaffected by `shell.enabled`) and one an authority restriction denies
    /// (`shell.run`, with `shell.enabled = false`) -- appends exactly one `tool_call.completed`
    /// event per call, batched together, each naming the right tool and outcome.
    #[tokio::test]
    async fn drive_records_one_tool_call_completed_event_per_dispatched_call() {
        let h = LiveHarness::new();
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        // `shell.enabled = false` denies `shell.run` at dispatch time while leaving `fs.stat` (a
        // read, unaffected by shell authority) free to complete -- one call of each outcome in
        // the same step.
        let mut authority = Authority::root();
        authority.shell.enabled = false;

        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            authority,
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        // Not testing `u1-worker-submit-nudge` here: turn 2's text-only reply should end the run
        // as `Failed` directly, so the event-count assertions below are about exactly the two
        // dispatched calls from turn 1.
        .with_max_submit_nudges(0);
        let task = h.task();
        let stat_input = serde_json::json!({"path": "nonexistent-tool-call-event-fixture.rs"});
        let shell_input = serde_json::json!({"argv": ["echo", "hi"]});

        let turn1_request = expected_request(&agent_loop, &task);
        let turn1_completion = Completion {
            model: model.clone(),
            candidates: vec![Candidate {
                content: vec![
                    ContentBlock::ToolUse {
                        id: "call-1".to_string(),
                        name: "fs.stat".to_string(),
                        input: stat_input.clone(),
                    },
                    ContentBlock::ToolUse {
                        id: "call-2".to_string(),
                        name: "shell.run".to_string(),
                        input: shell_input.clone(),
                    },
                ],
                stop_reason: StopReason::ToolUse,
            }],
            usage: Usage {
                input_tokens: 20,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: std::time::Duration::from_millis(0),
            received_at: h.clock.now(),
        };
        provider.script_response(&turn1_request, turn1_completion.clone());

        // Dispatch both calls directly (as `AgentLoop::drive` itself will) only to learn their
        // real resolutions, so turn 2's request can be predicted and scripted below -- mirroring
        // `expected_request_after`'s other users in this module. `AgentLoop::run` below redoes
        // both dispatches for real; nothing here is asserted against directly.
        let root = project_root();
        let effective_authority = agent_loop.authority().intersect(&task.authority);
        let ctx = CallContext {
            authority: &effective_authority,
            ticket: task.ticket.as_ref(),
            session: &task.session,
            actor: &h.actor,
            clock: h.clock.as_ref(),
            ids: h.ids.as_ref(),
            root: &root,
        };
        let resolution1 = agent_loop
            .tools()
            .dispatch(
                &ToolCall {
                    id: "call-1".to_string(),
                    name: "fs.stat".to_string(),
                    input: stat_input.clone(),
                },
                &ctx,
            )
            .await;
        assert!(matches!(resolution1, ToolOutcome::Completed { .. }));
        let resolution2 = agent_loop
            .tools()
            .dispatch(
                &ToolCall {
                    id: "call-2".to_string(),
                    name: "shell.run".to_string(),
                    input: shell_input.clone(),
                },
                &ctx,
            )
            .await;
        assert!(matches!(resolution2, ToolOutcome::Denied { .. }));

        let step1 = StepRecord {
            index: 1,
            served_by: model.to_string(),
            assistant_text: None,
            tool_calls: vec![
                ToolCallRecord {
                    tool_use_id: "call-1".to_string(),
                    tool_name: "fs.stat".to_string(),
                    input: stat_input.clone(),
                    resolution: resolution1,
                },
                ToolCallRecord {
                    tool_use_id: "call-2".to_string(),
                    tool_name: "shell.run".to_string(),
                    input: shell_input.clone(),
                    resolution: resolution2,
                },
            ],
            spend: step_spend_of(&turn1_completion),
            at: h.clock.now(),
        };

        // Turn 2: a plain text reply ends the run as `Failed` -- the simplest terminal turn
        // available, matching this module's other multi-turn tests' idiom.
        let turn2_request =
            expected_request_after(&agent_loop, &task, std::slice::from_ref(&step1));
        let turn2_completion = text_only_completion(model.clone(), &h.clock);
        provider.script_response(&turn2_request, turn2_completion);

        let outcome = agent_loop.run(task).await.expect("run completes");
        assert!(
            matches!(outcome, AgentOutcome::Failed { .. }),
            "expected Failed via a text-only turn 2 reply, got {outcome:?}"
        );

        let events = h.all_events();
        let tool_call_events = find_events(&events, EventKind::ToolCallCompleted);
        assert_eq!(
            tool_call_events.len(),
            2,
            "exactly one tool_call.completed event per dispatched call"
        );
        let payload1 = tool_call_events[0]
            .payload
            .as_tool_call_completed()
            .expect("tool_call.completed payload");
        assert_eq!(payload1.tool_name, "fs.stat");
        assert_eq!(payload1.outcome, "completed");
        let payload2 = tool_call_events[1]
            .payload
            .as_tool_call_completed()
            .expect("tool_call.completed payload");
        assert_eq!(payload2.tool_name, "shell.run");
        assert_eq!(payload2.outcome, "denied");
    }

    // -----------------------------------------------------------------------------------------
    // `Oversight` wired at the real effect boundary (`docs/audit-2026-09-18-fable.md` M-16,
    // `docs/decisions/D-009-oversight-policy-wiring.md`): a policy requiring approval for a
    // dispatched action's class actually suspends the run *before* `ToolRegistry::dispatch`
    // executes it, and a run with no such policy (`Oversight::default`, what `load_oversight`
    // returns when a project has no `oversight.toml`) dispatches exactly as it did before this
    // wiring existed.
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn run_suspends_for_approval_when_oversight_requires_it_for_the_dispatched_action() {
        let h = LiveHarness::new();
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        // `Authority::root()` (this harness's task authority) grants `git.commit` outright —
        // `Decision::Allow` — so any suspension below is provably `Oversight::review`'s doing,
        // not an authority denial.
        let oversight = Oversight {
            approval_required: ["git.commit".to_string()].into_iter().collect(),
            spend_over_micros: None,
            approved_for_session: Default::default(),
        };
        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        .with_oversight(oversight);
        let task = h.task();
        let request = expected_request(&agent_loop, &task);
        provider.script_response(
            &request,
            tool_call_completion(
                model.clone(),
                &h.clock,
                "call-1",
                "git.commit",
                serde_json::json!({"message": "should never actually run"}),
            ),
        );

        let outcome = agent_loop.run(task).await.expect("run completes");
        match outcome {
            AgentOutcome::AwaitingApproval { pending_call, .. } => {
                assert_eq!(pending_call.tool_name, "git.commit");
                assert_eq!(pending_call.reason, "git.commit");
            }
            other => panic!("expected AwaitingApproval, got {other:?}"),
        }

        // `approval.requested` is durable (`docs/decisions/D-007-desktop-notifications.md` reads
        // exactly this event kind), and no `command.started`/`effect.journaled` landed — the real
        // `git commit` this call would have shelled out to never ran.
        let events = h.all_events();
        let requested = find_events(&events, EventKind::ApprovalRequested);
        assert_eq!(
            requested.len(),
            1,
            "expected exactly one approval.requested"
        );
        assert_eq!(
            requested[0]
                .payload
                .as_approval_requested()
                .expect("approval.requested payload")
                .note,
            "git.commit"
        );
        assert!(find_events(&events, EventKind::CommandStarted).is_empty());
        assert!(find_events(&events, EventKind::EffectJournaled).is_empty());
    }

    #[tokio::test]
    async fn run_dispatches_normally_with_the_default_oversight_policy_no_oversight_toml() {
        // `crate::executor`/`crates/tm-cli/src/dispatch.rs`'s `load_oversight` returns
        // `Oversight::default()` when a project has no `oversight.toml`, and `AgentLoop::new`
        // defaults to the same policy when `with_oversight` is never called — this exercises
        // that default through a full `run()`, proving it asks nothing and a dispatched action
        // proceeds exactly as it did before `Oversight` had a real caller.
        let h = LiveHarness::new();
        h.store
            .activate(&h.ticket, h.actor.clone())
            .expect("activate");
        h.store
            .acquire_lease(
                &h.ticket,
                h.actor.clone(),
                Authority::none(),
                vec![],
                60,
                h.actor.clone(),
            )
            .expect("acquire_lease");
        h.store
            .transition(&h.ticket, tm_core::Trigger::WorkStarted, h.actor.clone())
            .expect("work started");

        let evidence_events = h
            .store
            .store_artifact(
                tm_core::ArtifactKind::Patch,
                "text/plain".to_string(),
                b"diff --git a/x b/x\n".to_vec(),
                serde_json::json!({}),
                None,
                h.actor.clone(),
            )
            .expect("store_artifact");
        let evidence_id = evidence_events[0]
            .payload
            .as_artifact_created()
            .expect("artifact.created payload")
            .artifact
            .clone();

        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        // No `.with_oversight(..)` call: this is `AgentLoop::new`'s own default, deliberately.
        let mut agent_loop = AgentLoop::new(
            fabric,
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        );
        assert_eq!(agent_loop.oversight(), &Oversight::default());

        let task = h.task();
        let request = expected_request(&agent_loop, &task);
        let submit_input = serde_json::json!({
            "summary": "goal met",
            "evidence": [evidence_id.as_str()],
        });
        provider.script_response(
            &request,
            tool_call_completion(
                model.clone(),
                &h.clock,
                "call-1",
                "ticket.submit",
                submit_input,
            ),
        );

        let outcome = agent_loop.run(task).await.expect("run completes");
        assert!(
            matches!(outcome, AgentOutcome::Submitted { .. }),
            "no oversight.toml means nothing needs approval; expected Submitted, got {outcome:?}"
        );

        let events = h.all_events();
        assert!(
            find_events(&events, EventKind::ApprovalRequested).is_empty(),
            "the default oversight policy must never ask for approval"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Tool replay (`replay-tool-replay-mode-design`,
    // `docs/decisions/D-028-record-replay-harness.md`'s "Tool replay" section): a recorded
    // `ToolCallRecord` served verbatim instead of a real dispatch, and a miss as a hard,
    // distinctly-tagged failure rather than a silent fallback to real execution.
    // -----------------------------------------------------------------------------------------

    /// A [`CapabilityProvider`] that delegates `id`/`tools`/`to_action`/`requires` straight to a
    /// real [`BuiltinCapability`] (so a `MockProvider` turn scripted against it hashes identically
    /// to one scripted against a real registry) but counts every [`CapabilityProvider::invoke`]
    /// instead of ever really executing one — the "test-double ... asserted to receive zero
    /// calls" `replay-tool-replay-mode-design`'s acceptance check asks for.
    struct SpyCapability {
        inner: BuiltinCapability,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl CapabilityProvider for SpyCapability {
        fn id(&self) -> &str {
            self.inner.id()
        }
        fn tools(&self) -> Vec<tm_types::ToolSchema> {
            self.inner.tools()
        }
        fn to_action(&self, tool: &str, input: &serde_json::Value) -> Result<tm_types::Action> {
            self.inner.to_action(tool, input)
        }
        fn requires(&self) -> tm_types::AuthorityRequirement {
            self.inner.requires()
        }
        async fn invoke(
            &self,
            tool: &str,
            _input: serde_json::Value,
            _ctx: &CallContext<'_>,
        ) -> Result<serde_json::Value> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(TmError::invariant(format!(
                "SpyCapability::invoke unexpectedly called for `{tool}`; tool-replay mode must \
                 never dispatch a real tool call"
            )))
        }
    }

    fn spy_tools(h: &LiveHarness, calls: Arc<std::sync::atomic::AtomicUsize>) -> ToolRegistry {
        let ci = Arc::new(CodeIntel::open(h.dir.path()).expect("codeintel"));
        let spy: Arc<dyn CapabilityProvider> = Arc::new(SpyCapability {
            inner: BuiltinCapability::new(
                ci,
                h.store.clone(),
                Arc::new(NoopCommandCache),
                Arc::new(NoopCommandExecutor),
            ),
            calls,
        });
        ToolRegistry::new(vec![spy], h.store.clone())
    }

    #[tokio::test]
    async fn tool_replay_reaches_the_recorded_outcome_without_dispatching_real_tool_calls() {
        let h = LiveHarness::new();
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        // The recorded run: a real registry, a real dispatch.
        let mut recorded_loop = AgentLoop::new(
            fabric.clone(),
            h.tools(),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        // Not testing `u1-worker-submit-nudge` here: turn 2's text-only reply should end the run
        // as `Failed` directly, matching this test's own comment below.
        .with_max_submit_nudges(0);
        let task = h.task();
        let stat_input = serde_json::json!({"path": "nonexistent-tool-replay-fixture.rs"});

        let turn1_request = expected_request(&recorded_loop, &task);
        let turn1_completion = tool_call_completion(
            model.clone(),
            &h.clock,
            "call-1",
            "fs.stat",
            stat_input.clone(),
        );
        provider.script_response(&turn1_request, turn1_completion.clone());

        let root = project_root();
        let effective_authority = recorded_loop.authority().intersect(&task.authority);
        let ctx = CallContext {
            authority: &effective_authority,
            ticket: task.ticket.as_ref(),
            session: &task.session,
            actor: &h.actor,
            clock: h.clock.as_ref(),
            ids: h.ids.as_ref(),
            root: &root,
        };
        let resolution1 = recorded_loop
            .tools()
            .dispatch(
                &ToolCall {
                    id: "call-1".to_string(),
                    name: "fs.stat".to_string(),
                    input: stat_input.clone(),
                },
                &ctx,
            )
            .await;
        assert!(matches!(resolution1, ToolOutcome::Completed { .. }));

        let step1 = StepRecord {
            index: 1,
            served_by: model.to_string(),
            assistant_text: None,
            tool_calls: vec![ToolCallRecord {
                tool_use_id: "call-1".to_string(),
                tool_name: "fs.stat".to_string(),
                input: stat_input.clone(),
                resolution: resolution1,
            }],
            spend: step_spend_of(&turn1_completion),
            at: h.clock.now(),
        };

        let turn2_request =
            expected_request_after(&recorded_loop, &task, std::slice::from_ref(&step1));
        provider.script_response(
            &turn2_request,
            text_only_completion(model.clone(), &h.clock),
        );

        let recorded_outcome = recorded_loop
            .run(task.clone())
            .await
            .expect("recorded run completes");
        assert!(
            matches!(recorded_outcome, AgentOutcome::Failed { .. }),
            "expected Failed via a text-only turn 2 reply, got {recorded_outcome:?}"
        );

        // The replay: a spy registry asserted to receive zero calls, driven by the same fabric
        // (already scripted for both turns above -- `SpyCapability` delegates its schema straight
        // to a real `BuiltinCapability`, so the request hash matches the recorded run's turn for
        // turn, and `MockProvider::script_response` serves any request hashing equal, not just
        // the first).
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut replay_loop = AgentLoop::new(
            fabric,
            spy_tools(&h, calls.clone()),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        .with_max_submit_nudges(0)
        .with_replay_tool_source(ReplayToolSource::from_steps(recorded_outcome.steps()));

        // Belt and suspenders on top of the "same schema hashes the same" reasoning above: script
        // the replay loop's own exact requests too, so a schema difference this test didn't
        // anticipate fails loudly as an unscripted-request `ProviderError` rather than silently
        // passing for the wrong reason.
        provider.script_response(&expected_request(&replay_loop, &task), turn1_completion);
        provider.script_response(
            &expected_request_after(&replay_loop, &task, std::slice::from_ref(&step1)),
            text_only_completion(model.clone(), &h.clock),
        );

        let replayed_outcome = replay_loop.run(task).await.expect("replay run completes");

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "tool replay must never dispatch a real tool call"
        );
        assert_eq!(
            replayed_outcome, recorded_outcome,
            "a tool replay must reach the exact same AgentOutcome as the recorded run"
        );
    }

    #[tokio::test]
    async fn tool_replay_with_no_recorded_match_produces_the_divergence_failure() {
        let h = LiveHarness::new();
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
        let model = ModelId::new("mock", "m1");
        let provider = Arc::new(MockProvider::new("mock", model.clone(), h.clock.clone()));
        fabric.register_provider(provider.clone());

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // An empty replay source: nothing was ever recorded, so the very first tool call the
        // model issues has no recorded resolution to match.
        let mut agent_loop = AgentLoop::new(
            fabric,
            spy_tools(&h, calls.clone()),
            Authority::root(),
            Budget::unlimited(),
            h.clock.clone(),
            h.ids.clone(),
            Role::CoderFast,
            h.actor.clone(),
            h.store.clone(),
        )
        .with_replay_tool_source(ReplayToolSource::from_steps(&[]));

        let task = h.task();
        let stat_input = serde_json::json!({"path": "nonexistent-tool-replay-fixture.rs"});
        let request = expected_request(&agent_loop, &task);
        provider.script_response(
            &request,
            tool_call_completion(model.clone(), &h.clock, "call-1", "fs.stat", stat_input),
        );

        let outcome = agent_loop
            .run(task)
            .await
            .expect("a tool-replay miss is Ok(AgentOutcome::Failed), never an Err");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an unmatched tool replay must never fall through to a real dispatch"
        );
        assert!(
            is_replay_divergence(&outcome),
            "expected the tool-replay divergence failure, got {outcome:?}"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Live Codex ChatGPT-subscription auth-loop proof (D-016, opt-in, real network): drives a
    // real `AgentLoop` turn over a real `Fabric` registered with a real
    // `tm_provider::providers::codex_chatgpt::CodexChatGptProvider`, itself authenticated
    // through a real `tm_auth::CodexSubscriptionOAuth` reading whatever already-logged-in Codex
    // CLI session is present on this machine (`$CODEX_HOME`/`$HOME/.codex/auth.json`) — the full
    // chain the rest of this file only ever exercises against `tm_provider::MockProvider`.
    //
    // Three independent layers keep this out of every ordinary `cargo test`/`mise run verify`
    // run, not just one:
    //   1. `#[cfg(feature = "live-codex-auth")]` on this whole module — the test does not exist
    //      in the compiled binary at all without `--features live-codex-auth` (mirrors
    //      `crates/tm-cli`'s `otel` feature, D-010).
    //   2. `#[ignore]` — the standard Rust escape hatch, so even a feature-enabled
    //      `cargo test -p tm-agent --features live-codex-auth` still skips it without
    //      `-- --ignored`.
    //   3. The `TM_LIVE_CODEX_AUTH=1` runtime env check below — the explicit opt-in this
    //      workspace's task brief for D-016 asked for, so a deliberate
    //      `--features live-codex-auth -- --ignored` invocation still gets a clear skip message
    //      rather than a hard failure on a machine with no real Codex session.
    //
    // See `mise.toml`'s `test:live-codex-auth` task and
    // `docs/decisions/D-016-codex-chatgpt-session-auth-adapter.md`.
    // -----------------------------------------------------------------------------------------
    #[cfg(feature = "live-codex-auth")]
    mod live_codex_auth {
        use super::*;
        use tm_provider::providers::codex_chatgpt::CodexChatGptProvider;

        /// A task whose objective directly asks the real model to close the loop through the
        /// one tool this file's other tests also use to reach `AgentOutcome::Submitted`
        /// (`ticket.submit`), with a summary this test can assert on byte-for-byte — the same
        /// "trivial, cheap prompt with an exact expected string" shape D-016 asked for, just
        /// routed through a real tool call instead of a bare text reply, since `AgentLoop::drive`
        /// only reaches `Submitted` via a tool call (a text-only turn is `Failed`, see `drive`'s
        /// `tool_uses.is_empty()` branch above).
        fn live_task(h: &LiveHarness) -> AgentTask {
            let body = "Call the ticket.submit tool exactly once. Set its summary argument to \
                         exactly the word authloopworks (all lowercase, no punctuation, no other \
                         words). Do not call any other tool first."
                .to_string();
            let section = Section {
                kind: SectionKind::Objective,
                title: "Task".to_string(),
                body: body.clone(),
                tokens: body.len() / 4,
                bytes: body.len(),
                provenance: Vec::new(),
            };
            AgentTask {
                ticket: Some(h.ticket.clone()),
                context_pack: ContextPack {
                    sections: vec![section],
                    tokens: body.len() / 4,
                    bytes: body.len(),
                    provenance: Vec::new(),
                    dropped: Vec::new(),
                },
                authority: Authority::root(),
                budget: Budget::unlimited(),
                harness_epoch: 0,
                session: h.session.clone(),
                conversation: None,
            }
        }

        #[tokio::test]
        #[ignore = "hits the real chatgpt.com/backend-api/codex backend; opt in with \
                    TM_LIVE_CODEX_AUTH=1 and --features live-codex-auth -- --ignored"]
        async fn real_agent_loop_turn_reaches_the_real_codex_backend_and_submits() {
            if std::env::var("TM_LIVE_CODEX_AUTH").ok().as_deref() != Some("1") {
                eprintln!(
                    "skipping real_agent_loop_turn_reaches_the_real_codex_backend_and_submits: \
                     set TM_LIVE_CODEX_AUTH=1 to actually call the real Codex backend"
                );
                return;
            }

            // Whatever model this machine's own real Codex CLI session is configured for
            // (`~/.codex/config.toml`'s `model` field, surfaced by `codex doctor`) — never
            // hardcoded, since a hardcoded model this account cannot reach would fail for a
            // reason unrelated to what this test actually proves.
            let model = std::env::var("TM_LIVE_CODEX_MODEL")
                .unwrap_or_else(|_| "gpt-5.6-terra".to_string());

            let h = LiveHarness::new();
            let table = RoleTable::parse(&format!(
                "[coder_fast]\ncandidates = [{{ provider = \"codex-chatgpt\", model = \"{model}\", max_concurrency = 1 }}]\n"
            ))
            .expect("role table parses");
            let fabric = Arc::new(Fabric::new(table, h.clock.clone()));
            let provider = Arc::new(
                CodexChatGptProvider::from_local_session(model, h.clock.clone()).expect(
                    "a real, already-logged-in Codex CLI session must exist at \
                     $CODEX_HOME/auth.json (or $HOME/.codex/auth.json) to run this test -- run \
                     `codex login` first",
                ),
            );
            fabric.register_provider(provider);

            let mut agent_loop = AgentLoop::new(
                fabric,
                h.tools(),
                Authority::root(),
                Budget::unlimited(),
                h.clock.clone(),
                h.ids.clone(),
                Role::CoderFast,
                h.actor.clone(),
                h.store.clone(),
            );

            let outcome = agent_loop
                .run(live_task(&h))
                .await
                .expect("no infrastructure failure driving the loop");

            match outcome {
                AgentOutcome::Submitted { evidence, .. } => {
                    eprintln!(
                        "live codex auth loop proved end-to-end; submitted summary: {:?}",
                        evidence.summary
                    );
                    assert!(
                        evidence.summary.to_lowercase().contains("authloopworks"),
                        "expected the real model's submitted summary to contain the exact \
                         word \"authloopworks\", got: {:?}",
                        evidence.summary
                    );
                }
                other => panic!(
                    "expected a real model turn to reach AgentOutcome::Submitted, got: {other:?}"
                ),
            }
        }
    }
}

#[cfg(test)]
mod step_limit_detail_tests {
    use super::*;
    use crate::outcome::{ToolCallRecord, ToolCallResolution};

    #[test]
    fn final_submit_failure_names_the_cause_and_keeps_prior_evidence_in_the_outcome_steps() {
        let evidence_step = StepRecord {
            index: 1,
            served_by: "mock/test".to_string(),
            assistant_text: None,
            tool_calls: vec![ToolCallRecord {
                tool_use_id: "test".to_string(),
                tool_name: "command.run".to_string(),
                input: serde_json::json!({"command": "cargo test"}),
                resolution: ToolCallResolution::Completed {
                    result: serde_json::json!({"stdout": "all tests passed"}),
                    artifact: None,
                },
            }],
            spend: Spend::default(),
            at: tm_types::Timestamp::from_unix_nanos(0),
        };
        let submit_step = StepRecord {
            index: 2,
            served_by: "mock/test".to_string(),
            assistant_text: None,
            tool_calls: vec![ToolCallRecord {
                tool_use_id: "submit".to_string(),
                tool_name: TICKET_SUBMIT.to_string(),
                input: serde_json::json!({"summary": "done"}),
                resolution: ToolCallResolution::Errored {
                    detail: "submission requires evidence".to_string(),
                },
            }],
            spend: Spend::default(),
            at: tm_types::Timestamp::from_unix_nanos(0),
        };
        let steps = vec![evidence_step, submit_step];

        let detail = step_limit_detail(2, &steps);

        assert!(detail.contains("step limit (2)"), "{detail}");
        assert!(detail.contains("submission requires evidence"), "{detail}");
        assert!(detail.contains("ticket was not submitted"), "{detail}");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].tool_calls[0].tool_name, "command.run");
        assert!(matches!(
            &steps[0].tool_calls[0].resolution,
            ToolCallResolution::Completed { result, .. }
                if result["stdout"] == "all tests passed"
        ));
    }
}

#[cfg(test)]
mod unproductive_exploration_tests {
    use super::*;
    use crate::outcome::{ToolCallRecord, ToolCallResolution};

    fn call(name: &str, input: serde_json::Value, result: serde_json::Value) -> ToolCallRecord {
        ToolCallRecord {
            tool_use_id: "call".to_string(),
            tool_name: name.to_string(),
            input,
            resolution: ToolCallResolution::Completed {
                result,
                artifact: None,
            },
        }
    }

    fn step(calls: Vec<ToolCallRecord>) -> StepRecord {
        StepRecord {
            index: 1,
            served_by: "mock/test".to_string(),
            assistant_text: None,
            tool_calls: calls,
            spend: Spend::default(),
            at: tm_types::Timestamp::from_unix_nanos(0),
        }
    }

    #[test]
    fn repeated_identical_inspections_produce_goal_focused_nudge() {
        let repeated = || {
            call(
                "fs.read",
                serde_json::json!({"path":"src/main.rs"}),
                serde_json::json!({"text":"found target"}),
            )
        };
        let nudge = repeated_exploration_nudge(&[step(vec![repeated(), repeated(), repeated()])])
            .expect("third repeat should be recognized");
        assert!(nudge.contains("src/main.rs"));
        assert!(nudge.contains("concrete next step"));
    }

    #[test]
    fn distinct_progress_resets_inspection_streak() {
        let read = || {
            call(
                "fs.read",
                serde_json::json!({"path":"src/main.rs"}),
                serde_json::json!({}),
            )
        };
        let edit = call(
            "edit.apply_patch",
            serde_json::json!({"path":"src/main.rs"}),
            serde_json::json!({}),
        );
        assert!(
            repeated_exploration_nudge(&[step(vec![read(), read(), edit, read(), read()])])
                .is_none()
        );
    }

    #[test]
    fn repeated_source_and_history_targets_trigger_nudge_despite_changed_ranges_or_revisions() {
        let read = |range| {
            call(
                "fs.read_range",
                serde_json::json!({"path":"src/requests.py", "start":range}),
                serde_json::json!({"text":"existing findings"}),
            )
        };
        let diff = |revision| {
            call(
                "git.diff",
                serde_json::json!({"path":"src/requests.py", "revision":revision}),
                serde_json::json!({"diff":"existing history"}),
            )
        };
        let nudge = repeated_exploration_nudge(&[step(vec![
            read(1),
            diff("HEAD~1"),
            read(20),
            diff("HEAD~2"),
            read(40),
        ])])
        .expect("same source target should be bounded despite changed ranges/revisions");
        assert!(nudge.contains("Summarize the findings"));
        assert!(nudge.contains("src/requests.py"));
    }

    #[test]
    fn focused_reread_of_another_target_remains_available() {
        let call_for = |path: &str| {
            call(
                "fs.read",
                serde_json::json!({"path":path}),
                serde_json::json!({"text":"new evidence"}),
            )
        };
        assert!(repeated_exploration_nudge(&[step(vec![
            call_for("src/requests.py"),
            call_for("tests/test_requests.py"),
            call_for("src/requests.py"),
        ])])
        .is_none());
    }

    #[test]
    fn missing_toolchain_command_names_dependency() {
        let command = call(
            "shell.run",
            serde_json::json!({"command":"go test ./..."}),
            serde_json::json!({"stderr":"go: command not found"}),
        );
        assert_eq!(
            missing_toolchain_dependency_on_path(&[step(vec![command])], "")
                .map(|(dep, _, _, _)| dep),
            Some("go".to_string())
        );
    }

    #[test]
    fn unrelated_cargo_failure_is_not_classified_as_missing_cargo() {
        let command = call(
            "shell.run",
            serde_json::json!({"command":"cargo build"}),
            serde_json::json!({"exit_code":101,"stderr":"error: package `missing-crate` not found"}),
        );
        assert!(missing_toolchain_dependency_on_path(&[step(vec![command])], "/usr/bin").is_none());
    }

    #[test]
    fn cargo_not_found_detail_retains_command_output_and_path() {
        let command = call(
            "shell.run",
            serde_json::json!({"command":"cargo build"}),
            serde_json::json!({"stderr":"sh: cargo: command not found"}),
        );
        let (dependency, failed_command, output, path) =
            missing_toolchain_dependency_on_path(&[step(vec![command])], "").unwrap();
        assert_eq!(dependency, "cargo");
        assert_eq!(failed_command, "cargo build");
        assert!(output.contains("command not found"));
        assert!(path.is_empty());
    }
}

#[cfg(test)]
mod no_progress_tests {
    use super::*;
    use crate::outcome::{ToolCallRecord, ToolCallResolution};

    fn step(index: u32, name: &str, path: &str) -> StepRecord {
        StepRecord {
            index,
            served_by: "mock/test".to_string(),
            assistant_text: Some("Found the relevant implementation".to_string()),
            tool_calls: vec![ToolCallRecord {
                tool_use_id: format!("call-{index}"),
                tool_name: name.to_string(),
                input: serde_json::json!({"path": path}),
                resolution: ToolCallResolution::Completed {
                    result: if name.starts_with("edit.") {
                        serde_json::json!({"applied": true})
                    } else {
                        serde_json::json!({"text": "useful finding"})
                    },
                    artifact: None,
                },
            }],
            spend: Spend::default(),
            at: tm_types::Timestamp::from_unix_nanos(0),
        }
    }

    #[test]
    fn repeated_investigation_hits_bounded_threshold_and_keeps_actionable_summary() {
        let steps: Vec<_> = (1..=NO_PROGRESS_STEP_LIMIT as u32)
            .map(|i| step(i, "fs.read", "crates/tm-agent/src/agent_loop.rs"))
            .collect();
        assert_eq!(no_progress_steps(&steps), NO_PROGRESS_STEP_LIMIT);
        let summary = InvestigationSummary::from_steps(&steps).render();
        assert!(summary.contains("crates/tm-agent/src/agent_loop.rs"));
        assert!(summary.contains("Found the relevant implementation"));
    }

    #[test]
    fn successful_edit_resets_no_progress_streak() {
        let mut steps = vec![step(1, "fs.read", "src/lib.rs")];
        steps.push(step(2, "edit.apply_patch", "src/lib.rs"));
        steps.extend((3..=5).map(|i| step(i, "shell.run", "cargo test")));
        assert_eq!(no_progress_steps(&steps), 3);
    }

    #[test]
    fn edit_conflict_does_not_count_as_progress() {
        let mut edit = step(1, "edit.write_file", "src/lib.rs");
        edit.tool_calls[0].resolution = ToolCallResolution::Completed {
            result: serde_json::json!({"applied": false}),
            artifact: None,
        };
        assert_eq!(no_progress_steps(&[edit]), 1);
    }

    #[test]
    fn repeated_source_inspections_have_an_early_run_level_bound() {
        let steps: Vec<_> = (1..=REPEATED_EXPLORATION_LIMIT as u32)
            .map(|i| step(i, "fs.read", "src/repeated.rs"))
            .collect();
        assert_eq!(
            repeated_exploration_count(&steps),
            REPEATED_EXPLORATION_LIMIT
        );
        let totals = run_totals(&steps);
        assert_eq!(totals.tool_calls, REPEATED_EXPLORATION_LIMIT);
    }

    #[test]
    fn productive_action_resets_the_repeated_inspection_bound() {
        let mut steps = vec![step(1, "fs.read", "src/repeated.rs")];
        steps.push(step(2, "edit.apply_patch", "src/repeated.rs"));
        steps.extend(
            (3..=REPEATED_EXPLORATION_LIMIT as u32 + 2)
                .map(|i| step(i, "fs.read", "src/repeated.rs")),
        );
        assert_eq!(
            repeated_exploration_count(&steps),
            REPEATED_EXPLORATION_LIMIT
        );
    }
}
