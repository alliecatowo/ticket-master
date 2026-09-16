//! [`AgentLoop::run`]: the step loop over the provider fabric, tool dispatch, denial handling,
//! approval suspension/resume, budget enforcement between steps, step limits, and clean
//! termination into an [`AgentOutcome`].
//!
//! The loop is deterministic under `tm_provider::MockProvider`: every provider call goes
//! through the injected [`tm_provider::fabric::Fabric`], every tool call through
//! [`crate::tools::ToolRegistry::dispatch`], and every timestamp/id through the injected
//! [`tm_types::Clock`]/[`tm_types::IdSource`], so replaying the same scripted provider against
//! the same task produces byte-identical [`AgentOutcome`]s.

use std::sync::Arc;

use tm_codeintel::CodeIntel;
use tm_context::command::{CommandCache, CommandExecutor};
use tm_core::Store;
use tm_provider::fabric::Fabric;
use tm_types::{Authority, Budget, Clock, IdSource, ParticipantId, Result, Role};

use crate::outcome::{AgentOutcome, AgentTask, BudgetDimension};
use crate::patch::PatchEngine;
use crate::tools::ToolRegistry;

/// Upper bound on steps a single [`AgentLoop::run`] call will take before it forces a
/// [`AgentOutcome::Failed`] with `tm_core::FailureClass::Other`, guarding against a
/// non-terminating tool-call loop even when budget alone hasn't tripped yet.
pub const DEFAULT_MAX_STEPS: u32 = 64;

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

/// The tool-using agent loop.
pub struct AgentLoop {
    fabric: Arc<Fabric>,
    tools: ToolRegistry,
    authority: Authority,
    budget: Budget,
    cache: PromptCacheState,
    ci: Arc<CodeIntel>,
    store: Arc<Store>,
    command_cache: Arc<dyn CommandCache>,
    command_executor: Arc<dyn CommandExecutor>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    role: Role,
    actor: ParticipantId,
    max_steps: u32,
}

impl AgentLoop {
    /// Build a loop over `fabric`, offering the standard [`ToolRegistry`], gated by `authority`
    /// and capped at `budget` (this is the loop's own ceiling; [`AgentTask::budget`] is
    /// intersected with it per run so neither can override the other upward).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fabric: Arc<Fabric>,
        tools: ToolRegistry,
        authority: Authority,
        budget: Budget,
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache>,
        command_executor: Arc<dyn CommandExecutor>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        role: Role,
        actor: ParticipantId,
    ) -> Self {
        AgentLoop {
            fabric,
            tools,
            authority,
            budget,
            cache: PromptCacheState::new(),
            ci,
            store,
            command_cache,
            command_executor,
            clock,
            ids,
            role,
            actor,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    /// Override the default step limit (mainly for tests that want to force
    /// [`AgentOutcome::Failed`] quickly).
    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// Run `task` to completion (or suspension), driving the provider fabric and tool registry.
    ///
    /// # Errors
    /// Returns `Err` only for an infrastructure failure that makes it impossible to produce any
    /// `AgentOutcome` at all (e.g. the harness epoch's prompt fragments can't be resolved).
    /// Every in-band failure — a denied tool, a budget exhaustion, a provider error, a step
    /// limit — is represented as an `Ok(AgentOutcome::Failed { .. })` or the matching variant,
    /// never as an `Err`.
    // IMPL: (1) intersect `self.budget` with `task.budget` (`Budget` has no direct intersect in
    // tm_types beyond `Budget::intersect` on `Authority`-adjacent scopes — use the smaller of
    // each component) to get this run's effective ceiling; track spend locally starting from
    // zero. (2) render the initial system/task prompt via `crate::prompt::render` from the
    // task's compiled context pack and the harness prompt fragments (obtained by the caller and
    // passed in, or resolved via a harness epoch lookup the caller already pinned — this loop
    // does not re-resolve the epoch itself, it trusts `task.harness_epoch` was already used to
    // build whatever `PromptFragments` it was constructed with). (3) build the first
    // `tm_provider::CompletionRequest` with `tools: self.tools.tool_defs()`. (4) loop up to
    // `self.max_steps`: before each `self.fabric.execute(self.role, request)` call, check the
    // effective budget's `remaining()`/`is_exhausted()`; if exhausted, return
    // `AgentOutcome::BudgetExhausted` with the transcript so far and the first exhausted
    // `BudgetDimension`. (5) call `fabric.execute`; a `Err` from the fabric (provider
    // exhausted/unavailable) becomes `AgentOutcome::Failed` with
    // `tm_core::FailureClass::ProviderUnavailable`. (6) record the step's `Usage` as spend via
    // `Budget::try_spend`, appending a `StepRecord`. (7) for each `ContentBlock::ToolUse` in the
    // completion, build a `crate::tools::ToolCall` and call `self.tools.dispatch`; before
    // dispatch, if the mapped `Action`'s `Authority::permits` decision is
    // `Decision::NeedsApproval(reason)`, stop the loop immediately and return
    // `AgentOutcome::AwaitingApproval` with a `PendingApproval` built from the call — do not
    // dispatch it. (8) a `search.*`/`fs.*`/... `Denied`/`Completed` result becomes a
    // `ContentBlock::ToolResult` appended to the next request's messages so the model can adapt,
    // per the "denial is a tool result, not a crash" non-negotiable. (9) a `ticket.submit`
    // (`ToolName::TicketSubmit`) call that dispatches successfully ends the run: build an
    // `EvidenceBundle` from `crate::session::Session::promote` and return
    // `AgentOutcome::Submitted`. (10) if the model's completion has `StopReason::EndTurn` with
    // no tool calls and no submission has happened, treat this as
    // `AgentOutcome::Failed { class: FailureClass::Other, detail: "model ended turn without
    // submitting" }` rather than looping forever. (11) exceeding `self.max_steps` without
    // submitting is `AgentOutcome::Failed` with `FailureClass::Other`, detail noting the step
    // limit.
    pub async fn run(&mut self, task: AgentTask) -> Result<AgentOutcome> {
        todo!("drive the provider fabric + tool dispatch step loop to a terminal or suspended AgentOutcome, per the IMPL note")
    }

    /// Resume a previously suspended run after `approval.decided` landed.
    ///
    /// # Errors
    /// Returns `Err` under the same infrastructure-failure conditions as [`AgentLoop::run`].
    // IMPL: reconstruct the pending tool call from `pending` (as captured in
    // `AgentOutcome::AwaitingApproval`'s `PendingApproval`); if `approved` is false, synthesize
    // a `ToolOutcome::Denied` result (reason: "approval declined") and continue the loop exactly
    // as step (8)/(9)/(10)/(11) of `run`'s IMPL note describe, starting from `steps_so_far`
    // rather than an empty transcript. If `approved` is true, dispatch the call for real (it was
    // never dispatched in `run`) and continue identically.
    pub async fn resume(
        &mut self,
        task: AgentTask,
        steps_so_far: Vec<crate::outcome::StepRecord>,
        pending: crate::outcome::PendingApproval,
        approved: bool,
    ) -> Result<AgentOutcome> {
        todo!("re-dispatch or deny the pending call and continue the step loop from steps_so_far, per the IMPL note")
    }

    /// The loop's current prompt-cache bookkeeping, for diagnostics.
    pub fn cache_state(&self) -> &PromptCacheState {
        &self.cache
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
