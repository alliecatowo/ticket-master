//! [`ProjectionPolicy`]: which tickets surface on an external tracker, how descendants roll up
//! into one external issue, and capability-aware degradation.
//!
//! Pure and adapter-agnostic: this module never touches a [`crate::tracker::Tracker`], the
//! network, or the clock. It consumes `tm-core` ticket data and a target adapter's
//! [`crate::tracker::TrackerCapabilities`] and produces a [`Projection`] plus a record of every
//! degradation applied, so degradation is always visible on the resulting mirror link rather
//! than silent (`SPEC.md` §13).

use std::collections::BTreeMap;

use tm_core::{Ticket, TicketKind, TicketState};
use tm_types::TicketId;

use crate::tracker::TrackerCapabilities;

/// A degradation recorded when the target adapter can't faithfully represent something in the
/// internal graph, so it never happens silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Degradation {
    /// `parent_child` is false: descendants were folded into a checklist in the issue body
    /// instead of becoming separate linked issues.
    ChecklistRollup {
        /// Descendant tickets folded into the checklist, in body order.
        children: Vec<TicketId>,
    },
    /// `arbitrary_states` is false, or the target's state table doesn't have an exact entry:
    /// the internal state was mapped to the nearest configured external state rather than
    /// represented exactly. The exact internal state stays authoritative in Ticketmaster
    /// regardless of what the external issue displays.
    StateCoarsened {
        /// The exact internal state.
        internal: String,
        /// The external state it was mapped onto.
        external: String,
    },
    /// The projected body exceeded `max_body_bytes` and was truncated to fit.
    BodyTruncated {
        /// Length in bytes before truncation.
        original_bytes: usize,
    },
}

/// One line of a checklist rollup body, produced when descendants can't become linked issues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistItem {
    /// The descendant ticket this line represents.
    pub ticket: TicketId,
    /// Ticket objective, used as the checklist label.
    pub label: String,
    /// Whether to render the checklist box checked (ticket is `Closed` or `Cancelled`).
    pub done: bool,
}

/// The outbound shape of one ticket (plus rolled-up descendants), ready to hand to a
/// [`crate::tracker::Tracker::push`].
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    /// The ticket this projection represents.
    pub ticket: TicketId,
    /// Issue title, derived from the ticket objective.
    pub title: String,
    /// Issue body: objective prose plus, where degraded, an appended checklist rollup.
    pub body: String,
    /// The external state to request, already mapped for the target adapter's capabilities.
    pub state_hint: String,
    /// Labels to apply, subject to the adapter's `labels` capability.
    pub labels: Vec<String>,
    /// Milestone name to attach, subject to the adapter's `milestones` capability.
    pub milestone: Option<String>,
    /// Checklist rollup lines, populated only when `parent_child` is false for the target.
    pub checklist: Vec<ChecklistItem>,
    /// Degradations applied while building this projection, to be recorded on the mirror link.
    pub degradations: Vec<Degradation>,
}

/// Decides which tickets surface externally and how, given per-adapter capabilities.
#[derive(Debug, Clone)]
pub struct ProjectionPolicy {
    /// Ticket kinds eligible to mirror at all, before the per-ticket `mirror` override is
    /// consulted. `Verification`, `Audit` and `Recovery` are never eligible regardless of what's
    /// configured here — [`ProjectionPolicy::should_mirror`] enforces that hard denylist itself.
    pub eligible_kinds: Vec<TicketKind>,
    /// Internal `TicketState` name -> external state name. Consulted directly when the target's
    /// `arbitrary_states` is true; used as a coarse (e.g. open/closed-shaped) table when it's
    /// false, in which case every mapping is recorded as a [`Degradation::StateCoarsened`].
    pub state_mapping: BTreeMap<String, String>,
    /// Body size ceiling assumed before a target's own `max_body_bytes` is known; a target's
    /// declared capability always wins once available.
    pub default_max_body_bytes: usize,
}

impl ProjectionPolicy {
    /// The default policy from `SPEC.md` §13: `Work` tickets are eligible (subject to the
    /// per-ticket `mirror` override / milestone-parented check applied by
    /// [`ProjectionPolicy::should_mirror`]); `state_mapping` starts as the identity map over
    /// [`TicketState`]'s external (debug-ish, lowercase-with-underscores) names; body ceiling is
    /// a conservative 64 KiB pending a real adapter capability.
    pub fn default_policy() -> Self {
        // IMPL: eligible_kinds = vec![TicketKind::Work]; state_mapping = one entry per
        // TicketState variant mapping its name (e.g. "Draft" -> "draft") to itself, built by
        // iterating a fixed TicketState::ALL-style list (add one if tm-core doesn't already
        // expose it, or match TicketState exhaustively) so the table stays in sync if tm-core
        // adds a state; default_max_body_bytes = 64 * 1024.
        todo!("construct the default projection policy")
    }

