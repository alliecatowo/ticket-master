//! [`ExecutorDispatcher`]: turns a [`crate::plan::SchedulerAction::Lease`] into a real
//! executor doing real work, instead of leasing to a placeholder holder id and stopping
//! (`SPEC.md` §24, audit B-01/B-04).
//!
//! [`crate::driver::SchedulerLoop::apply_action`] calls [`ExecutorDispatcher::dispatch`]
//! synchronously and non-blockingly: it picks the executor, checks capabilities, acquires the
//! lease with the executor's *real* participant id, transitions the ticket to `Running`, and
//! then hands the actual run off to a background task on an injected
//! [`tokio::runtime::Handle`] — so a tick stays fast and deterministic (the events it returns
//! are exactly the ones the synchronous portion produced) while execution proceeds
//! independently, with a TTL heartbeat keeping the lease alive while it runs.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tm_core::executor::{validate_return_scope, ExecutorOutcome, ExecutorTask};
use tm_core::store::Store;
use tm_core::ticket::{FailureClass, TicketState, Trigger};
use tm_core::{ArtifactKind, EvidenceKind, Executor, Ticket};
use tm_events::Event;
use tm_types::{LeaseId, ParticipantId, Role, TicketId};

use crate::select::capabilities_satisfy;

/// Compiles the context pack for `ticket` into the rendered text an [`tm_core::ExecutorTask`]
/// carries. A narrow seam (mirrors `crate::select::ExecutorAvailability`'s pattern) so
/// `tm-scheduler` never depends on `tm-context`; the real implementation lives with whichever
/// caller already has a `tm_context`/`tm_codeintel` handle (`tm-cli`'s `Project`).
pub trait ContextPackSource: Send + Sync {
    /// Compile and render the context pack for `ticket`.
    fn compile(&self, ticket: &TicketId) -> tm_types::Result<String>;
}

/// Which concrete [`Executor`] serves which [`Role`], plus the dedicated human executor for
/// `human_required` tickets (bypassing role lookup entirely, matching
/// `crate::select::executor_matches`'s existing "human is always matchable" rule).
pub struct ExecutorRegistry {
    by_role: BTreeMap<Role, Arc<dyn Executor>>,
    human: Arc<dyn Executor>,
}

impl ExecutorRegistry {
    /// Build a registry with `human` as the dedicated human executor and no role executors yet.
    pub fn new(human: Arc<dyn Executor>) -> Self {
        ExecutorRegistry {
            by_role: BTreeMap::new(),
            human,
        }
    }

    /// Register `executor` to serve `role`. A later call for the same `role` replaces the
    /// earlier registration.
    pub fn register(&mut self, role: Role, executor: Arc<dyn Executor>) {
        self.by_role.insert(role, executor);
    }

    /// The executor that should serve `role`, if any is registered.
    pub fn for_role(&self, role: Role) -> Option<Arc<dyn Executor>> {
        self.by_role.get(&role).cloned()
    }

    /// The dedicated human executor.
    pub fn human(&self) -> Arc<dyn Executor> {
        self.human.clone()
    }
}

