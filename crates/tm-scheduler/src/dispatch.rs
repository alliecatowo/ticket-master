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
use tm_types::{LeaseId, ParticipantId, Predicate, Role, TicketId};

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
    #[error("no worker is set up for role {0}")]
    NoExecutorForRole(Role),
    /// A candidate executor's declared capabilities do not satisfy the ticket's requirements.
    #[error("no worker can handle this ticket: {0}")]
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
    let harness_epoch = match store.harness_epochs() {
        Ok(epochs) => epochs.last().map_or(0, |epoch| epoch.epoch),
        Err(error) => {
            tracing::warn!(%ticket, error = %error, "could not read promoted harness epoch; using genesis epoch");
            0
        }
    };
    let pack = match context.compile(&ticket) {
        Ok(pack) => pack,
        Err(e) => {
            if let Err(store_err) = store.record_failure(
                &ticket,
                FailureClass::Other,
                format!("couldn't put together context for this ticket: {e}"),
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
        harness_epoch,
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
                    format!("the change touched files outside what this ticket is allowed to write: {violation}"),
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
                        format!("couldn't save the patch as evidence: {e}"),
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
                "the worker said it finished but left no evidence (patch, summary, etc.)"
                    .to_string(),
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

        // Either branch above may have left the ticket `Submitted` (the common case, since
        // `tm-agent`'s `BuiltinExecutor` submits mid-run via its own `ticket.submit` tool and
        // takes the `else` arm here, not the `store.submit` one). Run the ticket's own automatic
        // verification commands now, still as part of this same report, rather than leaving
        // "did the check ever run" to a separate poller.
        let post_state = store
            .view()
            .ok()
            .and_then(|v| v.tickets.get(ticket).map(|t| t.state));
        if post_state == Some(TicketState::Submitted) {
            run_automatic_verification(store, ticket, t, repo_root).await;
        }
        return;
    };

    if let Err(store_err) = store.record_failure(ticket, failure.class, failure.detail, holder) {
        tracing::warn!(%ticket, error = %store_err, "record_failure for a classified executor failure also failed");
    }
}

/// The argv of every [`Predicate::CommandSucceeds`] leaf named anywhere in `t.success`
/// (including nested under `AllOf`/`AnyOf`/`Not` — [`Predicate::walk`] finds them regardless of
/// nesting). This is the "first slice" of `u1-automatic-verification-step`: `Predicate::TestsPass`
/// and the other machine-checkable leaves, and a project-wide `harness.toml` `verify_command`,
/// are later work (see that task's own note in `docs/tasks/TASKS.md`).
fn verification_commands(t: &Ticket) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    for p in &t.success {
        p.walk(&mut |leaf| {
            if let Predicate::CommandSucceeds { command } = leaf {
                commands.push(command.clone());
            }
        });
    }
    commands
}

