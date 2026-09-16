//! Decisions as durable project objects (`SPEC.md` §4.6).
//!
//! Superseding is an event, never a mutation: the superseded record's row is untouched in the
//! log and in materialized state (`decisions.superseded_by` is set, but the row's own fields
//! stay exactly as first recorded). This module is the pure query surface over a decision set;
//! `Store` owns writing them via `materialize::apply`.

use tm_types::{ArtifactId, DecisionId, ParticipantId, PatternSet, TicketId, Timestamp};

/// One decision record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Identity.
    pub id: DecisionId,
    /// What the decision is about, in one line.
    pub subject: String,
    /// The decision itself, in prose.
    pub decision: String,
    /// Why, in prose.
    pub reason: String,
    /// Supporting evidence.
    pub evidence: Vec<ArtifactId>,
    /// Tickets this decision affects.
    pub affected_tickets: Vec<TicketId>,
    /// Repository paths this decision affects (glob-able, matched against `PatternSet`).
    pub affected_paths: Vec<String>,
    /// Who authored it.
    pub author: ParticipantId,
    /// When it was recorded.
    pub ts: Timestamp,
    /// The decision this one supersedes, if any.
    pub supersedes: Option<DecisionId>,
    /// The decision that supersedes this one, if any. Set only by a later `supersede` call; this
    /// record's other fields never change after creation.
    pub superseded_by: Option<DecisionId>,
}

impl Decision {
    /// True when no later decision has superseded this one.
    pub fn is_active(&self) -> bool {
        self.superseded_by.is_none()
    }

    /// Build a fresh, non-superseding decision record. `id`/`ts` are caller-supplied (from the
    /// injected `IdSource`/`Clock`) since this module never allocates either itself.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        id: DecisionId,
        subject: String,
        decision: String,
        reason: String,
        evidence: Vec<ArtifactId>,
        affected_tickets: Vec<TicketId>,
        affected_paths: Vec<String>,
        author: ParticipantId,
        ts: Timestamp,
    ) -> Self {
        Decision {
            id,
            subject,
            decision,
            reason,
            evidence,
            affected_tickets,
            affected_paths,
            author,
            ts,
            supersedes: None,
            superseded_by: None,
        }
    }

    /// Build the replacement record for a `supersede` call: a new [`Decision`] with
    /// `supersedes` pointing back at `self.id`. Does **not** mutate `self` — the caller (`Store`,
    /// inside one `Tx`) is responsible for separately recording, on the *existing* row, that
    /// `superseded_by` is now `new_id`, via a `decision.superseded` event rather than an
    /// in-place edit of the original record's other fields.
    #[allow(clippy::too_many_arguments)]
    pub fn superseding(
        &self,
        new_id: DecisionId,
        subject: String,
        decision: String,
        reason: String,
        evidence: Vec<ArtifactId>,
        affected_tickets: Vec<TicketId>,
        affected_paths: Vec<String>,
        author: ParticipantId,
        ts: Timestamp,
    ) -> Self {
        Decision {
            id: new_id,
            subject,
            decision,
            reason,
            evidence,
            affected_tickets,
            affected_paths,
            author,
            ts,
            supersedes: Some(self.id.clone()),
            superseded_by: None,
        }
    }
}

/// Read-only query surface over a decision set, implemented by `Store`'s view over materialized
/// state (`decisions` table) and usable directly against an in-memory `Vec<Decision>` in tests.
pub trait DecisionStore {
    /// Every decision, active and superseded.
    fn all(&self) -> Vec<&Decision>;

    /// Look up one decision by id.
    fn get(&self, id: &DecisionId) -> Option<&Decision>;

    /// Every decision whose `subject` mentions `subject` (implementation-defined match: exact or
    /// substring, documented at the `Store` call site since it depends on how subjects are
    /// indexed there).
    fn by_subject(&self, subject: &str) -> Vec<&Decision> {
        self.all()
            .into_iter()
            .filter(|d| d.subject == subject)
            .collect()
    }

    /// Every decision whose `affected_paths` may overlap `path` (via
    /// `tm_types::PathPattern`/`PatternSet` matching, conservative toward "does overlap").
    fn by_affected_path(&self, path: &str) -> Vec<&Decision> {
        self.all()
            .into_iter()
            .filter(|d| {
                let pattern_set = PatternSet::parse(d.affected_paths.iter().cloned())
                    .unwrap_or_else(|_| PatternSet::empty());
                pattern_set.matches(path)
            })
            .collect()
    }

