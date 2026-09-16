//! Success predicates.
//!
//! A predicate is how a ticket says what "done" means. Machine-checkable predicates are
//! evaluated by deterministic machinery; only [`Predicate::Judgment`] requires a model, and it
//! is the one the semantic auditor exists to answer.

use crate::id::TicketId;
use serde::{Deserialize, Serialize};

/// A condition that must hold for a ticket to close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    /// The command exits zero.
    CommandSucceeds {
        /// argv, not a shell string.
        command: Vec<String>,
    },
    /// The path exists in the working tree.
    FileExists {
        /// Repository-relative path.
        path: String,
    },
    /// The file exists and its contents match the regex.
    FileMatches {
        /// Repository-relative path.
        path: String,
        /// A regular expression.
        regex: String,
    },
    /// The named test suite passes (empty means the project default).
    TestsPass {
        /// Suite selector, if the project has more than one.
        #[serde(default)]
        suite: Option<String>,
    },
    /// Another ticket has closed.
    TicketClosed {
        /// The ticket that must be closed.
        ticket: TicketId,
    },
    /// Every child predicate holds.
    AllOf(Vec<Predicate>),
    /// At least one child predicate holds.
    AnyOf(Vec<Predicate>),
    /// The child predicate does not hold.
    Not(Box<Predicate>),
    /// A human has attested to this.
    HumanAttested {
        /// What the human is being asked to confirm.
        note: String,
    },
    /// A claim only a semantic audit can settle.
    Judgment {
        /// The claim, in natural language.
        claim: String,
    },
}

impl Predicate {
    /// True when this predicate can be settled without inference.
    ///
    /// [`Predicate::HumanAttested`] counts: the attestation is a durable evidence record, and
    /// checking whether it exists is ordinary software.
    pub fn is_machine_checkable(&self) -> bool {
        match self {
            Predicate::Judgment { .. } => false,
            Predicate::AllOf(ps) | Predicate::AnyOf(ps) => {
                ps.iter().all(|p| p.is_machine_checkable())
            }
            Predicate::Not(p) => p.is_machine_checkable(),
            _ => true,
        }
    }

    /// Every ticket this predicate depends on.
    pub fn referenced_tickets(&self) -> Vec<TicketId> {
        let mut out = Vec::new();
        self.walk(&mut |p| {
            if let Predicate::TicketClosed { ticket } = p {
                out.push(ticket.clone());
            }
        });
        out
    }

    /// Visit this predicate and all of its descendants.
    pub fn walk(&self, f: &mut impl FnMut(&Predicate)) {
        f(self);
        match self {
            Predicate::AllOf(ps) | Predicate::AnyOf(ps) => ps.iter().for_each(|p| p.walk(f)),
            Predicate::Not(p) => p.walk(f),
            _ => {}
        }
    }

    /// Combine the outcomes of children according to this predicate's shape.
    ///
    /// `leaf` settles the non-composite predicates; composition is pure.
    pub fn evaluate(
        &self,
        leaf: &mut impl FnMut(&Predicate) -> PredicateOutcome,
    ) -> PredicateOutcome {
        match self {
            Predicate::AllOf(ps) => {
                let mut pending: Option<PredicateOutcome> = None;
                for p in ps {
                    match p.evaluate(leaf) {
                        PredicateOutcome::Satisfied => {}
                        PredicateOutcome::Unsatisfied(why) => {
                            return PredicateOutcome::Unsatisfied(why)
                        }
                        o @ PredicateOutcome::RequiresJudgment(_) => pending = Some(o),
                    }
                }
                pending.unwrap_or(PredicateOutcome::Satisfied)
            }
            Predicate::AnyOf(ps) => {
                let mut pending: Option<PredicateOutcome> = None;
                let mut reasons = Vec::new();
                for p in ps {
                    match p.evaluate(leaf) {
                        PredicateOutcome::Satisfied => return PredicateOutcome::Satisfied,
                        PredicateOutcome::Unsatisfied(why) => reasons.push(why),
                        o @ PredicateOutcome::RequiresJudgment(_) => pending = Some(o),
                    }
                }
                pending.unwrap_or_else(|| {
                    PredicateOutcome::Unsatisfied(format!(
                        "no alternative held: {}",
                        reasons.join("; ")
                    ))
                })
            }
            Predicate::Not(p) => match p.evaluate(leaf) {
                PredicateOutcome::Satisfied => {
                    PredicateOutcome::Unsatisfied("negated predicate held".to_string())
                }
                PredicateOutcome::Unsatisfied(_) => PredicateOutcome::Satisfied,
                o @ PredicateOutcome::RequiresJudgment(_) => o,
            },
            other => leaf(other),
        }
    }
}

