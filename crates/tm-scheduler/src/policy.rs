//! Scheduler tunables, as explicit fields — not scattered special cases.
//!
//! [`SchedulingPolicy`] is the single value threaded through [`crate::plan::plan`]: everything
//! the pure planner may consult to decide what runs next lives here, including the
//! Ignition-vs-SteadyState relaxations (`SPEC.md` §5 implies a cold-start phase needs looser
//! admission than steady state; rather than branch on "is this early", the planner reads
//! `policy.mode` and `policy.ignition`). Nothing in this module performs I/O or reads a clock.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tm_types::{MilestoneId, Role, Timestamp};

/// Which phase of the project's life the policy should optimize for. The planner is still a
/// pure function of `(view, now, policy)`; `mode` is just another field of `policy`, set by
/// whatever owns the driver (e.g. "first N tickets closed" or "operator flips a switch").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulingMode {
    /// Cold start: few or no tickets have closed yet. [`IgnitionRelaxations`] applies.
    Ignition,
    /// Normal operation: full admission ceilings apply.
    SteadyState,
}

/// Relaxations applied only while [`SchedulingMode::Ignition`] is active, so the project can get
/// moving before enough history exists to trust steady-state ceilings. Every field is an
/// explicit override, not a magic multiplier, so the planner's behavior in this phase is
/// auditable from the struct alone.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IgnitionRelaxations {
    /// Multiplier applied to [`SchedulingPolicy::max_in_flight_per_project`] while igniting.
    /// `1.0` means no relaxation.
    pub max_in_flight_multiplier: f64,
    /// Number of tickets closed at which the planner should stop treating the project as
    /// igniting and the driver should flip `mode` to [`SchedulingMode::SteadyState`]. The
    /// planner itself does not read this counter (it has no notion of "closed so far" beyond
    /// what `SchedulerView` shows); it exists so callers computing the next `policy` value have
    /// one source of truth for the threshold.
    pub graduate_after_closed: u32,
    /// Whether harness-capacity admission ([`crate::admission::AdmissionGate`]) is suspended
    /// during ignition, letting harness tickets run without the steady-state cap.
    pub suspend_harness_cap: bool,
}

impl IgnitionRelaxations {
    /// No relaxation at all: ignition behaves identically to steady state. Useful as a baseline
    /// in tests and for callers that don't want an ignition phase.
    pub fn none() -> Self {
        IgnitionRelaxations {
            max_in_flight_multiplier: 1.0,
            graduate_after_closed: 0,
            suspend_harness_cap: false,
        }
    }
}

/// Relative weights for the deterministic ordering rule in `SPEC.md` §5:
/// priority desc, then critical-path length desc, then milestone deadline proximity, then ticket
/// id asc. The comparator in [`crate::select`] does not sum these into a score (that would admit
/// non-deterministic float ties); they are read in strict lexicographic precedence order. The
/// weights are still carried explicitly so a future scoring-based selector can be swapped in
/// without changing the policy's shape, and so policy diffs are self-documenting.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OrderingWeights {
    /// Precedence of ticket priority. Compared first, descending.
    pub priority: u32,
    /// Precedence of critical-path length. Compared second, descending.
    pub critical_path_length: u32,
    /// Precedence of milestone deadline proximity. Compared third, ascending (soonest first).
    pub milestone_deadline_proximity: u32,
    /// Precedence of ticket id. Always the final, total-order tie-break, ascending.
    pub ticket_id_tiebreak: u32,
}

impl OrderingWeights {
    /// The documented default precedence order from `SPEC.md` §5, expressed as strictly
    /// descending weights so the comparator can sort weights alongside fields if ever needed.
    pub fn spec_default() -> Self {
        OrderingWeights {
            priority: 4,
            critical_path_length: 3,
            milestone_deadline_proximity: 2,
            ticket_id_tiebreak: 1,
        }
    }
}

