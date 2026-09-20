//! `SchedulerLoop`: the thin, I/O-touching shell around the pure planner.
//!
//! Everything decision-making happens in [`crate::plan::plan`]; this module only turns those
//! decisions into `tm_core::Store` calls (each already transactional, per
//! `Store::run_command`) and reports what happened as a flat, ordered event sequence. Determinism
//! here means: given the same injected `Clock` and the same sequence of `tick()` calls against
//! the same `Store` state, `SchedulerLoop` emits the same [`SchedulerLoopEvent`] sequence every
//! time — which is exactly what lets tests drive it by advancing a `tm_types::FixedClock` and
//! asserting exact output, instead of racing a real clock.

use std::sync::Arc;

use tm_core::store::Store;
use tm_core::ticket::Trigger;
use tm_events::Event;
use tm_types::{Clock, ParticipantId, Timestamp};

use crate::dispatch::ExecutorDispatcher;
use crate::plan::SchedulerAction;
use crate::policy::SchedulingPolicy;

/// One observable outcome of a [`SchedulerLoop::tick`] call, in emission order: exactly one
/// [`SchedulerLoopEvent::Ticked`] first, then one `Applied`/`Failed` per action `plan()`
/// returned, in `plan()`'s own deterministic order.
#[derive(Debug, Clone, PartialEq)]
pub enum SchedulerLoopEvent {
    /// The tick ran: `plan()` was called at `at` and returned `planned` actions.
    Ticked {
        /// The timestamp `plan()` was called with.
        at: Timestamp,
        /// `plan()`'s output length, before any action was applied.
        planned: usize,
    },
    /// `action` was applied successfully, producing these `tm-core` events.
    Applied {
        /// The action that was applied.
        action: SchedulerAction,
        /// The events `Store` emitted for it.
        events: Vec<Event>,
    },
    /// `action` failed to apply; `plan()` had judged it valid against the snapshot it read, but
    /// `Store` rejected it (e.g. state changed between the snapshot and the apply, another actor
    /// raced the same ticket). Not a panic path: the next tick's fresh snapshot will replan
    /// around the new state.
    Failed {
        /// The action that failed.
        action: SchedulerAction,
        /// `Store`'s rejection, rendered via `Display`.
        error: String,
    },
}

/// Applies [`plan`]'s output against a [`Store`], one tick at a time.
///
/// Holds the same `Arc<dyn Clock>` the caller passed to whichever `Store::open_with` produced
/// `store` — `SchedulerLoop` does not construct its own clock, so its notion of `now` and the
/// event log's timestamps can never diverge. In production that `Clock` is a `SystemClock`; in
/// tests it is a `FixedClock` the test advances explicitly between `tick()` calls.
pub struct SchedulerLoop<'a> {
    store: &'a Store,
    clock: Arc<dyn Clock>,
    policy: SchedulingPolicy,
    /// When set, `SchedulerAction::Lease` is routed through a real [`ExecutorDispatcher`]
    /// instead of leasing to a placeholder holder id. `None` (the default) keeps the exact
    /// pre-B-01 behaviour, which existing tests (and a bare `tm sched tick` with no executors
    /// configured) rely on staying deterministic and dispatcher-free.
    dispatcher: Option<Arc<ExecutorDispatcher>>,
}

impl<'a> SchedulerLoop<'a> {
    /// Build a loop over `store`, ticking with `clock` and planning under `policy`. No
    /// dispatcher is attached; see [`Self::with_dispatcher`].
    pub fn new(store: &'a Store, clock: Arc<dyn Clock>, policy: SchedulingPolicy) -> Self {
        SchedulerLoop {
            store,
            clock,
            policy,
            dispatcher: None,
        }
    }

    /// Route `SchedulerAction::Lease` through `dispatcher` instead of the placeholder-holder
    /// fallback (`SPEC.md` §24, audit B-01/B-04).
    pub fn with_dispatcher(mut self, dispatcher: Arc<ExecutorDispatcher>) -> Self {
        self.dispatcher = Some(dispatcher);
        self
    }

    /// The policy currently in effect.
    pub fn policy(&self) -> &SchedulingPolicy {
        &self.policy
    }

    /// Replace the policy used by subsequent ticks (e.g. the caller graduating
    /// `SchedulingMode::Ignition` to `SteadyState`). Takes effect on the next [`Self::tick`];
    /// never mutates a tick already in progress.
    pub fn set_policy(&mut self, policy: SchedulingPolicy) {
        self.policy = policy;
    }