    /// The active (non-superseded) decisions relevant to `path` and/or `ticket`, the query
    /// `tm-context` compilation uses to decide what prior decisions a worker should see. Returns
    /// only decisions where [`Decision::is_active`] is true.
    fn active_for(&self, path: Option<&str>, ticket: Option<&TicketId>) -> Vec<&Decision> {
        let mut candidates = Vec::new();

        if let Some(p) = path {
            candidates.extend(self.by_affected_path(p));
        }

        if let Some(t) = ticket {
            candidates.extend(
                self.all()
                    .into_iter()
                    .filter(|d| d.affected_tickets.contains(t)),
            );
        }

        // Filter to active decisions only
        candidates.retain(|d| d.is_active());

        // Deduplicate by id and sort by timestamp then id for stable order
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|d| seen.insert(d.id.clone()));
        candidates.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));

        candidates
    }
}

// Helper trait implementation for testing
impl DecisionStore for Vec<Decision> {
    fn all(&self) -> Vec<&Decision> {
        self.iter().collect()
    }

    fn get(&self, id: &DecisionId) -> Option<&Decision> {
        self.iter().find(|d| d.id == *id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::Timestamp;

    fn test_id(n: u8) -> DecisionId {
        DecisionId::new(format!("D-{}", n)).unwrap()
    }

    fn test_ticket(n: u8) -> TicketId {
        TicketId::new(format!("T-{}", n)).unwrap()
    }

    fn test_artifact(n: u8) -> ArtifactId {
        ArtifactId::new(format!("ART-{:012x}", n)).unwrap()
    }

    fn test_participant(name: &str) -> ParticipantId {
        ParticipantId::new(format!("human:{}", name)).unwrap()
    }

    fn make_decision(
        id: DecisionId,
        subject: &str,
        affected_paths: Vec<&str>,
        affected_tickets: Vec<TicketId>,
        ts: Timestamp,
    ) -> Decision {
        Decision::create(
            id,
            subject.to_string(),
            "decision content".to_string(),
            "reason".to_string(),
            vec![],
            affected_tickets,
            affected_paths.into_iter().map(|s| s.to_string()).collect(),
            test_participant("alice"),
            ts,
        )
    }

    #[test]
    fn test_by_subject_exact_match() {
        let decisions = vec![
            make_decision(test_id(1), "API design", vec![], vec![], Timestamp::EPOCH),
            make_decision(
                test_id(2),
                "Database schema",
                vec![],
                vec![],
                Timestamp::EPOCH,
            ),
            make_decision(test_id(3), "API design", vec![], vec![], Timestamp::EPOCH),
        ];

        let results = decisions.by_subject("API design");
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|d| d.subject == "API design"));
    }

