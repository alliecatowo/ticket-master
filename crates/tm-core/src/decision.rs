//! Decisions as durable project objects.
//!
//! A decision is never mutated once recorded; superseding it appends a new decision and a
//! `decision.superseded` event pointing the old record at the new one, but the old record's own
//! fields never change in the log (`SPEC.md` §4.6). This module owns the pure query helpers
//! (`lookup_by_subject`, `lookup_by_path`, `active`) that `tm-context` compiles context against;
//! `store.rs::record_decision`/`supersede` own the event-drafting and persistence.

use tm_types::{ArtifactId, DecisionId, ParticipantId, TicketId, Timestamp};

/// One durable decision record. See `SPEC.md` §4.6.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Stable identifier.
    pub id: DecisionId,
    /// What this decision is about (a free-text subject line, e.g. "auth strategy").
    pub subject: String,
    /// The decision itself, in natural language.
    pub decision: String,
    /// Why this decision was made.
    pub reason: String,
    /// Supporting artifacts.
    pub evidence: Vec<ArtifactId>,
    /// Tickets this decision constrains or explains.
    pub affected_tickets: Vec<TicketId>,
    /// Project-relative paths this decision constrains or explains.
    pub affected_docs: Vec<String>,
    /// Who recorded this decision.
    pub author: ParticipantId,
    /// When this decision was recorded.
    pub ts: Timestamp,
    /// The decision this one supersedes, if any.
    pub supersedes: Option<DecisionId>,
    /// The decision that supersedes this one, if any. `None` means this decision is active.
    pub superseded_by: Option<DecisionId>,
}

/// A [`Decision`] together with the identity fields query helpers key on; kept as a distinct
/// type from `Decision` so `materialize.rs`'s row shape and `decision.rs`'s query surface can
/// evolve independently even though today they carry the same fields.
pub type DecisionRecord = Decision;

impl Decision {
    /// True when no later decision supersedes this one.
    pub fn is_active(&self) -> bool {
        self.superseded_by.is_none()
    }
}

/// Every decision whose `subject` equals `subject`, most recent first.
pub fn lookup_by_subject<'a>(all: &'a [Decision], subject: &str) -> Vec<&'a Decision> {
    let mut result: Vec<&Decision> = all.iter().filter(|d| d.subject == subject).collect();
    result.sort_by(|a, b| b.ts.cmp(&a.ts));
    result
}

/// Every decision whose `affected_docs` contains a path that `path` falls under, most recent
/// first. Used by context compilation to find decisions relevant to a file being touched.
pub fn lookup_by_path<'a>(all: &'a [Decision], path: &str) -> Vec<&'a Decision> {
    let mut result: Vec<&Decision> = all
        .iter()
        .filter(|d| {
            d.affected_docs.iter().any(|affected_path| {
                path == affected_path || path.starts_with(&format!("{}/", affected_path))
            })
        })
        .collect();
    result.sort_by(|a, b| b.ts.cmp(&a.ts));
    result
}