/// Every tunable [`crate::plan::plan`] may consult. Passed by `&` so callers can share one
/// instance across ticks; `plan` never mutates it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulingPolicy {
    /// How often the driver should tick in the absence of an event notification, in seconds.
    pub tick_interval_seconds: u32,
    /// Default lease TTL granted by [`SchedulerAction::Lease`](crate::plan::SchedulerAction::Lease)
    /// when a ticket does not specify its own.
    pub default_lease_ttl_seconds: u32,
    /// Ceiling on concurrently leased tickets for the whole project.
    pub max_in_flight_per_project: u32,
    /// Ceiling on concurrently leased tickets sharing the same parent ticket.
    pub max_in_flight_per_parent: u32,
    /// Ceiling on concurrently in-flight commands (distinct from leased tickets: a ticket's
    /// execution may issue several commands over its lifetime).
    pub max_concurrent_commands: u32,
    /// The documented selection precedence.
    pub ordering: OrderingWeights,
    /// Which phase the policy currently optimizes for.
    pub mode: SchedulingMode,
    /// Relaxations applied while `mode` is [`SchedulingMode::Ignition`].
    pub ignition: IgnitionRelaxations,
    /// Hard cap, as a fraction in `[0.0, 1.0]`, on the share of total in-flight capacity that
    /// `TicketKind::Harness` work may consume at once (`SPEC.md` §5 admission control). `0.0`
    /// forbids harness work from running concurrently with anything else being capacity-limited;
    /// `1.0` removes the cap.
    pub harness_capacity_fraction: f64,
    /// Milestone deadlines, keyed by milestone id, used for the "milestone deadline proximity"
    /// tie-break in `SPEC.md` §5's ordering. Lives on the policy rather than on
    /// [`tm_core::view::SchedulerView`] because that view deliberately omits milestone data (see
    /// its module docs) to keep `tm-scheduler` from depending on decision/doc machinery it
    /// doesn't otherwise need; the caller assembling `policy` for a tick is expected to project
    /// this from `ProjectView::milestones` (a future addition, not yet a field) once deadlines
    /// exist on `tm_core::milestone::Milestone`. A milestone absent from this map, or a ticket
    /// with no milestone, sorts after every ticket with a known deadline.
    pub milestone_deadlines: BTreeMap<MilestoneId, Timestamp>,
    /// Roles with at least one healthy executor (human or provider) available right now. Lives
    /// on the policy, rather than as a separate parameter to [`crate::plan::plan`], so `plan`'s
    /// signature can stay exactly `(&SchedulerView, Timestamp, &SchedulingPolicy)` as `SPEC.md`
    /// §5 specifies — the caller assembling `policy` for a tick is expected to snapshot this
    /// from `tm-provider`'s `FabricState` (and from human-availability tracking, for
    /// `human_required` tickets) immediately before calling `plan`, the same way it snapshots
    /// `SchedulerView` from `Store`.
    pub available_roles: BTreeSet<Role>,
    /// Project-wide ceiling on the total number of events one ticket's subject may accumulate
    /// before a goal-oriented worker loop must stop regardless of its own `CycleBudget`/step
    /// limit (`SPEC.md` §29, `docs/audit-2026-09-18-fable.md` B-09's "dumb global backstop",
    /// mirroring §21.5's phrase for idempotent effects). Named here so a caller that assembles
    /// both a `SchedulingPolicy` and a `tm_agent::agent_loop::AgentLoop` for the same project can
    /// derive one ceiling and keep the two in agreement; nothing in `tm-scheduler` itself reads
    /// or enforces this field (no ticket, lease or event lives on `SchedulerView`), and no
    /// `Cargo.toml` dependency runs from `tm-agent` to this crate, so the actual enforcement is
    /// `tm_agent::agent_loop::DEFAULT_MAX_EVENTS_PER_TICKET` / `AgentLoop::
    /// with_max_events_per_ticket` — see that constant's doc comment for the same default value
    /// and the arithmetic behind it.
    pub max_events_per_ticket: u32,
    /// Fraction, in `[0.0, 1.0]`, of a ticket's own `Budget` (each dimension's *limit*, not just
    /// what remains) reserved for the verification phase that must follow its work phase
    /// (`SPEC.md` §31.4 "reserve verification budget before dispatching work",
    /// `docs/audit-2026-09-18-fable.md` B-10). [`crate::admission::AdmissionGate::check`] refuses
    /// to admit a ticket whose remaining budget, after this reserve, could not even cover the
    /// reserve itself — a project that spends its last dollars on execution and cannot afford to
    /// verify has produced nothing trustworthy. `0.0` shrinks the reserve to nothing, so only a
    /// dimension already fully exhausted (`0` remaining) still refuses.
    pub verification_budget_reserve_fraction: f64,
}

