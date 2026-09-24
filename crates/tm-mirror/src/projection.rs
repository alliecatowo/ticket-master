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

#[cfg(test)]
fn test_ticket(id: &str, kind: TicketKind, state: TicketState, objective: &str) -> Ticket {
    use tm_core::{ExecutorRequirements, RetryPolicy, VerificationPolicy};
    use tm_types::{Authority, Budget, Role, Timestamp, Tolerance};

    Ticket {
        id: TicketId::new(id).expect("valid test ticket id"),
        kind,
        objective: objective.to_string(),
        state,
        parent: None,
        children: Vec::new(),
        dependencies: Vec::new(),
        milestone: None,
        due: None,
        authority: Authority::default(),
        resources: Vec::new(),
        executor: ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Preferred,
        },
        context_refs: Vec::new(),
        success: Vec::new(),
        verification: VerificationPolicy::None,
        budget: Budget::default(),
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 120,
        },
        cycle: None,
        attempts: 0,
        failures: Vec::new(),
        priority: 0,
        created: Timestamp::EPOCH,
        updated: Timestamp::EPOCH,
    }
}

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
        const ALL_STATES: [TicketState; 14] = [
            TicketState::Draft,
            TicketState::Blocked,
            TicketState::Ready,
            TicketState::Leased,
            TicketState::Running,
            TicketState::Submitted,
            TicketState::Verifying,
            TicketState::Auditing,
            TicketState::Rework,
            TicketState::Replan,
            TicketState::Recovery,
            TicketState::Escalated,
            TicketState::Closed,
            TicketState::Cancelled,
        ];
        let mut state_mapping = BTreeMap::new();
        for state in ALL_STATES {
            let name = format!("{state:?}");
            state_mapping.insert(name.clone(), name);
        }
        Self {
            eligible_kinds: vec![TicketKind::Work],
            state_mapping,
            default_max_body_bytes: 64 * 1024,
        }
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
        if matches!(
            ticket.kind,
            TicketKind::Verification | TicketKind::Audit | TicketKind::Recovery
        ) {
            return false;
        }
        if let Some(overridden) = mirror_override {
            return overridden;
        }
        self.eligible_kinds.contains(&ticket.kind)
            && (ticket.kind != TicketKind::Work || ticket.milestone.is_some())
    }

    /// Map an internal state to the external state to request, given the target's capability.
    /// Returns the external state name and, when the mapping isn't exact, the
    /// [`Degradation::StateCoarsened`] to attach to the resulting projection.
    pub fn map_state(
        &self,
        state: TicketState,
        caps: &TrackerCapabilities,
    ) -> (String, Option<Degradation>) {
        let internal_name = format!("{state:?}");
        if caps.arbitrary_states {
            let external = self
                .state_mapping
                .get(&internal_name)
                .cloned()
                .unwrap_or_else(|| internal_name.clone());
            (external, None)
        } else {
            let external = self
                .state_mapping
                .get(&internal_name)
                .cloned()
                .unwrap_or_else(|| "open".to_string());
            let degradation = Degradation::StateCoarsened {
                internal: internal_name,
                external: external.clone(),
            };
            (external, Some(degradation))
        }
    }

    /// Build the checklist body for descendants when `parent_child` is false. Order matches
    /// `descendants` (callers pass them in a stable order, e.g. creation order).
    pub fn checklist_rollup(&self, descendants: &[Ticket]) -> Vec<ChecklistItem> {
        descendants
            .iter()
            .map(|descendant| ChecklistItem {
                ticket: descendant.id.clone(),
                label: descendant.objective.clone(),
                done: matches!(
                    descendant.state,
                    TicketState::Closed | TicketState::Cancelled
                ),
            })
            .collect()
    }

    /// Project `ticket` (with its already-fetched `descendants`) for a target with the given
    /// capabilities. The single entry point adapters and [`crate::sync::SyncEngine`] call.
    pub fn project(
        &self,
        ticket: &Ticket,
        descendants: &[Ticket],
        caps: &TrackerCapabilities,
    ) -> Projection {
        let title = ticket
            .objective
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let mut body = ticket.objective.clone();
        let mut degradations = Vec::new();

        let (state_hint, state_degradation) = self.map_state(ticket.state, caps);
        if let Some(degradation) = state_degradation {
            degradations.push(degradation);
        }

        let checklist = if !caps.parent_child && !descendants.is_empty() {
            let items = self.checklist_rollup(descendants);
            body.push_str("\n\n## Subtasks\n");
            for item in &items {
                let mark = if item.done { 'x' } else { ' ' };
                body.push_str(&format!("- [{mark}] {} ({})\n", item.label, item.ticket));
            }
            degradations.push(Degradation::ChecklistRollup {
                children: descendants.iter().map(|d| d.id.clone()).collect(),
            });
            items
        } else {
            Vec::new()
        };

        if body.len() > caps.max_body_bytes {
            let original_bytes = body.len();
            let mut boundary = caps.max_body_bytes;
            while boundary > 0 && !body.is_char_boundary(boundary) {
                boundary -= 1;
            }
            body.truncate(boundary);
            degradations.push(Degradation::BodyTruncated { original_bytes });
        }

        Projection {
            ticket: ticket.id.clone(),
            title,
            body,
            state_hint,
            labels: Vec::new(),
            milestone: None,
            checklist,
            degradations,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::MilestoneId;

    fn full_caps(max_body_bytes: usize) -> TrackerCapabilities {
        TrackerCapabilities {
            parent_child: true,
            arbitrary_states: true,
            milestones: true,
            labels: true,
            comments: true,
            max_body_bytes,
        }
    }

    #[test]
    fn should_mirror_denies_verification_audit_and_recovery_regardless_of_override() {
        let policy = ProjectionPolicy::default_policy();
        for kind in [
            TicketKind::Verification,
            TicketKind::Audit,
            TicketKind::Recovery,
        ] {
            let mut ticket = test_ticket("T-1", kind, TicketState::Draft, "obj");
            ticket.milestone = Some(MilestoneId::new("M-1").unwrap());
            assert!(!policy.should_mirror(&ticket, Some(true)));
        }
    }

    #[test]
    fn should_mirror_override_wins_over_default_eligibility() {
        let policy = ProjectionPolicy::default_policy();
        let ticket = test_ticket("T-1", TicketKind::Work, TicketState::Draft, "obj");
        assert!(policy.should_mirror(&ticket, Some(true)));
        assert!(!policy.should_mirror(&ticket, Some(false)));
    }

    #[test]
    fn should_mirror_work_ticket_requires_milestone_by_default() {
        let policy = ProjectionPolicy::default_policy();
        let mut ticket = test_ticket("T-1", TicketKind::Work, TicketState::Draft, "obj");
        assert!(!policy.should_mirror(&ticket, None));
        ticket.milestone = Some(MilestoneId::new("M-1").unwrap());
        assert!(policy.should_mirror(&ticket, None));
    }

    #[test]
    fn should_mirror_kind_outside_eligible_list_is_denied() {
        let policy = ProjectionPolicy::default_policy();
        let ticket = test_ticket("T-1", TicketKind::Harness, TicketState::Draft, "obj");
        assert!(!policy.should_mirror(&ticket, None));
    }

    #[test]
    fn map_state_with_arbitrary_states_uses_exact_internal_name_without_degradation() {
        let policy = ProjectionPolicy::default_policy();
        let caps = full_caps(1024);
        let (external, degradation) = policy.map_state(TicketState::Running, &caps);
        assert_eq!(external, "Running");
        assert!(degradation.is_none());
    }

    #[test]
    fn map_state_without_arbitrary_states_always_records_coarsening() {
        let policy = ProjectionPolicy::default_policy();
        let mut caps = full_caps(1024);
        caps.arbitrary_states = false;
        let (external, degradation) = policy.map_state(TicketState::Running, &caps);
        assert_eq!(external, "Running");
        assert_eq!(
            degradation,
            Some(Degradation::StateCoarsened {
                internal: "Running".to_string(),
                external: "Running".to_string(),
            })
        );
    }

    #[test]
    fn map_state_falls_back_to_open_when_coarse_table_lacks_an_entry() {
        let mut policy = ProjectionPolicy::default_policy();
        policy.state_mapping.remove("Running");
        let mut caps = full_caps(1024);
        caps.arbitrary_states = false;
        let (external, degradation) = policy.map_state(TicketState::Running, &caps);
        assert_eq!(external, "open");
        assert_eq!(
            degradation,
            Some(Degradation::StateCoarsened {
                internal: "Running".to_string(),
                external: "open".to_string(),
            })
        );
    }

    #[test]
    fn checklist_rollup_marks_closed_and_cancelled_done_and_preserves_order() {
        let policy = ProjectionPolicy::default_policy();
        let descendants = vec![
            test_ticket("T-1", TicketKind::Work, TicketState::Running, "first"),
            test_ticket("T-2", TicketKind::Work, TicketState::Closed, "second"),
            test_ticket("T-3", TicketKind::Work, TicketState::Cancelled, "third"),
        ];
        let items = policy.checklist_rollup(&descendants);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].ticket.as_str(), "T-1");
        assert!(!items[0].done);
        assert!(items[1].done);
        assert!(items[2].done);
    }

    #[test]
    fn project_without_parent_child_rolls_up_descendants_into_checklist_body() {
        let policy = ProjectionPolicy::default_policy();
        let mut caps = full_caps(4096);
        caps.parent_child = false;
        let ticket = test_ticket("T-1", TicketKind::Work, TicketState::Ready, "parent obj");
        let descendants = vec![test_ticket(
            "T-2",
            TicketKind::Work,
            TicketState::Closed,
            "child obj",
        )];
        let projection = policy.project(&ticket, &descendants, &caps);
        assert!(projection.body.contains("## Subtasks"));
        assert!(projection.body.contains("[x] child obj (T-2)"));
        assert_eq!(projection.checklist.len(), 1);
        assert!(projection
            .degradations
            .iter()
            .any(|d| matches!(d, Degradation::ChecklistRollup { children } if children == &[TicketId::new("T-2").unwrap()])));
    }

    #[test]
    fn project_with_parent_child_leaves_body_and_checklist_untouched() {
        let policy = ProjectionPolicy::default_policy();
        let caps = full_caps(4096);
        let ticket = test_ticket("T-1", TicketKind::Work, TicketState::Ready, "parent obj");
        let descendants = vec![test_ticket(
            "T-2",
            TicketKind::Work,
            TicketState::Closed,
            "child obj",
        )];
        let projection = policy.project(&ticket, &descendants, &caps);
        assert_eq!(projection.body, "parent obj");
        assert!(projection.checklist.is_empty());
        assert!(projection.degradations.is_empty());
    }

    #[test]
    fn project_truncates_body_exceeding_max_body_bytes_and_records_original_length() {
        let policy = ProjectionPolicy::default_policy();
        let caps = full_caps(5);
        let ticket = test_ticket(
            "T-1",
            TicketKind::Work,
            TicketState::Ready,
            "much longer than five bytes",
        );
        let projection = policy.project(&ticket, &[], &caps);
        assert!(projection.body.len() <= 5);
        assert!(projection.degradations.iter().any(
            |d| matches!(d, Degradation::BodyTruncated { original_bytes } if *original_bytes == 27)
        ));
    }

    #[test]
    fn project_truncation_stays_on_a_utf8_char_boundary() {
        let policy = ProjectionPolicy::default_policy();
        // "café" is 5 bytes ('é' is 2 bytes); a naive byte-4 cut would land mid-character.
        let caps = full_caps(4);
        let ticket = test_ticket("T-1", TicketKind::Work, TicketState::Ready, "café");
        let projection = policy.project(&ticket, &[], &caps);
        assert!(String::from_utf8(projection.body.clone().into_bytes()).is_ok());
        assert!(projection.body.len() <= 4);
    }

    #[test]
    fn default_policy_state_mapping_covers_every_ticket_state() {
        let policy = ProjectionPolicy::default_policy();
        assert_eq!(policy.state_mapping.len(), 14);
        assert_eq!(
            policy.state_mapping.get("Closed").map(String::as_str),
            Some("Closed")
        );
    }
}