    /// Whether `ticket` is eligible to mirror at all.
    ///
    /// `mirror_override` is the per-ticket `mirror = true/false` flag from `SPEC.md` §13, when
    /// the caller has one (e.g. from ticket metadata this crate doesn't itself own). Precedence,
    /// highest first: (1) `TicketKind::{Verification,Audit,Recovery}` and any other
    /// machine-only kind are *never* mirrored, full stop, regardless of override or
    /// `eligible_kinds` — this is the one invariant `SPEC.md` calls out as non-negotiable; (2)
    /// `mirror_override`, if `Some`, wins outright; (3) otherwise, `ticket.kind` must be in
    /// `self.eligible_kinds` *and*, for `TicketKind::Work` specifically, `ticket.milestone` must
    /// be `Some` (the default policy's "parent is a milestone" rule).
    pub fn should_mirror(&self, ticket: &Ticket, mirror_override: Option<bool>) -> bool {
        // IMPL: see precedence above. Machine-only hard-deny list:
        // matches!(ticket.kind, TicketKind::Verification | TicketKind::Audit |
        // TicketKind::Recovery) -> false immediately, before even looking at
        // mirror_override.
        todo!("apply the never-mirror hard denylist, then override, then default eligibility")
    }

    /// Map an internal state to the external state to request, given the target's capability.
    /// Returns the external state name and, when the mapping isn't exact, the
    /// [`Degradation::StateCoarsened`] to attach to the resulting projection.
    pub fn map_state(
        &self,
        state: TicketState,
        caps: &TrackerCapabilities,
    ) -> (String, Option<Degradation>) {
        // IMPL: internal_name = format!("{state:?}") (or a dedicated Display if tm-core adds
        // one later). If caps.arbitrary_states: look up internal_name in self.state_mapping;
        // if present and equal to internal_name, no degradation; if present and different,
        // still no degradation (arbitrary states means the exact internal state *is*
        // representable, the mapping is just a display choice) — only fall back to
        // internal_name itself (no degradation) if absent from the table. If
        // !caps.arbitrary_states: look up internal_name in self.state_mapping (the adapter's
        // coarse table, e.g. every state but Closed/Cancelled -> "open"), falling back to
        // "open" if absent; always attach StateCoarsened{internal: internal_name, external:
        // <result>} in this branch, since precision is lost regardless of whether the coarse
        // table happens to already agree.
        todo!("map internal state to external state, recording coarsening when caps demand it")
    }

    /// Build the checklist body for descendants when `parent_child` is false. Order matches
    /// `descendants` (callers pass them in a stable order, e.g. creation order).
    pub fn checklist_rollup(&self, descendants: &[Ticket]) -> Vec<ChecklistItem> {
        // IMPL: one ChecklistItem per descendant; `done` = matches!(descendant.state,
        // TicketState::Closed | TicketState::Cancelled); `label` = descendant.objective.clone().
        // Pure map, preserves input order, no filtering (a caller that wants only mirror-eligible
        // descendants filters before calling, since eligibility is a `should_mirror` decision
        // this function doesn't need to repeat).
        todo!("map descendants to checklist items in input order")
    }

    /// Project `ticket` (with its already-fetched `descendants`) for a target with the given
    /// capabilities. The single entry point adapters and [`crate::sync::SyncEngine`] call.
    pub fn project(
        &self,
        ticket: &Ticket,
        descendants: &[Ticket],
        caps: &TrackerCapabilities,
    ) -> Projection {
        // IMPL: title = ticket.objective.clone() (or a truncated first line if it's
        // multi-paragraph — keep it a single line, external issue titles are). body starts as
        // ticket.objective.clone(); call self.map_state(ticket.state, caps), pushing its
        // Degradation if any and setting state_hint. If !caps.parent_child &&
        // !descendants.is_empty(): checklist = self.checklist_rollup(descendants), append a
        // rendered "\n\n## Subtasks\n" + one "- [x]/[ ] <label> (<ticket id>)" line per item to
        // body, push Degradation::ChecklistRollup{children: descendants.iter().map(|d|
        // d.id.clone()).collect()}; else checklist = Vec::new(). If body.len() as usize >
        // caps.max_body_bytes: truncate body to a UTF-8 char boundary at or before
        // caps.max_body_bytes, push Degradation::BodyTruncated{original_bytes:
        // <pre-truncation length>}. labels/milestone are populated by the caller layer (adapter
        // or sync) from AdapterConfig::projection overrides, since this function only sees a
        // ticket + capabilities, not config — leave them Vec::new()/None here.
        todo!("assemble a Projection from a ticket, its descendants, and target capabilities")
    }
}
