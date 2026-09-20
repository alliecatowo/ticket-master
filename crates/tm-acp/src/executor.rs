//! [`AcpExecutor`]: the `acp` adapter `docs/audit-2026-09-18-fable.md` B-12/B-04 names —
//! `tm_core::Executor` for an external ACP-speaking coding agent, registered alongside
//! `tm_agent::BuiltinExecutor`/`HumanExecutor` in whatever `ExecutorRegistry` a binary builds
//! (`crates/tm-cli/src/dispatch.rs`'s `build_dispatcher`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tm_core::executor::{
    CostClass, ExecutionHandle, Executor, ExecutorCapabilities, ExecutorFailure, ExecutorOutcome,
    ExecutorTask,
};
use tm_core::{ArtifactKind, FailureClass, Store};
use tm_types::{ArtifactId, Spend, TicketId};

use crate::client::AcpClient;
use crate::error::AcpError;
use crate::protocol::{ContentBlock, StopReason};

/// Static configuration for one external ACP agent this executor drives.
#[derive(Debug, Clone)]
pub struct AcpAgentConfig {
    /// argv to launch the agent; `command[0]` is the program.
    pub command: Vec<String>,
    /// Working directory both the spawned process and the ACP session are rooted at.
    pub cwd: PathBuf,
    /// Per-call timeout (`initialize`/`session/new`/`session/prompt` are each bounded by this
    /// individually, not combined) — see `crate::connection::Outbound::call`'s doc comment.
    pub timeout: Duration,
}

/// Drives an external ACP-speaking coding agent as a [`tm_core::executor::Executor`]. Every
/// fs/shell/network action the agent wants to take is authorized through
/// `session/request_permission` against *this task's own* already-attenuated
/// [`tm_types::Authority`], never the host process's ambient authority — the same discipline
/// `tm_agent::executor::BuiltinExecutor`'s doc comment describes (`SPEC.md` §24.3: "authority is
/// enforced on our side of the executor boundary"), applied here to a process whose internals
/// this crate does not control at all.
///
/// Scope, stated plainly: this executor runs one `session/prompt` turn per dispatched ticket and
/// returns whatever text the agent streamed back as the submission summary. It does not
/// implement `session/cancel`, session resumption, or any MCP-server wiring for the launched
/// agent — real extensions, deliberately left for later per this crate's own scope note, not
/// gaps papered over.
pub struct AcpExecutor {
    id: String,
    config: AcpAgentConfig,
    store: Arc<Store>,
}

impl AcpExecutor {
    /// Build an executor identified as `id`, launching `config.command` fresh for every
    /// dispatched ticket (mirroring `tm_agent::executor::BuiltinExecutor::build`'s "fresh state
    /// per call" discipline — a wedged or crashed agent from one ticket can never leak into the
    /// next). `store` is used only to persist the agent's transcript as submission evidence; this
    /// executor does not otherwise touch project state — every write the external agent itself
    /// makes lands directly on disk inside `config.cwd`, authorized call-by-call rather than
    /// collected as a diff (see [`AcpExecutor::capabilities`]'s `patch_output: false`).
    pub fn new(id: impl Into<String>, config: AcpAgentConfig, store: Arc<Store>) -> Self {
        AcpExecutor {
            id: id.into(),
            config,
            store,
        }
    }

    fn store_transcript(
        &self,
        ticket: &TicketId,
        actor: tm_types::ParticipantId,
        summary: &str,
    ) -> tm_types::Result<Vec<ArtifactId>> {
        let events = self.store.store_artifact(
            ArtifactKind::Report,
            "text/plain".to_string(),
            summary.as_bytes().to_vec(),
            serde_json::json!({"source": "acp", "executor": self.id}),
            Some(ticket.clone()),
            actor,
        )?;
        Ok(events
            .iter()
            .filter_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
            .collect())
    }
}

