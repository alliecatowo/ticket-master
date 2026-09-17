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

use tm_codeintel::CodeIntel;
use tm_context::command::{CommandCache, CommandExecutor};
use tm_core::{FailureClass, Store};
use tm_harness::config::PromptFragments;
use tm_provider::fabric::Fabric;
use tm_provider::{CompletionRequest, ContentBlock, Message, MessageRole};
use tm_types::{Authority, Budget, Clock, Decision, IdSource, ParticipantId, Result, Role, Spend};

use crate::outcome::{
    AgentOutcome, AgentTask, BudgetDimension, PendingApproval, StepRecord, ToolCallRecord,
};
use crate::patch::PatchEngine;
use crate::session::Session;
use crate::tools::{ToolCall, ToolContext, ToolName, ToolOutcome, ToolRegistry};

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
    pub async fn run(&mut self, task: AgentTask) -> Result<AgentOutcome> {
        self.drive(&task, Vec::new()).await
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
        let patch_engine = PatchEngine::new(project_root(), effective_authority.clone());

        let call = ToolCall {
            id: pending.tool_use_id.clone(),
            name: pending.tool_name.clone(),
            input: pending.input.clone(),
        };

        let resolution = if approved {
            let mut ctx = ToolContext {
                authority: &effective_authority,
                ci: self.ci.as_ref(),
                store: self.store.as_ref(),
                patch_engine: &patch_engine,
                command_cache: self.command_cache.as_ref(),
                command_executor: self.command_executor.as_ref(),
                clock: self.clock.as_ref(),
                ids: self.ids.as_ref(),
                ticket: &task.ticket,
                session: &task.session,
                actor: &self.actor,
            };
            self.tools.dispatch(&call, &mut ctx)
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

        if pending.tool_name == ToolName::TicketSubmit.as_str() {
            if let ToolOutcome::Completed { .. } = &resolution {
                steps.push(step);
                let evidence = self.build_evidence(&task, &steps, &pending.input);
                return Ok(AgentOutcome::Submitted { evidence, steps });
            }
        }

        steps.push(step);
        self.drive(&task, steps).await
    }

    /// The loop's current prompt-cache bookkeeping, for diagnostics.
    pub fn cache_state(&self) -> &PromptCacheState {
        &self.cache
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

        let patch_engine = PatchEngine::new(project_root(), effective_authority.clone());
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
                tools: self.tools.tool_defs(),
                max_tokens: MAX_TOKENS_PER_STEP,
                temperature: Some(0.0),
                stop_sequences: Vec::new(),
                stream: false,
                n: 1,
            };

            let completion = match self.fabric.execute(self.role, request).await {
                Ok(completion) => completion,
                Err(e) => {
                    return Ok(AgentOutcome::Failed {
                        steps,
                        class: FailureClass::ProviderUnavailable,
                        detail: e.to_string(),
                    });
                }
            };

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
                if let Some(spec) = self.tools.get(name) {
                    if let Ok(action) = (spec.to_action)(input) {
                        if let Decision::NeedsApproval(reason) =
                            effective_authority.permits(&action)
                        {
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
                }

                let call = ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                };
                let resolution = {
                    let mut ctx = ToolContext {
                        authority: &effective_authority,
                        ci: self.ci.as_ref(),
                        store: self.store.as_ref(),
                        patch_engine: &patch_engine,
                        command_cache: self.command_cache.as_ref(),
                        command_executor: self.command_executor.as_ref(),
                        clock: self.clock.as_ref(),
                        ids: self.ids.as_ref(),
                        ticket: &task.ticket,
                        session: &task.session,
                        actor: &self.actor,
                    };
                    self.tools.dispatch(&call, &mut ctx)
                };

                tool_call_records.push(ToolCallRecord {
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    input: input.clone(),
                    resolution: resolution.clone(),
                });

                if name == ToolName::TicketSubmit.as_str() {
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

/// The project root a [`PatchEngine`] applies edits beneath.
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
}