/// Why [`ExecutorDispatcher::dispatch`] could not hand a ticket to an executor.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// No executor is registered for the ticket's required role.
    #[error("no executor registered for role {0}")]
    NoExecutorForRole(Role),
    /// A candidate executor's declared capabilities do not satisfy the ticket's requirements.
    #[error("executor capability mismatch: {0}")]
    CapabilityMismatch(#[from] crate::select::CapabilityMismatch),
    /// The underlying `tm-core` call failed.
    #[error(transparent)]
    Core(#[from] tm_types::TmError),
}

impl From<DispatchError> for tm_types::TmError {
    fn from(err: DispatchError) -> Self {
        match err {
            DispatchError::Core(e) => e,
            other => tm_types::TmError::AuthorityDenied(other.to_string()),
        }
    }
}

/// Routes leased tickets to real [`Executor`]s and drives them to a `Store::submit`/
/// `record_failure` outcome.
pub struct ExecutorDispatcher {
    store: Arc<Store>,
    handle: tokio::runtime::Handle,
    context: Arc<dyn ContextPackSource>,
    registry: ExecutorRegistry,
    repo_root: Option<PathBuf>,
}

impl ExecutorDispatcher {
    /// Build a dispatcher over `store`, spawning background execution/heartbeat tasks onto
    /// `handle`. `repo_root`, when set, is where a turn that produces a real patch also gets a
    /// `git stash create`-style workspace snapshot captured (`crate::snapshot`,
    /// `docs/decisions/D-008-ticket-checkpoint-fork.md`); `None` disables snapshot capture
    /// entirely (e.g. a test harness with no real git working tree to snapshot).
    pub fn new(
        store: Arc<Store>,
        handle: tokio::runtime::Handle,
        context: Arc<dyn ContextPackSource>,
        registry: ExecutorRegistry,
        repo_root: Option<PathBuf>,
    ) -> Self {
        ExecutorDispatcher {
            store,
            handle,
            context,
            registry,
            repo_root,
        }
    }

    /// Dispatch `ticket` (already known as `t`) to a real executor for `ttl_seconds`, as
    /// `actor`. Synchronous and non-blocking: it returns as soon as the lease is acquired and
    /// the ticket has moved `Leased -> Running`; the run itself proceeds on a background task.
    ///
    /// # Errors
    /// - [`DispatchError::NoExecutorForRole`] / [`DispatchError::CapabilityMismatch`] if no
    ///   registered executor can take the ticket at all (refuses rather than degrading quietly,
    ///   `SPEC.md` §24.2).
    /// - Whatever `Store::acquire_lease`/`Store::transition` returns for a state race.
    pub fn dispatch(
        &self,
        ticket: &TicketId,
        t: &Ticket,
        ttl_seconds: u32,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.dispatch_inner(ticket, t, ttl_seconds, actor, None)
    }

    /// Like [`ExecutorDispatcher::dispatch`], but also returns a receiver that resolves once the
    /// spawned run — including [`report_outcome`]'s snapshot capture and patch-artifact storage,
    /// and the executor's own session teardown — has fully finished, not just once the ticket's
    /// own state visibly leaves `Leased`/`Running`. Those two moments differ for real: an
    /// executor that calls `Store::submit` mid-run (`tm-agent`'s `BuiltinExecutor` does, via its
    /// `ticket.submit` tool) can move the ticket to `Submitted` before `Executor::execute`
    /// itself returns, let alone before `report_outcome` runs afterward. `tm run <ticket>
    /// --worktree` needs this: removing the worktree the run executed inside as soon as the
    /// ticket *looks* finished would race `report_outcome` still reading it
    /// (`docs/decisions/D-012-run-worktree-isolation.md`). `dispatch` itself does not pay for an
    /// unused channel — this is opt-in.
    pub fn dispatch_with_completion(
        &self,
        ticket: &TicketId,
        t: &Ticket,
        ttl_seconds: u32,
        actor: ParticipantId,
    ) -> tm_types::Result<(Vec<Event>, tokio::sync::oneshot::Receiver<()>)> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let events = self.dispatch_inner(ticket, t, ttl_seconds, actor, Some(tx))?;
        Ok((events, rx))
    }

    fn dispatch_inner(
        &self,
        ticket: &TicketId,
        t: &Ticket,
        ttl_seconds: u32,
        actor: ParticipantId,
        completion: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> tm_types::Result<Vec<Event>> {
        let executor = if t.executor.human_required {
            self.registry.human()
        } else {
            self.registry
                .for_role(t.executor.role)
                .ok_or(DispatchError::NoExecutorForRole(t.executor.role))?
        };
        capabilities_satisfy(&t.executor, &executor.capabilities())
            .map_err(DispatchError::CapabilityMismatch)?;

        let holder = holder_for(&executor, ticket, t.executor.human_required)?;

        let lease_events = self.store.acquire_lease(
            ticket,
            holder.clone(),
            t.authority.clone(),
            t.resources.clone(),
            ttl_seconds,
            actor.clone(),
        )?;
        let lease_id = lease_events
            .iter()
            .find_map(|e| e.payload.as_ticket_leased().map(|p| p.lease.clone()))
            .ok_or_else(|| {
                tm_types::TmError::Invariant("acquire_lease did not emit ticket.leased".to_string())
            })?;

        let mut events = lease_events;
        events.extend(self.store.transition(ticket, Trigger::WorkStarted, actor)?);

        self.spawn_run(
            ticket.clone(),
            t.clone(),
            executor,
            holder,
            lease_id,
            ttl_seconds,
            completion,
        );

        Ok(events)
    }

    /// Hand the actual run to a background task: compile the pack, keep the lease's heartbeat
    /// alive while `executor.execute` runs, route the outcome to `Store::submit` /
    /// `Store::record_failure` when it returns, and — when `completion` is `Some` (see
    /// [`ExecutorDispatcher::dispatch_with_completion`]) — signal it only after all of that is
    /// done.
    #[allow(clippy::too_many_arguments)]
    fn spawn_run(
        &self,
        ticket: TicketId,
        t: Ticket,
        executor: Arc<dyn Executor>,
        holder: ParticipantId,
        lease_id: LeaseId,
        ttl_seconds: u32,
        completion: Option<tokio::sync::oneshot::Sender<()>>,
    ) {
        let store = self.store.clone();
        let context = self.context.clone();
        let repo_root = self.repo_root.clone();
        self.handle.spawn(async move {
            run_and_report(
                store,
                context,
                ticket,
                t,
                executor,
                holder,
                lease_id,
                ttl_seconds,
                repo_root,
            )
            .await;
            if let Some(tx) = completion {
                // The receiver may already be gone (e.g. a caller that dropped it after its own
                // wait timed out); nothing to do about that, the run itself already finished.
                let _ = tx.send(());
            }
        });
    }
}

/// Real participant id for the executor that was actually chosen — never the deterministic
/// `agent:<role>/<ticket>` placeholder the pre-dispatch driver used, though the shape stays
/// deterministic per `P-09` (`docs/audit-2026-09-18-fable.md`): `agent:<executor id>/<ticket>`,
/// or `human:<ticket>` for the human executor.
fn holder_for(
    executor: &Arc<dyn Executor>,
    ticket: &TicketId,
    human_required: bool,
) -> tm_types::Result<ParticipantId> {
    if human_required {
        ParticipantId::new(format!("human:{ticket}"))
    } else {
        ParticipantId::new(format!("agent:{}/{ticket}", executor.id()))
    }
}

/// How often the heartbeat task refreshes the lease: a third of the TTL, floored at one second,
/// so a lease with any TTL still gets at least a couple of heartbeats before it could expire.
fn heartbeat_interval(ttl_seconds: u32) -> Duration {
    Duration::from_secs(u64::from(ttl_seconds / 3).max(1))
}

/// The background task body: compile the pack, run the executor while a heartbeat task keeps
/// the lease alive, then report the outcome.
#[allow(clippy::too_many_arguments)]
async fn run_and_report(
    store: Arc<Store>,
    context: Arc<dyn ContextPackSource>,
    ticket: TicketId,
    t: Ticket,
    executor: Arc<dyn Executor>,
    holder: ParticipantId,
    lease_id: LeaseId,
    ttl_seconds: u32,
    repo_root: Option<PathBuf>,
) {
    let pack = match context.compile(&ticket) {
        Ok(pack) => pack,
        Err(e) => {
            if let Err(store_err) = store.record_failure(
                &ticket,
                FailureClass::Other,
                format!("context pack compilation failed: {e}"),
                holder,
            ) {
                tracing::warn!(%ticket, error = %store_err, "record_failure after pack compilation failure also failed");
            }
            return;
        }
    };

    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let heartbeat_task = {
        let store = store.clone();
        let holder = holder.clone();
        let lease_id = lease_id.clone();
        let interval = heartbeat_interval(ttl_seconds);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        // Heartbeat the exact lease this run acquired, never re-derived from a
                        // live view: `Store::acquire_lease` keeps prior leases for the same
                        // ticket around for epoch computation, so "find a lease for this
                        // ticket" can find a stale, already-expired one on attempt 2+.
                        if store.heartbeat(&lease_id, holder.clone()).is_err() {
                            // The lease is gone (expired or released); nothing left to keep
                            // alive.
                            break;
                        }
                    }
                    _ = &mut stop_rx => break,
                }
            }
        })
    };

    let task = ExecutorTask {
        ticket: ticket.clone(),
        role: t.executor.role,
        objective: t.objective.clone(),
        context_pack: pack,
        authority: t.authority.clone(),
        budget: t.budget,
        harness_epoch: 0,
        actor: holder.clone(),
        session: None,
    };

    let result = executor.execute(task).await;
    let _ = stop_tx.send(());
    let _ = heartbeat_task.await;

    report_outcome(&store, &ticket, &t, result, holder, repo_root.as_deref()).await;
}

