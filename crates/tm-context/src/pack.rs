//! [`ContextPack`]: the bounded, deterministic bundle [`compile`] hands to a worker instead of
//! raw files (`SPEC.md` §8.1). Sections are assembled in [`crate::tokens::SectionKind`]
//! priority order and admitted against a [`crate::tokens::TokenBudget`] via a
//! [`crate::tokens::BudgetLedger`]; whatever doesn't fit is dropped starting from the
//! lowest-priority section, and every drop is recorded in [`ContextPack::dropped`] rather than
//! silently truncated. Given the same `(ticket, view, codeintel-state, budget)`, `compile`
//! always produces byte-identical output, so the pack is snapshot-testable.

use tm_codeintel::{CodeIntel, SignalWeights};
use tm_core::{ProjectView, Ticket};
use tm_types::Result;

use crate::sections;
use crate::tokens::{estimate_tokens_prose, BudgetLedger, SectionKind, TokenBudget};

/// A pointer from a section's content back to the real-world thing it was drawn from
/// (a decision, a file, a commit, an artifact), so a consumer of the pack can verify or dig
/// deeper without re-deriving the same search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceRef {
    /// Free-form locator (decision id, path, commit sha, artifact id) interpreted by whatever
    /// produced it — mirrors `tm_core::ContextRef::locator`'s convention.
    pub locator: String,
    /// One-line human-readable detail about why this locator is here.
    pub detail: String,
}

/// One admitted section of a [`ContextPack`]: a [`sections::RawSection`] that survived budget
/// accounting, plus the token cost it was charged.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// Which section this is.
    pub kind: SectionKind,
    /// Short human-readable title.
    pub title: String,
    /// Rendered body text, exactly as it will be shown to a worker.
    pub body: String,
    /// Tokens this section was charged against the budget.
    pub tokens: usize,
    /// Provenance entries for this section's content.
    pub provenance: Vec<ProvenanceRef>,
}

/// A record of a section that did not fit and was dropped, so a consumer can see *that*
/// something was omitted rather than silently receiving a truncated picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedSection {
    /// Which section was dropped.
    pub kind: SectionKind,
    /// Why it was dropped (currently always a budget-overflow reason; the field is a `String`
    /// rather than a fixed enum so future drop reasons — e.g. an upstream fetch error a caller
    /// chooses to downgrade to "dropped" instead of failing the whole pack — don't need a
    /// signature change).
    pub reason: String,
    /// How many tokens the section would have needed to be admitted in full.
    pub tokens_needed: usize,
}

/// The compiled, bounded context handed to a worker for one ticket.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPack {
    /// Sections that were admitted, in priority order.
    pub sections: Vec<Section>,
    /// Total tokens actually spent across `sections`.
    pub tokens: usize,
    /// Provenance for every admitted section's content, flattened across sections in the same
    /// order they appear in `sections`.
    pub provenance: Vec<ProvenanceRef>,
    /// Sections that were dropped to fit the budget, lowest-priority first.
    pub dropped: Vec<DroppedSection>,
}