/// Every currently-active (`is_active`) decision, the query context compilation uses to build a
/// project's effective decision set.
pub fn active(all: &[Decision]) -> Vec<&Decision> {
    all.iter().filter(|d| d.is_active()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision_with(
        id: &str,
        subject: &str,
        affected_docs: Vec<&str>,
        ts: i64,
        superseded_by: Option<&str>,
    ) -> Decision {
        Decision {
            id: DecisionId::new(id).unwrap(),
            subject: subject.to_string(),
            decision: format!("decision for {}", subject),
            reason: "test reason".to_string(),
            evidence: vec![],
            affected_tickets: vec![],
            affected_docs: affected_docs.into_iter().map(|s| s.to_string()).collect(),
            author: ParticipantId::system(),
            ts: Timestamp::from_unix_seconds(ts),
            supersedes: None,
            superseded_by: superseded_by.map(|id| DecisionId::new(id).unwrap()),
        }
    }

    #[test]
    fn lookup_by_subject_finds_all_matching() {
        let d1 = decision_with("D-1", "auth", vec![], 100, None);
        let d2 = decision_with("D-2", "auth", vec![], 200, None);
        let d3 = decision_with("D-3", "storage", vec![], 150, None);

        let decisions = vec![d1, d2, d3];
        let result = lookup_by_subject(&decisions, "auth");

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id.as_str(), "D-2");
        assert_eq!(result[1].id.as_str(), "D-1");
    }

    #[test]
    fn lookup_by_subject_returns_most_recent_first() {
        let d1 = decision_with("D-1", "api", vec![], 100, None);
        let d2 = decision_with("D-2", "api", vec![], 300, None);
        let d3 = decision_with("D-3", "api", vec![], 200, None);

        let decisions = vec![d1, d2, d3];
        let result = lookup_by_subject(&decisions, "api");

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].ts.unix_seconds(), 300);
        assert_eq!(result[1].ts.unix_seconds(), 200);
        assert_eq!(result[2].ts.unix_seconds(), 100);
    }

    #[test]
    fn lookup_by_subject_no_matches() {
        let d1 = decision_with("D-1", "auth", vec![], 100, None);
        let d2 = decision_with("D-2", "storage", vec![], 200, None);

        let decisions = vec![d1, d2];
        let result = lookup_by_subject(&decisions, "network");

        assert!(result.is_empty());
    }

    #[test]
    fn lookup_by_subject_case_sensitive() {
        let d1 = decision_with("D-1", "Auth", vec![], 100, None);
        let d2 = decision_with("D-2", "auth", vec![], 200, None);

        let decisions = vec![d1, d2];
        let result = lookup_by_subject(&decisions, "auth");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id.as_str(), "D-2");
    }

    #[test]
    fn lookup_by_subject_single_match() {
        let d1 = decision_with("D-1", "auth", vec![], 100, None);
        let d2 = decision_with("D-2", "storage", vec![], 200, None);

        let decisions = vec![d1, d2];
        let result = lookup_by_subject(&decisions, "auth");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id.as_str(), "D-1");
    }

    #[test]
    fn lookup_by_path_exact_match() {
        let d1 = decision_with("D-1", "api", vec!["docs/api.md"], 100, None);
        let d2 = decision_with("D-2", "storage", vec!["src"], 200, None);

        let decisions = vec![d1, d2];
        let result = lookup_by_path(&decisions, "docs/api.md");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id.as_str(), "D-1");
    }

    #[test]
    fn lookup_by_path_prefix_match() {
        let d1 = decision_with("D-1", "api", vec!["src"], 100, None);
        let d2 = decision_with("D-2", "storage", vec!["docs"], 200, None);

        let decisions = vec![d1, d2];
        let result = lookup_by_path(&decisions, "src/lib.rs");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id.as_str(), "D-1");
    }

    #[test]
    fn lookup_by_path_deep_nested_prefix() {
        let d1 = decision_with("D-1", "api", vec!["src/core"], 100, None);

        let decisions = vec![d1];
        let result = lookup_by_path(&decisions, "src/core/machine.rs");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id.as_str(), "D-1");
    }

    #[test]
    fn lookup_by_path_multiple_matches_sorted() {
        let d1 = decision_with("D-1", "api", vec!["src"], 100, None);
        let d2 = decision_with("D-2", "storage", vec!["src/core"], 300, None);
        let d3 = decision_with("D-3", "network", vec!["src"], 200, None);

        let decisions = vec![d1, d2, d3];
        let result = lookup_by_path(&decisions, "src/core/machine.rs");

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].ts.unix_seconds(), 300);
        assert_eq!(result[1].ts.unix_seconds(), 200);
        assert_eq!(result[2].ts.unix_seconds(), 100);
    }

    #[test]
    fn lookup_by_path_no_match() {
        let d1 = decision_with("D-1", "api", vec!["src"], 100, None);
        let d2 = decision_with("D-2", "storage", vec!["docs"], 200, None);

        let decisions = vec![d1, d2];
        let result = lookup_by_path(&decisions, "tests/unit.rs");

        assert!(result.is_empty());
    }

    #[test]
    fn lookup_by_path_no_false_prefix_match() {
        let d1 = decision_with("D-1", "api", vec!["src"], 100, None);

        let decisions = vec![d1];
        let result = lookup_by_path(&decisions, "source/lib.rs");

        assert!(result.is_empty());
    }

    #[test]
    fn lookup_by_path_multiple_affected_docs() {
        let d1 = decision_with("D-1", "api", vec!["src", "docs", "tests"], 100, None);

        let decisions = vec![d1];

        let result1 = lookup_by_path(&decisions, "src/main.rs");
        assert_eq!(result1.len(), 1);

        let result2 = lookup_by_path(&decisions, "docs/README.md");
        assert_eq!(result2.len(), 1);

        let result3 = lookup_by_path(&decisions, "tests/unit.rs");
        assert_eq!(result3.len(), 1);

        let result4 = lookup_by_path(&decisions, "other/file.rs");
        assert!(result4.is_empty());
    }

    #[test]
    fn lookup_by_path_root_path() {
        let d1 = decision_with("D-1", "api", vec![""], 100, None);

        let decisions = vec![d1];
        let result = lookup_by_path(&decisions, "any/file.rs");

        assert!(result.is_empty());
    }

    #[test]
    fn lookup_by_path_root_path_exact_match() {
        let d1 = decision_with("D-1", "api", vec![""], 100, None);

        let decisions = vec![d1];
        let result = lookup_by_path(&decisions, "");

        assert_eq!(result.len(), 1);
    }

    #[test]
    fn active_filters_superseded_decisions() {
        let d1 = decision_with("D-1", "auth", vec![], 100, None);
        let d2 = decision_with("D-2", "auth", vec![], 200, Some("D-3"));
        let d3 = decision_with("D-3", "auth", vec![], 300, None);

        let decisions = vec![d1, d2, d3];
        let result = active(&decisions);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id.as_str(), "D-1");
        assert_eq!(result[1].id.as_str(), "D-3");
    }

    #[test]
    fn active_all_active() {
        let d1 = decision_with("D-1", "auth", vec![], 100, None);
        let d2 = decision_with("D-2", "storage", vec![], 200, None);

        let decisions = vec![d1, d2];
        let result = active(&decisions);

        assert_eq!(result.len(), 2);
    }

    #[test]
    fn active_all_superseded() {
        let d1 = decision_with("D-1", "auth", vec![], 100, Some("D-2"));
        let d2 = decision_with("D-2", "auth", vec![], 200, Some("D-3"));

        let decisions = vec![d1, d2];
        let result = active(&decisions);

        assert!(result.is_empty());
    }

    #[test]
    fn active_empty_list() {
        let decisions: Vec<Decision> = vec![];
        let result = active(&decisions);

        assert!(result.is_empty());
    }

    #[test]
    fn decision_is_active_true() {
        let d = decision_with("D-1", "auth", vec![], 100, None);
        assert!(d.is_active());
    }

    #[test]
    fn decision_is_active_false() {
        let d = decision_with("D-1", "auth", vec![], 100, Some("D-2"));
        assert!(!d.is_active());
    }

    #[test]
    fn lookup_by_path_complex_scenario() {
        let d1 = decision_with("D-1", "api", vec!["src/api"], 100, None);
        let d2 = decision_with("D-2", "core", vec!["src/core"], 200, Some("D-3"));
        let d3 = decision_with("D-3", "core", vec!["src/core"], 300, None);
        let d4 = decision_with("D-4", "docs", vec!["docs"], 250, None);

        let decisions = vec![d1, d2, d3, d4];
        let result = lookup_by_path(&decisions, "src/core/machine.rs");

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id.as_str(), "D-3");
        assert_eq!(result[1].id.as_str(), "D-2");
    }
}