/// After a submit leaves `ticket` in `Submitted`, run every command
/// [`verification_commands`] finds in `t.success`, directly (never through a shell), in
/// `repo_root`, as [`ParticipantId::system`] (`SPEC.md:721-724`'s "verification separation" —
/// this is deterministic machinery, not a worker certifying its own work). A pass is recorded as
/// [`EvidenceKind::CommandOutput`] evidence and moves a passing ticket through `Verifying` into
/// `Auditing`; a failure is routed through
/// [`Store::fail_automatic_verification`], which sends the ticket back through the same
/// `Submitted -> Verifying -> Recovery -> retry` path a human's `tm ticket reject` uses.
///
/// Does nothing (past a `tracing::debug!`) when `t.success` names no `CommandSucceeds` predicate,
/// or when `repo_root` is `None` — there is no checked-out workspace to run a command in (e.g. a
/// test harness with no real git working tree).
///
/// Until separate verification tickets exist, deterministic system machinery records the
/// verification against the work ticket itself. The subsequent human audit remains distinct from
/// the worker that submitted it.
async fn run_automatic_verification(
    store: &Store,
    ticket: &TicketId,
    t: &Ticket,
    repo_root: Option<&std::path::Path>,
) {
    let commands = verification_commands(t);
    if commands.is_empty() {
        return;
    }
    let Some(root) = repo_root else {
        tracing::debug!(
            %ticket,
            "ticket names automatic verification commands but no repo root is configured; skipping"
        );
        return;
    };

    let mut all_passed = true;
    let mut transcript = String::new();
    for command in &commands {
        let Some((program, args)) = command.split_first() else {
            continue;
        };
        let rendered = command.join(" ");
        match tokio::process::Command::new(program)
            .args(args)
            .current_dir(root)
            .output()
            .await
        {
            Ok(output) => {
                all_passed &= output.status.success();
                transcript.push_str(&format!(
                    "$ {rendered}\nexit code: {}\n{}{}\n",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                ));
            }
            Err(e) => {
                all_passed = false;
                transcript.push_str(&format!("$ {rendered}\ncould not run this check: {e}\n"));
            }
        }
    }

    let artifact_id = match store.store_artifact(
        ArtifactKind::CommandOutput,
        "text/plain".to_string(),
        transcript.clone().into_bytes(),
        serde_json::json!({ "commands": commands, "passed": all_passed }),
        Some(ticket.clone()),
        ParticipantId::system(),
    ) {
        Ok(events) => events
            .iter()
            .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone())),
        Err(e) => {
            tracing::warn!(%ticket, error = %e, "couldn't store automatic verification output as an artifact");
            None
        }
    };

    if let Some(id) = &artifact_id {
        if let Err(e) = store.attach_evidence(
            ticket,
            EvidenceKind::CommandOutput,
            id,
            format!(
                "checks {}: {}",
                if all_passed { "passed" } else { "failed" },
                commands
                    .iter()
                    .map(|c| c.join(" "))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            ParticipantId::system(),
        ) {
            tracing::warn!(%ticket, error = %e, "couldn't attach automatic verification evidence");
        }
    }

    if all_passed {
        if let Err(e) = store.transition(
            ticket,
            tm_core::Trigger::VerificationStarted,
            ParticipantId::system(),
        ) {
            tracing::warn!(%ticket, error = %e, "couldn't start automatic verification");
            return;
        }
        if let Err(e) = store.verify(ticket, ticket, true, None, ParticipantId::system()) {
            tracing::warn!(%ticket, error = %e, "couldn't record automatic verification pass");
        }
        return;
    }

    let tail: String = transcript
        .chars()
        .rev()
        .take(800)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let first_command = commands.first().map(|c| c.join(" ")).unwrap_or_default();
    let reason = format!("The check `{first_command}` failed:\n{tail}");
    if let Err(e) = store.fail_automatic_verification(ticket, reason) {
        tracing::warn!(%ticket, error = %e, "couldn't send the ticket back to retry after a failed automatic verification");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;
    use tm_core::ticket::{ExecutorRequirements, RetryPolicy, TicketKind, TicketState};
    use tm_types::{Authority, Budget, Clock, CounterIds, FixedClock, Spend, Tolerance};

    fn executor_reqs() -> ExecutorRequirements {
        ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Any,
        }
    }

    fn retry_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 60,
        }
    }

    fn open_store(clock: Arc<dyn Clock>) -> (TempDir, Arc<Store>) {
        let dir = TempDir::new().expect("tempdir");
        let ids: Arc<dyn tm_types::IdSource> = Arc::new(CounterIds::new());
        let store = Store::open_with(dir.path(), clock, ids).expect("open store");
        (dir, Arc::new(store))
    }

    /// A ready `Work` ticket naming `success` as its verification commands.
    fn create_ready_ticket_with_success(store: &Store, success: Vec<Predicate>) -> TicketId {
        let events = store
            .create_ticket(
                TicketKind::Work,
                "do the thing, then verify it".into(),
                None,
                None,
                Authority::root(),
                vec![],
                executor_reqs(),
                vec![],
                success,
                tm_core::ticket::VerificationPolicy::Single,
                Budget::unlimited(),
                retry_policy(),
                0,
                ParticipantId::system(),
            )
            .expect("create_ticket should succeed");
        let ticket = TicketId::new(events[0].subject.as_str()).expect("ticket id");
        store
            .activate(&ticket, ParticipantId::system())
            .expect("activate should succeed");
        ticket
    }

    /// An [`Executor`] double that always reports success, citing one evidence artifact it
    /// stores itself first — mirroring what a real worker's `ticket.submit` evidence looks like.
    /// When `submits_mid_run` is `false` (the default via [`SucceedingExecutor::new`]),
    /// `report_outcome` reaches its `store.submit` path, the case a future external-harness
    /// adapter would hit. When `true`, `execute` itself calls `Store::submit` first — mirroring
    /// `tm-agent`'s `BuiltinExecutor`, which submits mid-run via its own `ticket.submit` tool —
    /// so `report_outcome` takes its `else` (already-submitted) branch instead, the path nearly
    /// every real run actually takes.
    struct SucceedingExecutor {
        id: String,
        store: Arc<Store>,
        submits_mid_run: bool,
    }

    impl SucceedingExecutor {
        fn new(id: &str, store: Arc<Store>) -> Arc<Self> {
            Arc::new(SucceedingExecutor {
                id: id.to_string(),
                store,
                submits_mid_run: false,
            })
        }

        fn new_submitting_mid_run(id: &str, store: Arc<Store>) -> Arc<Self> {
            Arc::new(SucceedingExecutor {
                id: id.to_string(),
                store,
                submits_mid_run: true,
            })
        }
    }

    #[async_trait::async_trait]
    impl Executor for SucceedingExecutor {
        fn id(&self) -> &str {
            &self.id
        }

        fn capabilities(&self) -> tm_core::ExecutorCapabilities {
            tm_core::ExecutorCapabilities {
                streaming: false,
                tool_use: true,
                patch_output: false,
                interactive: false,
                accepts_context_pack: true,
                sandboxed: true,
                max_context_tokens: None,
                cost_class: tm_core::CostClass::Cheap,
            }
        }

        async fn execute(&self, task: ExecutorTask) -> tm_types::Result<ExecutorOutcome> {
            let events = self
                .store
                .store_artifact(
                    ArtifactKind::Report,
                    "text/plain".to_string(),
                    b"work done".to_vec(),
                    serde_json::json!({}),
                    Some(task.ticket.clone()),
                    ParticipantId::system(),
                )
                .expect("store_artifact should succeed");
            let artifact = events
                .iter()
                .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
                .expect("artifact_created event");
            if self.submits_mid_run {
                self.store
                    .submit(
                        &task.ticket,
                        "done (submitted mid-run)".to_string(),
                        vec![artifact.clone()],
                        task.actor.clone(),
                    )
                    .expect("mid-run submit should succeed");
            }
            Ok(ExecutorOutcome {
                ticket: task.ticket.clone(),
                summary: "done".to_string(),
                evidence: vec![artifact],
                patch: None,
                usage: Spend::default(),
                decisions: vec![],
                failure: None,
            })
        }

        async fn cancel(&self, _handle: &tm_core::ExecutionHandle) -> tm_types::Result<()> {
            Ok(())
        }
    }

    /// Always compiles to the same fixed pack text; these tests do not care about pack content.
    struct FixedContextPack;

    impl ContextPackSource for FixedContextPack {
        fn compile(&self, _ticket: &TicketId) -> tm_types::Result<String> {
            Ok("<compiled pack>".to_string())
        }
    }

    /// Lease and dispatch `ticket` to `executor`, and wait for `report_outcome` — including
    /// automatic verification — to fully finish, using
    /// [`ExecutorDispatcher::dispatch_with_completion`] rather than a fixed sleep.
    async fn dispatch_and_wait(
        store: Arc<Store>,
        ticket: &TicketId,
        repo_root: Option<PathBuf>,
        executor: Arc<SucceedingExecutor>,
    ) {
        let mut registry = ExecutorRegistry::new(executor.clone());
        registry.register(Role::CoderFast, executor.clone());
        let dispatcher = ExecutorDispatcher::new(
            store.clone(),
            tokio::runtime::Handle::current(),
            Arc::new(FixedContextPack),
            registry,
            repo_root,
        );
        let t = store
            .view()
            .expect("view")
            .tickets
            .get(ticket)
            .expect("ticket exists")
            .clone();
        let (_events, rx) = dispatcher
            .dispatch_with_completion(ticket, &t, 60, ParticipantId::system())
            .expect("dispatch should succeed");
        tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("the run should finish within the timeout")
            .expect("the completion channel should not be dropped");
    }

    #[tokio::test]
    async fn a_passing_verification_command_moves_the_ticket_to_auditing_and_accept_works() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let (dir, store) = open_store(clock);
        let ticket = create_ready_ticket_with_success(
            &store,
            vec![Predicate::CommandSucceeds {
                command: vec!["true".to_string()],
            }],
        );

        let executor = SucceedingExecutor::new_submitting_mid_run("verifying-exec", store.clone());
        dispatch_and_wait(
            store.clone(),
            &ticket,
            Some(dir.path().to_path_buf()),
            executor,
        )
        .await;

        let view = store.view().expect("view");
        let t = view.tickets.get(&ticket).expect("ticket exists");
        assert_eq!(
            t.state,
            TicketState::Auditing,
            "a passing automatic check must move the ticket through verification"
        );
        let verification_artifact = view
            .artifacts
            .values()
            .find(|a| a.kind == ArtifactKind::CommandOutput)
            .expect("automatic verification must have stored a CommandOutput artifact");
        assert_eq!(
            verification_artifact.meta.get("passed"),
            Some(&serde_json::Value::Bool(true)),
            "the artifact's meta should record that the check passed: {:?}",
            verification_artifact.meta
        );
        drop(view);

        let human = ParticipantId::new("human:tester").expect("valid participant");
        store
            .accept(&ticket, None, human)
            .expect("tm ticket accept must still work from Auditing after a passing check");
        let view = store.view().expect("view");
        assert_eq!(
            view.tickets.get(&ticket).expect("ticket exists").state,
            TicketState::Closed
        );
    }

    #[tokio::test]
    async fn a_failing_verification_command_rejects_the_ticket_back_onto_the_retry_path() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let (dir, store) = open_store(clock);
        let ticket = create_ready_ticket_with_success(
            &store,
            vec![Predicate::CommandSucceeds {
                command: vec!["false".to_string()],
            }],
        );

        let executor = SucceedingExecutor::new_submitting_mid_run("verifying-exec", store.clone());
        dispatch_and_wait(
            store.clone(),
            &ticket,
            Some(dir.path().to_path_buf()),
            executor,
        )
        .await;

        let view = store.view().expect("view");
        let t = view.tickets.get(&ticket).expect("ticket exists");
        assert_eq!(
            t.state,
            TicketState::Ready,
            "a failing automatic check must send the ticket back onto the retry path, as system; got {:?}",
            t.state
        );
        assert_eq!(t.attempts, 1);
        let last_failure = t.failures.last().expect("a failure record was appended");
        assert_eq!(last_failure.class, FailureClass::VerificationFailed);
        assert!(
            last_failure.detail.contains("The check `false` failed"),
            "the failure detail should name the failing command and carry its output: {}",
            last_failure.detail
        );
        let verification_artifact = view
            .artifacts
            .values()
            .find(|a| a.kind == ArtifactKind::CommandOutput)
            .expect(
                "automatic verification must have stored a CommandOutput artifact even on failure",
            );
        assert_eq!(
            verification_artifact.meta.get("passed"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[tokio::test]
    async fn no_success_predicates_leaves_verification_untouched() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let (dir, store) = open_store(clock);
        let ticket = create_ready_ticket_with_success(&store, vec![]);

        let executor = SucceedingExecutor::new("non-verifying-exec", store.clone());
        dispatch_and_wait(
            store.clone(),
            &ticket,
            Some(dir.path().to_path_buf()),
            executor,
        )
        .await;

        let view = store.view().expect("view");
        let t = view.tickets.get(&ticket).expect("ticket exists");
        assert_eq!(
            t.state,
            TicketState::Submitted,
            "a ticket naming no CommandSucceeds predicate should submit exactly as before"
        );
    }
}
