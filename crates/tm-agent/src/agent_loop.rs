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
use std::sync::Arc;

use tm_core::{FailureClass, Store};
use tm_events::payload::{
    ApprovalDecidedPayload, ApprovalRequestedPayload, ProviderDegradedPayload,
    ProviderExhaustedPayload, ProviderRecoveredPayload, ProviderSelectedPayload,
    SessionEndedPayload, SessionStartedPayload,
};
use tm_events::{EventDraft, Payload};
use tm_harness::config::PromptFragments;
use tm_provider::fabric::{Fabric, FabricRecord};
use tm_provider::{CompletionRequest, ContentBlock, Message, MessageRole};
use tm_types::{
    Authority, Budget, CallContext, Clock, Decision, Id, IdSource, ParticipantId, Result, Role,
    Spend, TmError,
};

use crate::outcome::{
    AgentOutcome, AgentTask, BudgetDimension, PendingApproval, StepRecord, ToolCallRecord,
};
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

/// Upper bound on generated tokens per provider call.
const MAX_TOKENS_PER_STEP: u32 = 4096;

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
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    role: Role,
    actor: ParticipantId,
    max_steps: u32,
    /// Durable home for everything this loop produces (`docs/audit-2026-09-18-fable.md` B-07):
    /// session bracketing, usage debiting/history, provider routing events and approval
    /// events. Every write through this handle goes through [`tm_core::Store::append`] or one
    /// of its typed helpers, so it materializes in the same transaction it's appended in.
    store: Arc<Store>,
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
            cache: PromptCacheState::new(),
            clock,
            ids,
            role,
            actor,
            max_steps: DEFAULT_MAX_STEPS,
            store,
        }
    }

    /// Override the default step limit (mainly for tests that want to force
    /// [`AgentOutcome::Failed`] quickly).
    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// This loop's own authority ceiling, as constructed via [`AgentLoop::new`] — i.e. before
    /// intersecting with any particular [`AgentTask::authority`]. Exposed so a caller
    /// constructing the loop (e.g. `BuiltinExecutor`) can be tested for *what* it passed as the
    /// ceiling, not just inferred from run-time behaviour.
    pub fn authority(&self) -> &Authority {
        &self.authority
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
        let effective_authority = self.authority.intersect(&task.authority);
        let root = project_root();

        // The decision point itself: a human (or whatever resumed this suspended run) has
        // already decided `approved` by the time `resume` is called, independent of how the
        // dispatch below resolves — see `docs/audit-2026-09-18-fable.md` B-07.
        self.store.append(vec![EventDraft::new(
            self.actor.clone(),
            Id::from(task.ticket.clone()),
            Payload::from(ApprovalDecidedPayload {
                ticket: Some(task.ticket.clone()),
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

        let resolution = if approved {
            let ctx = CallContext {
                authority: &effective_authority,
                ticket: &task.ticket,
                session: &task.session,
                actor: &self.actor,
                clock: self.clock.as_ref(),
                ids: self.ids.as_ref(),
                root: &root,
            };
            self.tools.dispatch(&call, &ctx).await
        } else {
            ToolOutcome::Denied {
                reason: "approval declined".to_string(),
            }
        };

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

        if pending.tool_name == TICKET_SUBMIT {
            if let ToolOutcome::Completed { .. } = &resolution {
                steps.push(step);
                let evidence = self.build_evidence(&task, &steps, &pending.input);
                let outcome = Ok(AgentOutcome::Submitted { evidence, steps });
                self.finish_session(&task, &outcome)?;
                return outcome;
            }
        }

        steps.push(step);
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
    /// [`tm_core::Store::record_usage`], and record it durably as `usage.recorded`.
    ///
    /// A [`TmError::BudgetExhausted`] from the store is deliberately swallowed here rather than
    /// propagated: [`AgentLoop::drive`]'s own `effective_budget` tracking (updated by the caller
    /// right alongside this call) already reflects the same overspend, and
    /// [`first_exhausted_dimension`] at the top of the *next* loop iteration is what actually
    /// produces `AgentOutcome::BudgetExhausted` — preserving the "never mid-edit" contract that
    /// method's doc comment promises (this step's tool calls still need to run to completion).
    /// A real reconciliation between the loop's own ceiling and the ticket's durable budget is
    /// `docs/audit-2026-09-18-fable.md` B-10's scope, not this one's.
    fn record_usage(&self, task: &AgentTask, spend: Spend) -> Result<()> {
        match self.store.record_usage(
            Some(&task.ticket),
            Some(&task.session),
            spend,
            self.actor.clone(),
        ) {
            Ok(_events) => Ok(()),
            Err(TmError::BudgetExhausted(_)) => Ok(()),
            Err(e) => Err(e),
        }
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
                EventDraft::new(self.actor.clone(), Id::from(task.ticket.clone()), payload)
                    .with_session(task.session.clone())
            })
            .collect();
        self.store.append(drafts)?;
        Ok(())
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

        let root = project_root();
        let fragments = PromptFragments {
            system_preamble: String::new(),
            closing_reminder: String::new(),
            extra: BTreeMap::new(),
        };
        let rendered = crate::prompt::render(&task.ticket, &task.context_pack, &fragments);
        let mut messages = rebuild_messages(&rendered.task, &steps);

        loop {
            if steps.len() as u32 >= self.max_steps {
                return Ok(AgentOutcome::Failed {
                    steps,
                    class: FailureClass::Other,
                    detail: format!("step limit ({}) reached without submitting", self.max_steps),
                });
            }
            if let Some(exhausted) = first_exhausted_dimension(&effective_budget) {
                return Ok(AgentOutcome::BudgetExhausted { steps, exhausted });
            }

            let request = CompletionRequest {
                system: Some(rendered.system.clone()),
                messages: messages.clone(),
                tools: self.tools.tool_defs_for(&effective_authority),
                max_tokens: MAX_TOKENS_PER_STEP,
                temperature: Some(0.0),
                stop_sequences: Vec::new(),
                stream: false,
                n: 1,
            };

            let completion = match self.fabric.execute(self.role, request).await {
                Ok(completion) => completion,
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
                dollars_micros: 0,
                wall_seconds: completion.latency.as_secs(),
            };
            if effective_budget.try_spend(step_spend).is_err() {
                effective_budget.spent = effective_budget.spent.plus(step_spend);
            }
            self.record_usage(task, step_spend)?;

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
                steps.push(StepRecord {
                    index: step_index,
                    served_by,
                    assistant_text,
                    tool_calls: Vec::new(),
                    spend: step_spend,
                    at: self.clock.now(),
                });
                return Ok(AgentOutcome::Failed {
                    steps,
                    class: FailureClass::Other,
                    detail: "model ended turn without submitting".to_string(),
                });
            }

            let mut tool_call_records: Vec<ToolCallRecord> = Vec::new();

            for (id, name, input) in &tool_uses {
                if let Ok(action) = self.tools.to_action(name, input) {
                    if let Decision::NeedsApproval(reason) = effective_authority.permits(&action) {
                        self.store.append(vec![EventDraft::new(
                            self.actor.clone(),
                            Id::from(task.ticket.clone()),
                            Payload::from(ApprovalRequestedPayload {
                                ticket: Some(task.ticket.clone()),
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

                let call = ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                };
                let resolution = {
                    let ctx = CallContext {
                        authority: &effective_authority,
                        ticket: &task.ticket,
                        session: &task.session,
                        actor: &self.actor,
                        clock: self.clock.as_ref(),
                        ids: self.ids.as_ref(),
                        root: &root,
                    };
                    self.tools.dispatch(&call, &ctx).await
                };

                tool_call_records.push(ToolCallRecord {
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    input: input.clone(),
                    resolution: resolution.clone(),
                });

                if name == TICKET_SUBMIT {
                    if let ToolOutcome::Completed { .. } = &resolution {
                        steps.push(StepRecord {
                            index: step_index,
                            served_by,
                            assistant_text,
                            tool_calls: tool_call_records,
                            spend: step_spend,
                            at: self.clock.now(),
                        });
                        let evidence = self.build_evidence(task, &steps, input);
                        return Ok(AgentOutcome::Submitted { evidence, steps });
                    }
                }
            }

            messages.push(Message {
                role: MessageRole::Assistant,
                content: candidate.content,
            });
            let result_blocks = tool_call_records
                .iter()
                .map(|tcr| {
                    let (text, is_error) = tool_result_text(&tcr.resolution);
                    ContentBlock::ToolResult {
                        tool_use_id: tcr.tool_use_id.clone(),
                        content: vec![ContentBlock::Text { text }],
                        is_error,
                    }
                })
                .collect();
            messages.push(Message {
                role: MessageRole::User,
                content: result_blocks,
            });

            steps.push(StepRecord {
                index: step_index,
                served_by,
                assistant_text,
                tool_calls: tool_call_records,
                spend: step_spend,
                at: self.clock.now(),
            });
        }
    }

    /// Promote everything durable out of `steps` and wrap it into the [`crate::EvidenceBundle`]
    /// an [`AgentOutcome::Submitted`] carries, pulling the human-readable summary out of the
    /// `ticket.submit` call's own arguments.
    fn build_evidence(
        &self,
        task: &AgentTask,
        steps: &[StepRecord],
        submit_input: &serde_json::Value,
    ) -> crate::outcome::EvidenceBundle {
        let mut session = Session::new(
            task.session.clone(),
            task.ticket.clone(),
            task.harness_epoch,
            self.clock.now(),
        );
        session.transcript = steps.to_vec();
        let promotion = session.promote();
        let summary = submit_input
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("submitted")
            .to_string();
        crate::outcome::EvidenceBundle {
            ticket: task.ticket.clone(),
            artifacts: promotion.artifacts,
            decisions: promotion.decisions,
            summary,
        }
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
fn rebuild_messages(task_prompt: &str, steps: &[StepRecord]) -> Vec<Message> {
    let mut messages = vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: task_prompt.to_string(),
        }],
    }];

    for step in steps {
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
        messages.push(Message {
            role: MessageRole::Assistant,
            content: assistant_content,
        });

        if !step.tool_calls.is_empty() {
            let result_blocks = step
                .tool_calls
                .iter()
                .map(|tc| {
                    let (text, is_error) = tool_result_text(&tc.resolution);
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

    messages
}

/// Render one [`crate::outcome::ToolCallResolution`] as the text (and error flag) a provider's
/// `ContentBlock::ToolResult` should carry.
fn tool_result_text(resolution: &ToolOutcome) -> (String, bool) {
    match resolution {
        ToolOutcome::Completed { result, .. } => (result.to_string(), false),
        ToolOutcome::Denied { reason } => (format!("denied: {reason}"), true),
        ToolOutcome::Errored { detail } => (format!("error: {detail}"), true),
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

    // Presence check only: `TicketId` must be constructible in this module's tests without
    // reaching into `tm-core`'s store, since the tool/session/prompt modules this loop depends
    // on are implemented by concurrently-written sibling files.
    #[test]
    fn ticket_id_round_trips_through_display() {
        let id = TicketId::new("T-1").expect("valid ticket id literal");
        assert_eq!(id.to_string(), "T-1");
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
    use tm_types::{FixedClock, SessionId, TestIds};

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
                ticket: self.ticket.clone(),
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

    /// Build the exact [`CompletionRequest`] `AgentLoop::drive` will issue for `task` against a
    /// freshly built `AgentLoop` over `tools`/`fabric`, so a test can script
    /// [`MockProvider::script_response`]/`script_failure` against a request guaranteed to match
    /// (`MockProvider` keys scripts by an exact hash of the serialized request).
    fn expected_request(loop_: &AgentLoop, task: &AgentTask) -> CompletionRequest {
        let effective_authority = loop_.authority.intersect(&task.authority);
        let fragments = PromptFragments {
            system_preamble: String::new(),
            closing_reminder: String::new(),
            extra: BTreeMap::new(),
        };
        let rendered = crate::prompt::render(&task.ticket, &task.context_pack, &fragments);
        let messages = rebuild_messages(&rendered.task, &[]);
        CompletionRequest {
            system: Some(rendered.system),
            messages,
            tools: loop_.tools.tool_defs_for(&effective_authority),
            max_tokens: MAX_TOKENS_PER_STEP,
            temperature: Some(0.0),
            stop_sequences: Vec::new(),
            stream: false,
            n: 1,
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
        );
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
        );
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
}