impl SchedulingPolicy {
    /// A conservative, fully-specified default: steady state, no ignition relaxation, one
    /// eighth of capacity reserved for harness work. Intended as a starting point for tests and
    /// for callers that haven't yet derived project-specific values.
    pub fn conservative_default() -> Self {
        SchedulingPolicy {
            tick_interval_seconds: 5,
            default_lease_ttl_seconds: 300,
            max_in_flight_per_project: 8,
            max_in_flight_per_parent: 3,
            max_concurrent_commands: 16,
            ordering: OrderingWeights::spec_default(),
            mode: SchedulingMode::SteadyState,
            ignition: IgnitionRelaxations::none(),
            harness_capacity_fraction: 0.125,
            milestone_deadlines: BTreeMap::new(),
            available_roles: BTreeSet::new(),
            // Matches `tm_agent::agent_loop::DEFAULT_MAX_EVENTS_PER_TICKET` exactly; see this
            // field's own doc comment for why the two are duplicated rather than shared.
            max_events_per_ticket: 5000,
            // A conservative fifth of a ticket's budget held back for verification, mirroring
            // `harness_capacity_fraction`'s "reserve a slice, don't just hope" shape.
            verification_budget_reserve_fraction: 0.2,
        }
    }

    /// The effective project in-flight ceiling for `now`'s `mode`: `max_in_flight_per_project`
    /// scaled by `ignition.max_in_flight_multiplier` when igniting, floored at 1 so admission
    /// never divides by zero downstream.
    pub fn effective_max_in_flight_per_project(&self) -> u32 {
        match self.mode {
            SchedulingMode::SteadyState => self.max_in_flight_per_project,
            SchedulingMode::Ignition => (((self.max_in_flight_per_project as f64)
                * self.ignition.max_in_flight_multiplier)
                .round() as u32)
                .max(1),
        }
    }
}