/// Run one full `initialize` -> `session/new` -> `session/prompt` turn against `client`,
/// returning a submission summary and the turn's stop reason.
///
/// The summary is never empty, even when the agent streamed no `agent_message_chunk` text — an
/// agent that works entirely through tool calls and ends its turn silently is completely
/// ordinary, not an error. This matters beyond cosmetics: `tm_scheduler::dispatch::
/// report_outcome`'s success path treats *empty evidence* as a failure ("executor reported
/// success but produced no evidence"), and `AcpExecutor::execute` only attaches evidence when
/// `summary` is non-empty — so a real, successful, silent-turn run would otherwise be recorded
/// as a failure on a clean `end_turn`. Falling back to a description of the run (session id, stop
/// reason, the fact that no message text was streamed) keeps that path honest instead.
async fn run_task(
    client: &AcpClient,
    task: &ExecutorTask,
) -> Result<(String, StopReason), AcpError> {
    client.initialize().await?;
    let session = client.new_session().await?;
    let prompt_text = format!("{}\n\n{}", task.objective, task.context_pack);
    let response = client
        .prompt(&session.session_id, vec![ContentBlock::text(prompt_text)])
        .await?;
    let transcript = client.drain_transcript().await;
    let summary = if transcript.is_empty() {
        format!(
            "ACP session {} completed with stop reason {:?}; the agent streamed no \
             agent_message_chunk text (it likely worked entirely through tool calls).",
            session.session_id, response.stop_reason
        )
    } else {
        transcript.join("\n")
    };
    Ok((summary, response.stop_reason))
}