    /// Run one tick: snapshot `store`'s [`tm_core::view::SchedulerView`], call [`plan`] at the
    /// clock's current time, and apply every returned action in order, acting as `actor`.
    /// Intended to be called both on [`SchedulingPolicy::tick_interval_seconds`] and on an
    /// external event notification — both call sites are this same method; the interval/
    /// notification plumbing itself lives in the caller (`tm-agent`/`tm-server`), since it is
    /// ordinary async orchestration, not scheduling logic.
    ///
    /// # Errors
    /// Only for failure to read the snapshot itself ([`Store::scheduler_view`]'s error). A
    /// per-action failure does not abort the tick; it is reported as
    /// [`SchedulerLoopEvent::Failed`] and the loop continues to the next action.
    pub fn tick(&self, actor: ParticipantId) -> tm_types::Result<Vec<SchedulerLoopEvent>> {
        let view = self.store.scheduler_view()?;
        let now = self.clock.now();
        let actions = crate::plan::plan(&view, now, &self.policy);

        let mut out = Vec::with_capacity(actions.len() + 1);
        out.push(SchedulerLoopEvent::Ticked {
            at: now,
            planned: actions.len(),
        });
        for action in actions {
            match self.apply_action(&action, &actor) {
                Ok(events) => out.push(SchedulerLoopEvent::Applied { action, events }),
                Err(err) => out.push(SchedulerLoopEvent::Failed {
                    action,
                    error: err.to_string(),
                }),
            }
        }
        Ok(out)
    }

    /// Map one [`SchedulerAction`] to its `Store` call.
    fn apply_action(
        &self,
        action: &SchedulerAction,
        actor: &ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        match action {
            SchedulerAction::MarkReady(ticket) => {
                self.store
                    .transition(ticket, Trigger::DependenciesSatisfied, actor.clone())
            }
            SchedulerAction::MarkBlocked(ticket) => {
                self.store
                    .transition(ticket, Trigger::DependenciesUnsatisfied, actor.clone())
            }
            SchedulerAction::Lease {
                ticket,
                executor,
                ttl_seconds,
            } => {
                let view = self.store.view()?;
                let t = view
                    .tickets
                    .get(ticket)
                    .ok_or_else(|| tm_types::TmError::not_found("ticket", ticket))?;
                if let Some(dispatcher) = &self.dispatcher {
                    // Real executor resolution, capability matching, and hand-off to a
                    // background run: see `crate::dispatch::ExecutorDispatcher`.
                    dispatcher.dispatch(ticket, t, *ttl_seconds, actor.clone())
                } else {
                    // No dispatcher attached: keep the pre-B-01 deterministic-placeholder
                    // behaviour (still useful for `tm sched tick`/tests that only want to
                    // observe lease bookkeeping, not actually run anything).
                    let holder =
                        ParticipantId::new(format!("agent:{}/{}", executor.as_str(), ticket))?;
                    self.store.acquire_lease(
                        ticket,
                        holder,
                        t.authority.clone(),
                        t.resources.clone(),
                        *ttl_seconds,
                        actor.clone(),
                    )
                }
            }
            SchedulerAction::ExpireLease(_lease) => {
                // The attempt was already spent at lease acquisition (tm-core), so expiry only
                // reverts state here. Counting it again in the driver would both double-charge
                // the ticket and leave expiries swept by any other caller uncounted.
                let events = self.store.expire_leases()?;
                Ok(events)
            }
            SchedulerAction::Retry { ticket, after: _ } => {
                self.store
                    .transition(ticket, Trigger::RetryScheduled, actor.clone())
            }
            SchedulerAction::Escalate { ticket, reason: _ } => {
                // `plan()`'s phase 3 only escalates a ticket already in `TicketState::Recovery`,
                // whose failure was recorded by whichever `record_failure` call put it there;
                // escalating here is just driving the already-decided transition, not recording
                // a second failure.
                self.store
                    .transition(ticket, Trigger::RetryExhausted, actor.clone())
            }
            SchedulerAction::OpenRecovery(ticket) => {
                self.store
                    .transition(ticket, Trigger::RetryScheduled, actor.clone())
            }
            SchedulerAction::CloseMilestone(milestone) => {
                self.store.close_milestone(milestone, actor.clone())
            }
            SchedulerAction::Noop => Ok(vec![]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use tempfile::TempDir;
    use tm_core::ticket::{
        ExecutorRequirements, RetryPolicy, TicketKind, TicketState, VerificationPolicy,
    };
    use tm_types::{Authority, Budget, CounterIds, FixedClock, Role, Tolerance};

    use crate::policy::SchedulingPolicy;

    fn executor() -> ExecutorRequirements {
        ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Any,
        }
    }

    fn retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 60,
        }
    }