/// The result of evaluating a predicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredicateOutcome {
    /// The predicate holds.
    Satisfied,
    /// The predicate does not hold, with the reason.
    Unsatisfied(String),
    /// Settling this needs a semantic audit.
    RequiresJudgment(String),
}

impl PredicateOutcome {
    /// True only for [`PredicateOutcome::Satisfied`].
    pub fn is_satisfied(&self) -> bool {
        matches!(self, PredicateOutcome::Satisfied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tests_pass() -> Predicate {
        Predicate::TestsPass { suite: None }
    }

    #[test]
    fn only_judgment_needs_a_model() {
        assert!(tests_pass().is_machine_checkable());
        assert!(Predicate::HumanAttested { note: "x".into() }.is_machine_checkable());
        assert!(!Predicate::Judgment {
            claim: "feels right".into()
        }
        .is_machine_checkable());
        assert!(!Predicate::AllOf(vec![
            tests_pass(),
            Predicate::Judgment { claim: "x".into() }
        ])
        .is_machine_checkable());
    }

    #[test]
    fn all_of_short_circuits_on_failure() {
        let p = Predicate::AllOf(vec![
            tests_pass(),
            Predicate::FileExists { path: "a".into() },
        ]);
        let mut calls = 0;
        let out = p.evaluate(&mut |_| {
            calls += 1;
            PredicateOutcome::Unsatisfied("no".into())
        });
        assert_eq!(calls, 1);
        assert!(!out.is_satisfied());
    }

    #[test]
    fn judgment_defers_rather_than_failing() {
        let p = Predicate::AllOf(vec![
            tests_pass(),
            Predicate::Judgment {
                claim: "satisfies intent".into(),
            },
        ]);
        let out = p.evaluate(&mut |leaf| match leaf {
            Predicate::Judgment { claim } => PredicateOutcome::RequiresJudgment(claim.clone()),
            _ => PredicateOutcome::Satisfied,
        });
        assert!(matches!(out, PredicateOutcome::RequiresJudgment(_)));
    }

    #[test]
    fn any_of_and_not_compose() {
        let p = Predicate::AnyOf(vec![
            Predicate::FileExists {
                path: "missing".into(),
            },
            Predicate::Not(Box::new(Predicate::FileExists {
                path: "missing".into(),
            })),
        ]);
        let out = p.evaluate(&mut |_| PredicateOutcome::Unsatisfied("absent".into()));
        assert!(out.is_satisfied());
    }

    #[test]
    fn referenced_tickets_are_collected_recursively() {
        let p = Predicate::AllOf(vec![
            Predicate::TicketClosed {
                ticket: TicketId::new("T-1").unwrap(),
            },
            Predicate::Not(Box::new(Predicate::TicketClosed {
                ticket: TicketId::new("V-2").unwrap(),
            })),
        ]);
        let refs: Vec<String> = p
            .referenced_tickets()
            .iter()
            .map(|t| t.to_string())
            .collect();
        assert_eq!(refs, vec!["T-1", "V-2"]);
    }

    #[test]
    fn predicates_round_trip_through_json() {
        let p = Predicate::CommandSucceeds {
            command: vec!["cargo".into(), "test".into()],
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("command_succeeds"), "{s}");
        assert_eq!(serde_json::from_str::<Predicate>(&s).unwrap(), p);
    }
}