#[async_trait]
impl Executor for AcpExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            // The underlying agent streams `session/update` chunks internally, but nothing on
            // the `Executor` trait surface exposes that further up (`execute` returns one final
            // `ExecutorOutcome`) — honestly `false` rather than claiming a streaming surface this
            // executor doesn't expose to its own caller.
            streaming: false,
            tool_use: true,
            // The agent edits files directly, authorized call-by-call via
            // `session/request_permission`, not as a diff this crate parses back out.
            patch_output: false,
            interactive: false,
            accepts_context_pack: true,
            // Not OS-sandboxed via `tm_core::executor::sandbox_for` — every action is instead
            // gated one at a time through the permission callback, a real but different
            // enforcement mechanism than the one this field documents, so this stays `false`
            // rather than overclaiming.
            sandboxed: false,
            max_context_tokens: None,
            cost_class: CostClass::Standard,
        }
    }

    async fn execute(&self, task: ExecutorTask) -> tm_types::Result<ExecutorOutcome> {
        let ticket = task.ticket.clone();
        let mut client = match AcpClient::spawn(
            &self.config.command,
            &self.config.cwd,
            task.authority.clone(),
            self.config.timeout,
        )
        .await
        {
            Ok(client) => client,
            Err(e) => {
                return Ok(ExecutorOutcome {
                    ticket,
                    summary: String::new(),
                    evidence: Vec::new(),
                    patch: None,
                    usage: Spend::default(),
                    decisions: Vec::new(),
                    failure: Some(ExecutorFailure {
                        class: FailureClass::ExecutorCrash,
                        detail: format!("failed to spawn ACP agent: {e}"),
                    }),
                });
            }
        };

        let result = run_task(&client, &task).await;
        // Every path below returns; kill the child regardless of how the run ended, so a wedged
        // or misbehaving external agent process is never left running past the call that was
        // driving it (`crate::client::AcpClient::kill`'s doc comment).
        client.kill().await;

        match result {
            Ok((summary, stop_reason)) => {
                // `run_task` never returns an empty `summary` (see its own doc comment on why
                // that matters for `report_outcome`'s "empty evidence means failure" check), so
                // this always has something real to attach as evidence.
                let evidence = self.store_transcript(&ticket, task.actor.clone(), &summary)?;
                let failure = match stop_reason {
                    StopReason::EndTurn => None,
                    other => Some(ExecutorFailure {
                        class: FailureClass::Other,
                        detail: format!(
                            "ACP agent stopped with reason {other:?} rather than end_turn"
                        ),
                    }),
                };
                Ok(ExecutorOutcome {
                    ticket,
                    summary,
                    evidence,
                    patch: None,
                    usage: Spend::default(),
                    decisions: Vec::new(),
                    failure,
                })
            }
            Err(e) => Ok(ExecutorOutcome {
                ticket,
                summary: String::new(),
                evidence: Vec::new(),
                patch: None,
                usage: Spend::default(),
                decisions: Vec::new(),
                failure: Some(ExecutorFailure {
                    class: FailureClass::ExecutorCrash,
                    detail: e.to_string(),
                }),
            }),
        }
    }

    async fn cancel(&self, _handle: &ExecutionHandle) -> tm_types::Result<()> {
        // `session/cancel` is not wired up in this crate's MVP (see this type's scope note); the
        // run either completes or the caller drops the future — the same limitation
        // `tm_agent::executor::BuiltinExecutor::cancel` documents for its own loop.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tm_core::ticket::ExecutorRequirements;
    use tm_scheduler::select::capabilities_satisfy;
    use tm_types::{Authority, ParticipantId, Role, Tolerance};

    fn task_fixture() -> ExecutorTask {
        ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "do the thing".to_string(),
            context_pack: "pack text".to_string(),
            authority: Authority::none(),
            budget: tm_types::Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new("agent:acp/T-1").expect("participant"),
            session: None,
        }
    }

    fn open_scratch_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::FixedClock::epoch());
        let ids: Arc<dyn tm_types::IdSource> = Arc::new(tm_types::CounterIds::new());
        let store = Store::open_with(dir.path(), clock, ids).expect("open store");
        (dir, store)
    }

    /// Regression test for the bug an adversarial review caught: an ACP agent that works
    /// entirely through tool calls and ends its turn without ever streaming an
    /// `agent_message_chunk` is a completely ordinary success, not an error — but
    /// `tm_scheduler::dispatch::report_outcome`'s success path treats empty evidence as a
    /// failure, and evidence here is derived from `summary`. Drives `run_task` directly (no
    /// subprocess) against a fake agent that answers `session/prompt` with no `session/update`
    /// at all, and asserts the summary this crate hands back is never empty.
    #[tokio::test]
    async fn run_task_never_returns_an_empty_summary_even_with_no_streamed_text() {
        let (agent_io, client_io) = tokio::io::duplex(64 * 1024);
        let (agent_read, agent_write) = tokio::io::split(agent_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let agent_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(agent_read);
            let mut writer = agent_write;

            let line = crate::jsonrpc::read_line(&mut reader)
                .await
                .unwrap()
                .unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["method"], "initialize");
            crate::jsonrpc::write_message(
                &mut writer,
                &json!({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": 1}}),
            )
            .await
            .unwrap();

            let line = crate::jsonrpc::read_line(&mut reader)
                .await
                .unwrap()
                .unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["method"], "session/new");
            crate::jsonrpc::write_message(
                &mut writer,
                &json!({"jsonrpc": "2.0", "id": req["id"], "result": {"sessionId": "s-silent"}}),
            )
            .await
            .unwrap();

            let line = crate::jsonrpc::read_line(&mut reader)
                .await
                .unwrap()
                .unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["method"], "session/prompt");
            // Answer without ever sending a `session/update` — the ordinary "worked entirely
            // through tool calls, said nothing" case this test exists to prove is not treated
            // as an empty/failed run.
            crate::jsonrpc::write_message(
                &mut writer,
                &json!({"jsonrpc": "2.0", "id": req["id"], "result": {"stopReason": "end_turn"}}),
            )
            .await
            .unwrap();
        });

        let client = AcpClient::connect(
            client_read,
            client_write,
            Authority::none(),
            PathBuf::from("/repo"),
            Duration::from_secs(5),
        );
        let task = task_fixture();
        let (summary, stop_reason) = run_task(&client, &task).await.expect("run_task succeeds");

        assert!(
            !summary.is_empty(),
            "summary must never be empty — see run_task's doc comment"
        );
        assert!(summary.contains("s-silent"));
        assert_eq!(stop_reason, StopReason::EndTurn);

        agent_task.await.expect("fake agent task panicked");
    }

    /// `ExecutorDispatcher::dispatch` refuses a candidate whose declared capabilities don't
    /// satisfy `capabilities_satisfy` before ever calling `execute` — assert `AcpExecutor`
    /// actually clears that gate for an ordinary non-human ticket, rather than only asserting
    /// its fields look right by eye.
    #[test]
    fn capabilities_satisfy_the_dispatchers_own_gate_for_an_ordinary_ticket() {
        let (_dir, store) = open_scratch_store();
        let executor = AcpExecutor::new(
            "acp",
            AcpAgentConfig {
                command: vec!["true".to_string()],
                cwd: PathBuf::from("/repo"),
                timeout: Duration::from_secs(1),
            },
            Arc::new(store),
        );
        let reqs = ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Any,
        };
        assert!(capabilities_satisfy(&reqs, &executor.capabilities()).is_ok());
    }
}