    fn open_store(clock: Arc<dyn Clock>) -> (TempDir, Store) {
        let dir = TempDir::new().expect("tempdir");
        let ids: Arc<dyn tm_types::IdSource> = Arc::new(CounterIds::new());
        let store = Store::open_with(dir.path(), clock, ids).expect("open store");
        (dir, store)
    }

    fn create_ready_ticket(store: &Store) -> tm_types::TicketId {
        let events = store
            .create_ticket(
                TicketKind::Work,
                "do the thing".into(),
                None,
                None,
                Authority::root(),
                vec![],
                executor(),
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::unlimited(),
                retry(),
                0,
                ParticipantId::system(),
            )
            .expect("create_ticket should succeed");
        let ticket = tm_types::TicketId::new(events[0].subject.as_str()).expect("ticket id");
        store
            .activate(&ticket, ParticipantId::system())
            .expect("activate should succeed");
        ticket
    }

    #[test]
    fn crash_recovery_expired_lease_returns_ticket_to_ready_with_attempt_counted() {
        let fixed = Arc::new(FixedClock::epoch());
        let clock: Arc<dyn Clock> = fixed.clone();
        let (_dir, store) = open_store(clock.clone());
        let ticket = create_ready_ticket(&store);

        let holder = ParticipantId::new("agent:coder.fast/worker-1").expect("valid participant");
        store
            .acquire_lease(
                &ticket,
                holder,
                Authority::root(),
                vec![],
                30,
                ParticipantId::system(),
            )
            .expect("acquire_lease should succeed");

        fixed.advance_seconds(31);

        let policy = SchedulingPolicy::conservative_default();
        let scheduler = SchedulerLoop::new(&store, clock, policy);
        let events = scheduler.tick(ParticipantId::system()).expect("tick");

        let expired = events.iter().any(|e| {
            matches!(
                e,
                SchedulerLoopEvent::Applied {
                    action: SchedulerAction::ExpireLease(_),
                    ..
                }
            )
        });
        assert!(expired, "expected an Applied ExpireLease event: {events:?}");

        let view = store.view().expect("view");
        let t = view.tickets.get(&ticket).expect("ticket exists");
        assert_eq!(t.state, TicketState::Ready);
        assert_eq!(t.attempts, 1);
    }

    #[test]
    fn tick_sequence_under_fixed_clock_is_deterministic() {
        let fixed_a = Arc::new(FixedClock::epoch());
        let clock_a: Arc<dyn Clock> = fixed_a.clone();
        let (_dir_a, store_a) = open_store(clock_a.clone());
        create_ready_ticket(&store_a);

        let fixed_b = Arc::new(FixedClock::epoch());
        let clock_b: Arc<dyn Clock> = fixed_b.clone();
        let (_dir_b, store_b) = open_store(clock_b.clone());
        create_ready_ticket(&store_b);

        let policy_a = SchedulingPolicy::conservative_default();
        let policy_b = SchedulingPolicy::conservative_default();
        let scheduler_a = SchedulerLoop::new(&store_a, clock_a, policy_a);
        let scheduler_b = SchedulerLoop::new(&store_b, clock_b, policy_b);

        let mut events_a = Vec::new();
        let mut events_b = Vec::new();
        for step in [0u32, 5, 10] {
            fixed_a.advance_seconds(step as i64);
            fixed_b.advance_seconds(step as i64);
            events_a.extend(scheduler_a.tick(ParticipantId::system()).expect("tick a"));
            events_b.extend(scheduler_b.tick(ParticipantId::system()).expect("tick b"));
        }

        assert_eq!(events_a, events_b);
    }

    /// A recording [`tm_core::Executor`] double: remembers the [`tm_core::ExecutorTask`] it was
    /// called with and signals `done` once `execute` runs, so a test can `await` completion of
    /// the dispatcher's background task deterministically instead of sleeping.
    struct RecordingExecutor {
        id: String,
        seen: std::sync::Mutex<Option<tm_core::ExecutorTask>>,
        done: tokio::sync::Notify,
    }

    impl RecordingExecutor {
        fn new(id: &str) -> Arc<Self> {
            Arc::new(RecordingExecutor {
                id: id.to_string(),
                seen: std::sync::Mutex::new(None),
                done: tokio::sync::Notify::new(),
            })
        }
    }

    #[async_trait::async_trait]
    impl tm_core::Executor for RecordingExecutor {
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

