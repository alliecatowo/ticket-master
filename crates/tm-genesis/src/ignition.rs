//! Ignition: a project mode, expressed entirely as explicit policy fields.
//!
//! `SPEC.md` §12 is emphatic that Ignition is a *policy*, not a separate code path: every
//! relaxation Genesis grants a freshly-bootstrapped project lives on [`IgnitionPolicy`] as a
//! named field, so nothing about "early project behaves differently" is hidden in a conditional
//! buried elsewhere in the system. [`crate::maturity`] is what eventually swaps an
//! `IgnitionPolicy` for a [`SteadyStatePolicy`] once the project has proven itself.

use serde::{Deserialize, Serialize};
use tm_types::{Authority, MilestoneId, Tolerance};

/// Every relaxation Genesis grants while a project is igniting, as explicit fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IgnitionPolicy {
    /// The authority granted by default to a newly-created ticket during ignition, broader than
    /// the steady-state default (see [`SteadyStatePolicy::default_authority`]).
    pub default_authority: Authority,
    /// When true, planning tickets (spawning/reshaping children) and implementation tickets may
    /// run concurrently rather than requiring planning to fully settle first.
    pub interleave_planning_and_implementation: bool,
    /// When true, a ticket's mutable fields (see `tm_core::Store::update_ticket`) may change
    /// without the milestone-closure ceremony steady state requires around structural edits.
    pub ticket_mutation_without_milestone_ceremony: bool,
    /// How tolerant verification is of exploratory, not-yet-clean code during ignition.
    pub exploratory_code_tolerance: Tolerance,
    /// How many tickets the scheduler may fan a single parent out into at once during ignition.
    /// Wider than the steady-state ceiling by design: an unproven project benefits from
    /// exploring several approaches at once more than from tight admission control.
    pub fan_out_width: u32,
    /// The V0 milestone, pinned as the top-priority milestone for the duration of ignition.
    pub v0_objective_milestone: MilestoneId,
}

impl IgnitionPolicy {
    /// A conservative-but-broader-than-steady-state ignition policy pinning `v0_objective`.
    /// Callers are expected to override individual fields (especially `default_authority`,
    /// which depends on the project's actual repository) rather than rely on these values as
    /// anything but a starting point.
    pub fn for_v0(v0_objective_milestone: MilestoneId) -> Self {
        IgnitionPolicy {
            default_authority: Authority::default(),
            interleave_planning_and_implementation: true,
            ticket_mutation_without_milestone_ceremony: true,
            exploratory_code_tolerance: Tolerance::Preferred,
            fan_out_width: 4,
            v0_objective_milestone,
        }
    }
}

/// The narrower policy [`crate::maturity`]'s authority reconvergence swaps an [`IgnitionPolicy`]
/// for, once the project has passed the maturity gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SteadyStatePolicy {
    /// The authority granted by default to a newly-created ticket once steady state applies.
    /// Always no broader than the [`IgnitionPolicy::default_authority`] it replaces.
    pub default_authority: Authority,
    /// How tolerant verification is of exploratory code once steady state applies (typically
    /// [`Tolerance::Strict`]).
    pub exploratory_code_tolerance: Tolerance,
    /// The fan-out ceiling once steady state applies. Always `<=`
    /// [`IgnitionPolicy::fan_out_width`].
    pub fan_out_width: u32,
}

impl SteadyStatePolicy {
    /// Derive a steady-state policy from the ignition policy it replaces, narrowing every
    /// relaxation back down. `narrower_authority` is supplied by the caller (authority
    /// reconvergence computes it from the project's actual, no-longer-ignition-scoped grants)
    /// rather than derived here, since this module has no access to project state.
    pub fn narrowed_from(ignition: &IgnitionPolicy, narrower_authority: Authority) -> Self {
        SteadyStatePolicy {
            default_authority: narrower_authority,
            exploratory_code_tolerance: Tolerance::Strict,
            fan_out_width: (ignition.fan_out_width / 2).max(1),
        }
    }
}