impl Default for SchedulingPolicy {
    /// Delegates to [`SchedulingPolicy::conservative_default`], so `SchedulingPolicy::default()`
    /// and `SchedulingPolicy::conservative_default()` are always in sync.
    fn default() -> Self {
        SchedulingPolicy::conservative_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignition_relaxations_none_has_no_relaxation() {
        let relaxations = IgnitionRelaxations::none();
        assert_eq!(relaxations.max_in_flight_multiplier, 1.0);
        assert_eq!(relaxations.graduate_after_closed, 0);
        assert!(!relaxations.suspend_harness_cap);
    }

    #[test]
    fn ordering_weights_spec_default_has_correct_values() {
        let weights = OrderingWeights::spec_default();
        assert_eq!(weights.priority, 4);
        assert_eq!(weights.critical_path_length, 3);
        assert_eq!(weights.milestone_deadline_proximity, 2);
        assert_eq!(weights.ticket_id_tiebreak, 1);
    }

    #[test]
    fn ordering_weights_spec_default_are_strictly_descending() {
        let weights = OrderingWeights::spec_default();
        assert!(weights.priority > weights.critical_path_length);
        assert!(weights.critical_path_length > weights.milestone_deadline_proximity);
        assert!(weights.milestone_deadline_proximity > weights.ticket_id_tiebreak);
    }

    #[test]
    fn scheduling_policy_conservative_default_is_fully_specified() {
        let policy = SchedulingPolicy::conservative_default();
        assert_eq!(policy.tick_interval_seconds, 5);
        assert_eq!(policy.default_lease_ttl_seconds, 300);
        assert_eq!(policy.max_in_flight_per_project, 8);
        assert_eq!(policy.max_in_flight_per_parent, 3);
        assert_eq!(policy.max_concurrent_commands, 16);
        assert_eq!(policy.mode, SchedulingMode::SteadyState);
        assert_eq!(policy.harness_capacity_fraction, 0.125);
        assert!(policy.milestone_deadlines.is_empty());
        assert!(policy.available_roles.is_empty());
        assert_eq!(policy.max_events_per_ticket, 5000);
    }

    #[test]
    fn scheduling_policy_conservative_default_ignition_is_none() {
        let policy = SchedulingPolicy::conservative_default();
        let none = IgnitionRelaxations::none();
        assert_eq!(policy.ignition, none);
    }

    #[test]
    fn scheduling_policy_conservative_default_ordering_is_spec_default() {
        let policy = SchedulingPolicy::conservative_default();
        let spec = OrderingWeights::spec_default();
        assert_eq!(policy.ordering, spec);
    }

    #[test]
    fn effective_max_in_flight_per_project_returns_base_in_steady_state() {
        let policy = SchedulingPolicy {
            mode: SchedulingMode::SteadyState,
            max_in_flight_per_project: 8,
            ignition: IgnitionRelaxations {
                max_in_flight_multiplier: 2.0,
                graduate_after_closed: 0,
                suspend_harness_cap: false,
            },
            ..SchedulingPolicy::conservative_default()
        };
        assert_eq!(policy.effective_max_in_flight_per_project(), 8);
    }

    #[test]
    fn effective_max_in_flight_per_project_scales_by_multiplier_in_ignition() {
        let policy = SchedulingPolicy {
            mode: SchedulingMode::Ignition,
            max_in_flight_per_project: 8,
            ignition: IgnitionRelaxations {
                max_in_flight_multiplier: 2.0,
                graduate_after_closed: 0,
                suspend_harness_cap: false,
            },
            ..SchedulingPolicy::conservative_default()
        };
        assert_eq!(policy.effective_max_in_flight_per_project(), 16);
    }

    #[test]
    fn effective_max_in_flight_per_project_rounds_scaled_value() {
        let policy = SchedulingPolicy {
            mode: SchedulingMode::Ignition,
            max_in_flight_per_project: 3,
            ignition: IgnitionRelaxations {
                max_in_flight_multiplier: 1.5,
                graduate_after_closed: 0,
                suspend_harness_cap: false,
            },
            ..SchedulingPolicy::conservative_default()
        };
        // 3 * 1.5 = 4.5; f64::round ties away from zero, so this rounds up to 5.
        assert_eq!(policy.effective_max_in_flight_per_project(), 5);
    }

    #[test]
    fn effective_max_in_flight_per_project_floors_to_one_in_ignition() {
        let policy = SchedulingPolicy {
            mode: SchedulingMode::Ignition,
            max_in_flight_per_project: 2,
            ignition: IgnitionRelaxations {
                max_in_flight_multiplier: 0.1,
                graduate_after_closed: 0,
                suspend_harness_cap: false,
            },
            ..SchedulingPolicy::conservative_default()
        };
        // 2 * 0.1 = 0.2, rounds to 0, but floors to 1
        assert_eq!(policy.effective_max_in_flight_per_project(), 1);
    }

    #[test]
    fn effective_max_in_flight_per_project_with_zero_base_floors_to_one() {
        let policy = SchedulingPolicy {
            mode: SchedulingMode::Ignition,
            max_in_flight_per_project: 0,
            ignition: IgnitionRelaxations {
                max_in_flight_multiplier: 1.0,
                graduate_after_closed: 0,
                suspend_harness_cap: false,
            },
            ..SchedulingPolicy::conservative_default()
        };
        assert_eq!(policy.effective_max_in_flight_per_project(), 1);
    }

    #[test]
    fn ignition_relaxations_can_be_customized() {
        let relaxations = IgnitionRelaxations {
            max_in_flight_multiplier: 2.5,
            graduate_after_closed: 10,
            suspend_harness_cap: true,
        };
        assert_eq!(relaxations.max_in_flight_multiplier, 2.5);
        assert_eq!(relaxations.graduate_after_closed, 10);
        assert!(relaxations.suspend_harness_cap);
    }

    #[test]
    fn ordering_weights_can_be_customized() {
        let weights = OrderingWeights {
            priority: 10,
            critical_path_length: 5,
            milestone_deadline_proximity: 3,
            ticket_id_tiebreak: 1,
        };
        assert_eq!(weights.priority, 10);
        assert_eq!(weights.critical_path_length, 5);
        assert_eq!(weights.milestone_deadline_proximity, 3);
        assert_eq!(weights.ticket_id_tiebreak, 1);
    }

    #[test]
    fn scheduling_policy_can_be_in_ignition_mode() {
        let policy = SchedulingPolicy {
            mode: SchedulingMode::Ignition,
            ignition: IgnitionRelaxations {
                max_in_flight_multiplier: 1.5,
                graduate_after_closed: 5,
                suspend_harness_cap: true,
            },
            ..SchedulingPolicy::conservative_default()
        };
        assert_eq!(policy.mode, SchedulingMode::Ignition);
        assert_eq!(policy.ignition.graduate_after_closed, 5);
    }

    #[test]
    fn scheduling_policy_can_have_milestone_deadlines() {
        use std::collections::BTreeMap;
        let mut deadlines = BTreeMap::new();
        let milestone_id = MilestoneId::new("M-1".to_string()).unwrap();
        let deadline = Timestamp::from_unix_seconds(1000);
        deadlines.insert(milestone_id.clone(), deadline);

        let policy = SchedulingPolicy {
            milestone_deadlines: deadlines.clone(),
            ..SchedulingPolicy::conservative_default()
        };
        assert_eq!(policy.milestone_deadlines.len(), 1);
        assert_eq!(
            policy.milestone_deadlines.get(&milestone_id),
            Some(&deadline)
        );
    }

    #[test]
    fn scheduling_policy_can_have_available_roles() {
        use std::collections::BTreeSet;
        let mut roles = BTreeSet::new();
        roles.insert(Role::CoderFast);
        roles.insert(Role::AuditorSemantic);

        let policy = SchedulingPolicy {
            available_roles: roles.clone(),
            ..SchedulingPolicy::conservative_default()
        };
        assert_eq!(policy.available_roles.len(), 2);
        assert!(policy.available_roles.contains(&Role::CoderFast));
        assert!(policy.available_roles.contains(&Role::AuditorSemantic));
    }

    #[test]
    fn scheduling_mode_equality() {
        assert_eq!(SchedulingMode::Ignition, SchedulingMode::Ignition);
        assert_eq!(SchedulingMode::SteadyState, SchedulingMode::SteadyState);
        assert_ne!(SchedulingMode::Ignition, SchedulingMode::SteadyState);
    }

    #[test]
    fn harness_capacity_fraction_is_valid_fraction() {
        let policy = SchedulingPolicy::conservative_default();
        assert!(policy.harness_capacity_fraction >= 0.0);
        assert!(policy.harness_capacity_fraction <= 1.0);
    }

    #[test]
    fn harness_capacity_fraction_can_be_zero() {
        let policy = SchedulingPolicy {
            harness_capacity_fraction: 0.0,
            ..SchedulingPolicy::conservative_default()
        };
        assert_eq!(policy.harness_capacity_fraction, 0.0);
    }

    #[test]
    fn harness_capacity_fraction_can_be_one() {
        let policy = SchedulingPolicy {
            harness_capacity_fraction: 1.0,
            ..SchedulingPolicy::conservative_default()
        };
        assert_eq!(policy.harness_capacity_fraction, 1.0);
    }

    #[test]
    fn effective_max_in_flight_per_project_with_high_multiplier() {
        let policy = SchedulingPolicy {
            mode: SchedulingMode::Ignition,
            max_in_flight_per_project: 8,
            ignition: IgnitionRelaxations {
                max_in_flight_multiplier: 3.0,
                graduate_after_closed: 0,
                suspend_harness_cap: false,
            },
            ..SchedulingPolicy::conservative_default()
        };
        // 8 * 3.0 = 24.0, rounds to 24
        assert_eq!(policy.effective_max_in_flight_per_project(), 24);
    }

    #[test]
    fn effective_max_in_flight_per_project_rounding_down() {
        let policy = SchedulingPolicy {
            mode: SchedulingMode::Ignition,
            max_in_flight_per_project: 7,
            ignition: IgnitionRelaxations {
                max_in_flight_multiplier: 1.3,
                graduate_after_closed: 0,
                suspend_harness_cap: false,
            },
            ..SchedulingPolicy::conservative_default()
        };
        // 7 * 1.3 = 9.1, rounds to 9
        assert_eq!(policy.effective_max_in_flight_per_project(), 9);
    }

    #[test]
    fn clone_scheduling_policy() {
        let policy = SchedulingPolicy::conservative_default();
        let cloned = policy.clone();
        assert_eq!(policy, cloned);
    }

    #[test]
    fn clone_scheduling_mode() {
        let mode = SchedulingMode::Ignition;
        let cloned = mode;
        assert_eq!(mode, cloned);
    }
}