/// Translate an [`Executor::execute`] result into `Store::submit`/`Store::record_failure`,
/// validating a produced diff against the ticket's write authority first (`SPEC.md` §24.3).
///
/// An executor that itself calls `Store::submit` mid-run (`tm-agent`'s `BuiltinExecutor` does,
/// via the `ticket.submit` tool) already left the ticket in `TicketState::Submitted` by the time
/// its `Ok` outcome comes back here; calling `Store::submit` again would fail the ticket's
/// `Running -> Submitted` transition (it is no longer `Running`). So on success this re-checks
/// the ticket's *current* state and only submits on the executor's behalf when it is still
/// `Running` (an executor that produces evidence/a patch without itself submitting, e.g. a
/// future external-harness adapter); when it is already `Submitted`, any extra evidence
/// (a `patch`, most likely) is attached rather than re-submitted.
///
/// When the turn produced a real patch and `repo_root` is set, this also captures a
/// `git stash create`-style workspace snapshot (`crate::snapshot::capture_workspace_snapshot`)
/// and records it as a [`ArtifactKind::WorkspaceSnapshot`] artifact tied to `ticket` — best
/// effort, never load-bearing for the submission itself (see that function's own doc comment for
/// why it cannot fail this path).
async fn report_outcome(
    store: &Store,
    ticket: &TicketId,
    t: &Ticket,
    result: tm_types::Result<ExecutorOutcome>,
    holder: ParticipantId,
    repo_root: Option<&std::path::Path>,
) {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(e) => {
            if let Err(store_err) =
                store.record_failure(ticket, FailureClass::ExecutorCrash, e.to_string(), holder)
            {
                tracing::warn!(%ticket, error = %store_err, "record_failure after executor error also failed");
            }
            return;
        }
    };

    let Some(failure) = outcome.failure else {
        let mut evidence = outcome.evidence.clone();
        if let Some(diff) = &outcome.patch {
            if let Err(violation) = validate_return_scope(diff, &t.authority.repository.write) {
                if let Err(store_err) = store.record_failure(
                    ticket,
                    FailureClass::AuthorityDenied,
                    format!("return-scope validation failed: {violation}"),
                    holder,
                ) {
                    tracing::warn!(%ticket, error = %store_err, "record_failure after return-scope violation also failed");
                }
                return;
            }
            match store.store_artifact(
                ArtifactKind::Patch,
                "text/x-diff".to_string(),
                diff.clone().into_bytes(),
                serde_json::json!({}),
                Some(ticket.clone()),
                holder.clone(),
            ) {
                Ok(events) => {
                    if let Some(id) = events
                        .iter()
                        .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
                    {
                        evidence.push(id);
                    }
                    if let Some(root) = repo_root {
                        if let Some(snapshot) = crate::snapshot::capture_workspace_snapshot(root) {
                            let meta = serde_json::json!({
                                "git_ref": snapshot.git_ref,
                                "base_head": snapshot.base_head,
                            });
                            if let Err(e) = store.store_artifact(
                                ArtifactKind::WorkspaceSnapshot,
                                "application/x-git-stash-sha".to_string(),
                                snapshot.sha.clone().into_bytes(),
                                meta,
                                Some(ticket.clone()),
                                holder.clone(),
                            ) {
                                tracing::warn!(%ticket, error = %e, "failed to record workspace snapshot artifact (best-effort, not fatal)");
                            }
                        }
                    }
                }
                Err(e) => {
                    if let Err(store_err) = store.record_failure(
                        ticket,
                        FailureClass::Other,
                        format!("failed to store patch evidence: {e}"),
                        holder,
                    ) {
                        tracing::warn!(%ticket, error = %store_err, "record_failure after patch-storage failure also failed");
                    }
                    return;
                }
            }
        }

        if evidence.is_empty() {
            if let Err(store_err) = store.record_failure(
                ticket,
                FailureClass::Other,
                "executor reported success but produced no evidence".to_string(),
                holder,
            ) {
                tracing::warn!(%ticket, error = %store_err, "record_failure for evidence-less success also failed");
            }
            return;
        }

        let current_state = store
            .view()
            .ok()
            .and_then(|v| v.tickets.get(ticket).map(|t| t.state));
        if current_state == Some(TicketState::Running) {
            if let Err(store_err) = store.submit(ticket, outcome.summary.clone(), evidence, holder)
            {
                tracing::warn!(%ticket, error = %store_err, "submit after successful execution failed");
            }
        } else {
            // The executor already submitted (e.g. via the `ticket.submit` tool); attach any
            // extra evidence this outcome carried instead of re-submitting.
            for artifact in evidence {
                if let Err(store_err) = store.attach_evidence(
                    ticket,
                    EvidenceKind::Diff,
                    &artifact,
                    outcome.summary.clone(),
                    holder.clone(),
                ) {
                    tracing::warn!(%ticket, error = %store_err, "attach_evidence for already-submitted ticket failed");
                }
            }
        }
        return;
    };

    if let Err(store_err) = store.record_failure(ticket, failure.class, failure.detail, holder) {
        tracing::warn!(%ticket, error = %store_err, "record_failure for a classified executor failure also failed");
    }
}