        async fn execute(
            &self,
            task: tm_core::ExecutorTask,
        ) -> tm_types::Result<tm_core::ExecutorOutcome> {
            *self.seen.lock().unwrap() = Some(task.clone());
            let outcome = tm_core::ExecutorOutcome {
                ticket: task.ticket.clone(),
                summary: "recorded".to_string(),
                evidence: vec![],
                patch: None,
                usage: tm_types::Spend::default(),
                decisions: vec![],
                failure: None,
            };
            self.done.notify_one();
            Ok(outcome)
        }

        async fn cancel(&self, _handle: &tm_core::ExecutionHandle) -> tm_types::Result<()> {
            Ok(())
        }
    }

    /// Always compiles to the same fixed pack text; the test does not care about pack content.
    struct FixedContextPack;

    impl crate::dispatch::ContextPackSource for FixedContextPack {
        fn compile(&self, _ticket: &tm_types::TicketId) -> tm_types::Result<String> {
            Ok("<compiled pack>".to_string())
        }
    }

    fn restricted_authority() -> Authority {
        Authority {
            repository: tm_types::RepoAuthority {
                read: tm_types::PatternSet::parse(["src/**"]).unwrap(),
                write: tm_types::PatternSet::parse(["src/only/**"]).unwrap(),
            },
            ..Authority::none()
        }
    }

    fn create_ready_ticket_with_authority(
        store: &Store,
        authority: Authority,
    ) -> tm_types::TicketId {
        let events = store
            .create_ticket(
                TicketKind::Work,
                "do the restricted thing".into(),
                None,
                None,
                authority,
                vec![],
                executor(),
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::unlimited(),
                retry(),
                0,
                ParticipantId::system(),
            )
            .expect("create_ticket should succeed");
        let ticket = tm_types::TicketId::new(events[0].subject.as_str()).expect("ticket id");
        store
            .activate(&ticket, ParticipantId::system())
            .expect("activate should succeed");
        ticket
    }

    #[tokio::test]
    async fn lease_action_dispatches_to_a_real_executor_carrying_the_tickets_authority() {
        let fixed = Arc::new(FixedClock::epoch());
        let clock: Arc<dyn Clock> = fixed.clone();
        let (_dir, store) = open_store(clock.clone());
        let store = Arc::new(store);
        let authority = restricted_authority();
        let ticket = create_ready_ticket_with_authority(&store, authority.clone());

        let recorder = RecordingExecutor::new("recording-exec");
        let mut registry = crate::dispatch::ExecutorRegistry::new(recorder.clone());
        registry.register(Role::CoderFast, recorder.clone());
        let dispatcher = Arc::new(crate::dispatch::ExecutorDispatcher::new(
            store.clone(),
            tokio::runtime::Handle::current(),
            Arc::new(FixedContextPack),
            registry,
            None,
        ));

        let mut policy = SchedulingPolicy::conservative_default();
        policy.available_roles = [Role::CoderFast].into_iter().collect();
        let scheduler =
            SchedulerLoop::new(&store, clock, policy).with_dispatcher(dispatcher.clone());
        let events = scheduler
            .tick(ParticipantId::system())
            .expect("tick should succeed");

        let leased = events.iter().any(|e| {
            matches!(
                e,
                SchedulerLoopEvent::Applied {
                    action: SchedulerAction::Lease { .. },
                    ..
                }
            )
        });
        assert!(leased, "expected an Applied Lease event: {events:?}");

        // The lease holder is the real executor's own id, never the pre-B-01 placeholder shape.
        let view = store.view().expect("view");
        let lease = view
            .leases
            .values()
            .find(|l| l.ticket == ticket)
            .expect("a lease exists for the ticket");
        assert_eq!(
            lease.holder.as_str(),
            format!("agent:recording-exec/{ticket}")
        );
        assert_ne!(
            lease.holder.as_str(),
            format!("agent:{}/{ticket}", Role::CoderFast.as_str()),
            "holder must not be the deterministic placeholder shape"
        );

        // Wait for the background run to actually happen.
        tokio::time::timeout(std::time::Duration::from_secs(5), recorder.done.notified())
            .await
            .expect("executor.execute should have been called");

        let seen = recorder.seen.lock().unwrap().clone().expect("task seen");
        assert_eq!(seen.ticket, ticket);
        assert_eq!(
            seen.authority, authority,
            "the executor must receive the ticket's own attenuated authority, not root()"
        );
        assert_ne!(
            seen.authority,
            Authority::root(),
            "a restricted ticket must never reach the executor with root authority"
        );
    }
}