    #[test]
    fn test_by_subject_no_match() {
        let decisions = vec![make_decision(
            test_id(1),
            "API design",
            vec![],
            vec![],
            Timestamp::EPOCH,
        )];

        let results = decisions.by_subject("nonexistent");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_by_subject_empty_store() {
        let decisions: Vec<Decision> = vec![];
        let results = decisions.by_subject("API design");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_by_affected_path_glob_match() {
        let decisions = vec![
            make_decision(
                test_id(1),
                "docs change",
                vec!["docs/**"],
                vec![],
                Timestamp::EPOCH,
            ),
            make_decision(
                test_id(2),
                "src change",
                vec!["src/lib.rs"],
                vec![],
                Timestamp::EPOCH,
            ),
        ];

        let results = decisions.by_affected_path("docs/api.md");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, test_id(1));
    }

    #[test]
    fn test_by_affected_path_exact_match() {
        let decisions = vec![make_decision(
            test_id(1),
            "file change",
            vec!["src/lib.rs"],
            vec![],
            Timestamp::EPOCH,
        )];

        let results = decisions.by_affected_path("src/lib.rs");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_by_affected_path_no_match() {
        let decisions = vec![make_decision(
            test_id(1),
            "docs change",
            vec!["docs/**"],
            vec![],
            Timestamp::EPOCH,
        )];

        let results = decisions.by_affected_path("src/main.rs");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_by_affected_path_multiple_patterns() {
        let decisions = vec![make_decision(
            test_id(1),
            "multi pattern",
            vec!["docs/**", "tests/**"],
            vec![],
            Timestamp::EPOCH,
        )];

        let results_docs = decisions.by_affected_path("docs/guide.md");
        assert_eq!(results_docs.len(), 1);

        let results_tests = decisions.by_affected_path("tests/main.rs");
        assert_eq!(results_tests.len(), 1);

        let results_other = decisions.by_affected_path("src/lib.rs");
        assert_eq!(results_other.len(), 0);
    }

    #[test]
    fn test_active_for_path_only() {
        let ts1 = Timestamp::EPOCH;
        let ts2 = Timestamp::EPOCH.plus_seconds(1);

        let mut decisions = vec![
            make_decision(test_id(1), "decision1", vec!["docs/**"], vec![], ts1),
            make_decision(test_id(2), "decision2", vec!["src/**"], vec![], ts2),
        ];

        // Make decision 1 not active by marking it superseded
        decisions[0].superseded_by = Some(test_id(99));

        let results = decisions.active_for(Some("docs/api.md"), None);
        assert_eq!(results.len(), 0, "should not return superseded decisions");

        let results = decisions.active_for(Some("src/lib.rs"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, test_id(2));
    }

    #[test]
    fn test_active_for_ticket_only() {
        let ts1 = Timestamp::EPOCH;
        let ts2 = Timestamp::EPOCH.plus_seconds(1);
        let ticket_id = test_ticket(42);

        let decisions = vec![
            make_decision(
                test_id(1),
                "ticket decision",
                vec![],
                vec![ticket_id.clone()],
                ts1,
            ),
            make_decision(
                test_id(2),
                "other ticket",
                vec![],
                vec![test_ticket(99)],
                ts2,
            ),
        ];

        let results = decisions.active_for(None, Some(&ticket_id));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, test_id(1));
    }

    #[test]
    fn test_active_for_path_and_ticket() {
        let ts1 = Timestamp::EPOCH;
        let ts2 = Timestamp::EPOCH.plus_seconds(1);
        let ts3 = Timestamp::EPOCH.plus_seconds(2);
        let ticket_id = test_ticket(42);

        let decisions = vec![
            make_decision(test_id(1), "path decision", vec!["docs/**"], vec![], ts1),
            make_decision(
                test_id(2),
                "ticket decision",
                vec![],
                vec![ticket_id.clone()],
                ts2,
            ),
            make_decision(
                test_id(3),
                "both",
                vec!["src/**"],
                vec![ticket_id.clone()],
                ts3,
            ),
        ];

        let results = decisions.active_for(Some("docs/api.md"), Some(&ticket_id));
        assert_eq!(
            results.len(),
            3,
            "should return path match, ticket match, and both"
        );
    }

    #[test]
    fn test_active_for_filters_inactive() {
        let ts1 = Timestamp::EPOCH;
        let ts2 = Timestamp::EPOCH.plus_seconds(1);
        let ticket_id = test_ticket(42);

        let mut decisions = vec![
            make_decision(test_id(1), "path decision", vec!["docs/**"], vec![], ts1),
            make_decision(
                test_id(2),
                "ticket decision",
                vec![],
                vec![ticket_id.clone()],
                ts2,
            ),
        ];

        // Mark both as superseded
        decisions[0].superseded_by = Some(test_id(98));
        decisions[1].superseded_by = Some(test_id(99));

        let results = decisions.active_for(Some("docs/api.md"), Some(&ticket_id));
        assert_eq!(
            results.len(),
            0,
            "should not return any superseded decisions"
        );
    }

    #[test]
    fn test_active_for_deduplicates() {
        let ts1 = Timestamp::EPOCH;
        let ticket_id = test_ticket(42);

        let decisions = vec![
            // This decision matches both path and ticket
            make_decision(
                test_id(1),
                "decision",
                vec!["docs/**"],
                vec![ticket_id.clone()],
                ts1,
            ),
        ];

        let results = decisions.active_for(Some("docs/api.md"), Some(&ticket_id));
        assert_eq!(results.len(), 1, "should deduplicate the same decision");
        assert_eq!(results[0].id, test_id(1));
    }

    #[test]
    fn test_active_for_stable_sort_order() {
        let ts1 = Timestamp::EPOCH;
        let ts2 = Timestamp::EPOCH.plus_seconds(1);
        let ts3 = Timestamp::EPOCH.plus_seconds(2);
        let ticket_id = test_ticket(42);

        let decisions = vec![
            // Query will find these in reverse order from by_affected_path and then by_ticket
            make_decision(test_id(3), "third", vec!["docs/**"], vec![], ts3),
            make_decision(test_id(1), "first", vec!["docs/**"], vec![], ts1),
            make_decision(test_id(2), "second", vec![], vec![ticket_id.clone()], ts2),
        ];

        let results = decisions.active_for(Some("docs/api.md"), Some(&ticket_id));
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].id,
            test_id(1),
            "should be sorted by timestamp first"
        );
        assert_eq!(results[1].id, test_id(2));
        assert_eq!(results[2].id, test_id(3));
    }

    #[test]
    fn test_active_for_none_path_and_ticket() {
        let decisions = vec![make_decision(
            test_id(1),
            "decision",
            vec!["docs/**"],
            vec![],
            Timestamp::EPOCH,
        )];

        let results = decisions.active_for(None, None);
        assert_eq!(
            results.len(),
            0,
            "should return nothing when both path and ticket are None"
        );
    }

    #[test]
    fn test_is_active_true_when_no_supersede() {
        let decision = make_decision(test_id(1), "decision", vec![], vec![], Timestamp::EPOCH);
        assert!(decision.is_active());
    }

    #[test]
    fn test_is_active_false_when_superseded() {
        let mut decision = make_decision(test_id(1), "decision", vec![], vec![], Timestamp::EPOCH);
        decision.superseded_by = Some(test_id(2));
        assert!(!decision.is_active());
    }

    #[test]
    fn test_create_builds_correct_decision() {
        let id = test_id(1);
        let subject = "test subject".to_string();
        let decision = "test decision".to_string();
        let reason = "test reason".to_string();
        let ts = Timestamp::EPOCH;

        let result = Decision::create(
            id.clone(),
            subject.clone(),
            decision.clone(),
            reason.clone(),
            vec![],
            vec![],
            vec![],
            test_participant("bob"),
            ts,
        );

        assert_eq!(result.id, id);
        assert_eq!(result.subject, subject);
        assert_eq!(result.decision, decision);
        assert_eq!(result.reason, reason);
        assert_eq!(result.ts, ts);
        assert!(result.supersedes.is_none());
        assert!(result.superseded_by.is_none());
        assert!(result.is_active());
    }

    #[test]
    fn test_superseding_creates_new_decision() {
        let id1 = test_id(1);
        let id2 = test_id(2);
        let ts1 = Timestamp::EPOCH;
        let ts2 = Timestamp::EPOCH.plus_seconds(1);

        let original = Decision::create(
            id1.clone(),
            "old subject".to_string(),
            "old decision".to_string(),
            "old reason".to_string(),
            vec![],
            vec![],
            vec![],
            test_participant("alice"),
            ts1,
        );

        let replacement = original.superseding(
            id2.clone(),
            "new subject".to_string(),
            "new decision".to_string(),
            "new reason".to_string(),
            vec![],
            vec![],
            vec![],
            test_participant("bob"),
            ts2,
        );

        assert_eq!(replacement.id, id2);
        assert_eq!(replacement.supersedes, Some(id1.clone()));
        assert!(replacement.superseded_by.is_none());
        assert_eq!(replacement.subject, "new subject");
        assert_eq!(replacement.ts, ts2);
        // Original should be unchanged
        assert!(original.supersedes.is_none());
        assert!(original.superseded_by.is_none());
    }

    #[test]
    fn test_by_affected_path_empty_patterns() {
        let decisions = vec![make_decision(
            test_id(1),
            "no patterns",
            vec![],
            vec![],
            Timestamp::EPOCH,
        )];

        let results = decisions.by_affected_path("any/path");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_create_carries_evidence_artifacts_through() {
        let decision = Decision::create(
            test_id(1),
            "naming".to_string(),
            "use snake_case".to_string(),
            "consistency".to_string(),
            vec![test_artifact(1), test_artifact(2)],
            vec![test_ticket(1)],
            vec!["src/**".to_string()],
            test_participant("alice"),
            Timestamp::EPOCH,
        );
        assert_eq!(decision.evidence, vec![test_artifact(1), test_artifact(2)]);
    }
}