/// Compile a [`ContextPack`] for `ticket`.
///
/// Sections are built in [`SectionKind::PRIORITY_ORDER`] and admitted whole-or-dropped against
/// a [`BudgetLedger`] derived from `budget`: on overflow the lowest-priority section is the
/// first to lose out, and every drop is recorded in [`ContextPack::dropped`] rather than
/// silently truncating a section's body. Deterministic given identical `(ticket, view,
/// ci-index-state, budget)`, since every section builder is a pure function of its inputs.
pub fn compile(
    ticket: &Ticket,
    view: &ProjectView,
    ci: &CodeIntel,
    budget: TokenBudget,
    weights: SignalWeights,
    conventions: &[String],
) -> Result<ContextPack> {
    let raw_sections: Vec<(SectionKind, sections::RawSection)> = vec![
        (SectionKind::Objective, sections::build_objective(ticket)),
        (
            SectionKind::Decisions,
            sections::build_decisions(ticket, view),
        ),
        (
            SectionKind::Dependencies,
            sections::build_dependencies(ticket, view),
        ),
        (
            SectionKind::Retrieval,
            sections::build_retrieval(ticket, ci, weights)?,
        ),
        (
            SectionKind::SymbolOutlines,
            sections::build_symbol_outlines(ticket, ci)?,
        ),
        (
            SectionKind::GitHistory,
            sections::build_git_history(ticket, ci)?,
        ),
        (
            SectionKind::PriorFailures,
            sections::build_prior_failures(ticket),
        ),
        (
            SectionKind::Conventions,
            sections::build_conventions(conventions),
        ),
    ];

    let mut ledger = BudgetLedger::new(&budget);
    let mut sections_out = Vec::with_capacity(raw_sections.len());
    let mut provenance = Vec::new();
    let mut dropped = Vec::new();

    for (kind, raw) in raw_sections {
        let cost = estimate_tokens_prose(&raw.body);
        if ledger.spend(kind, cost) {
            provenance.extend(raw.provenance.iter().cloned());
            sections_out.push(Section {
                kind,
                title: raw.title,
                body: raw.body,
                tokens: cost,
                provenance: raw.provenance,
            });
        } else {
            dropped.push(DroppedSection {
                kind,
                reason: "exceeds remaining token budget".to_string(),
                tokens_needed: cost,
            });
        }
    }

    let tokens = ledger.total_used();

    Ok(ContextPack {
        sections: sections_out,
        tokens,
        provenance,
        dropped,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tm_codeintel::CodeIntel;
    use tm_core::{
        ExecutorRequirements, FailureClass, FailureRecord, RetryPolicy, Ticket, TicketKind,
        TicketState, VerificationPolicy,
    };
    use tm_types::{Authority, Budget, Role, TicketId, Timestamp, Tolerance};

    use super::*;

    fn base_ticket() -> Ticket {
        Ticket {
            id: TicketId::new("T-1").expect("literal id matches T-n"),
            kind: TicketKind::Work,
            objective: "Implement the widget".to_string(),
            state: TicketState::Ready,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Any,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::None,
            budget: Budget::none(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 1,
                backoff_multiplier: 2.0,
                max_delay_seconds: 60,
            },
            cycle: None,
            attempts: 0,
            failures: vec![],
            priority: 0,
            created: Timestamp::EPOCH,
            updated: Timestamp::EPOCH,
        }
    }

    fn open_empty_codeintel(dir: &std::path::Path) -> CodeIntel {
        CodeIntel::open(dir).expect("opening a fresh index over an empty temp dir succeeds")
    }

    #[test]
    fn compile_admits_every_section_when_budget_is_generous() {
        let ticket = base_ticket();
        let view = ProjectView::empty();
        let dir = tempfile::tempdir().expect("tempdir");
        let ci = open_empty_codeintel(dir.path());

        let pack = compile(
            &ticket,
            &view,
            &ci,
            TokenBudget::even(100_000),
            SignalWeights::default(),
            &[],
        )
        .expect("compile with a generous budget succeeds");

        assert!(pack.dropped.is_empty());
        assert_eq!(
            pack.tokens,
            pack.sections.iter().map(|s| s.tokens).sum::<usize>()
        );
        assert!(pack
            .provenance
            .iter()
            .any(|p| p.locator == ticket.id.to_string()));
    }

    #[test]
    fn compile_drops_a_section_that_does_not_fit_its_share() {
        let mut ticket = base_ticket();
        ticket.failures.push(FailureRecord {
            class: FailureClass::Other,
            detail: "boom".to_string(),
            at: Timestamp::EPOCH,
            attempt: 1,
        });
        let view = ProjectView::empty();
        let dir = tempfile::tempdir().expect("tempdir");
        let ci = open_empty_codeintel(dir.path());

        // Every other section renders empty (no decisions, deps, claimed paths or
        // conventions) so it costs 0 tokens and is admitted regardless of share; only
        // `Objective` gets a nonzero allotment, forcing `PriorFailures`' nonzero-cost body
        // to be the one dropped.
        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Objective, 1.0);
        let budget = TokenBudget {
            total: 1_000,
            shares,
        };

        let pack = compile(&ticket, &view, &ci, budget, SignalWeights::default(), &[])
            .expect("compile still succeeds when a section is dropped");

        assert_eq!(pack.dropped.len(), 1);
        let dropped = &pack.dropped[0];
        assert_eq!(dropped.kind, SectionKind::PriorFailures);
        assert_eq!(dropped.reason, "exceeds remaining token budget");
        assert!(dropped.tokens_needed > 0);
        assert!(!pack
            .sections
            .iter()
            .any(|s| s.kind == SectionKind::PriorFailures));
        assert!(pack
            .sections
            .iter()
            .any(|s| s.kind == SectionKind::Objective));
    }

    #[test]
    fn compile_is_deterministic_given_identical_inputs() {
        let ticket = base_ticket();
        let view = ProjectView::empty();
        let dir = tempfile::tempdir().expect("tempdir");
        let ci = open_empty_codeintel(dir.path());
        let budget = TokenBudget::even(5_000);

        let first = compile(
            &ticket,
            &view,
            &ci,
            budget.clone(),
            SignalWeights::default(),
            &[],
        )
        .expect("first compile succeeds");
        let second = compile(&ticket, &view, &ci, budget, SignalWeights::default(), &[])
            .expect("second compile succeeds");

        assert_eq!(first, second);
    }

    #[test]
    fn compile_reports_no_drops_when_nothing_overflows() {
        let ticket = base_ticket();
        let view = ProjectView::empty();
        let dir = tempfile::tempdir().expect("tempdir");
        let ci = open_empty_codeintel(dir.path());

        let pack = compile(
            &ticket,
            &view,
            &ci,
            TokenBudget::even(10_000),
            SignalWeights::default(),
            &["use tabs".to_string()],
        )
        .expect("compile succeeds");

        assert!(pack.dropped.is_empty());
        let conventions = pack
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::Conventions)
            .expect("conventions section admitted under a generous budget");
        assert!(conventions.body.contains("use tabs"));
    }
}
