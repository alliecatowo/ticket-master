//! [`ContextPack`]: the bounded, deterministic bundle [`compile`] hands to a worker instead of
//! raw files (`SPEC.md` §8.1). Sections are assembled in [`crate::tokens::SectionKind`]
//! priority order and admitted against a [`crate::tokens::TokenBudget`] via a
//! [`crate::tokens::BudgetLedger`]; whatever doesn't fit is dropped starting from the
//! lowest-priority section, and every drop is recorded in [`ContextPack::dropped`] rather than
//! silently truncated. Given the same `(ticket, view, codeintel-state, budget)`, `compile`
//! always produces byte-identical output, so the pack is snapshot-testable.

use std::fmt::Write as _;

use tm_codeintel::{CodeIntel, SignalWeights};
use tm_core::{ProjectView, Ticket};
use tm_provider::RoleTable;
use tm_types::Result;

use crate::sections;
use crate::tokens::{
    estimate_tokens_prose, estimate_tokens_source, BudgetLedger, SectionKind, TokenBudget,
};

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
    /// Bytes of `body`, so a consumer can see the raw wire cost alongside the token estimate
    /// (`SPEC.md` §30.2's "per-section byte and token breakdown").
    pub bytes: usize,
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
    /// How many bytes the section's rendered body would have needed to be admitted in full.
    pub bytes_needed: usize,
}

/// The compiled, bounded context handed to a worker for one ticket.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPack {
    /// Sections that were admitted, in priority order.
    pub sections: Vec<Section>,
    /// Total tokens actually spent across `sections`.
    pub tokens: usize,
    /// Total bytes across `sections` (sum of each [`Section::bytes`]).
    pub bytes: usize,
    /// Provenance for every admitted section's content, flattened across sections in the same
    /// order they appear in `sections`.
    pub provenance: Vec<ProvenanceRef>,
    /// Sections that were dropped to fit the budget, lowest-priority first.
    pub dropped: Vec<DroppedSection>,
}

/// The byte/token cost of one tool's schema on the wire (`SPEC.md` §30.2): a tool definition is
/// re-sent on every single turn just like a pack's sections are, so it needs the same
/// attribution rather than being an invisible, uncounted cost. Lives in this crate (not
/// `tm-agent`, which owns the actual tool catalog) so [`ContextPack::rent_report`] can accept it
/// without `tm-context` depending back on `tm-agent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSurfaceCost {
    /// The tool's dotted wire name (e.g. `"fs.read"`).
    pub tool: String,
    /// Bytes of `name` + `description` + the compact-serialized `input_schema`, i.e. the parts
    /// of a `tm_provider::ToolDef` that cost bytes on the wire — excludes surrounding JSON
    /// framing (field names, commas) shared with the rest of the request.
    pub bytes: usize,
    /// Tokens estimated for the same payload, via [`estimate_tokens_source`] for the schema
    /// and [`estimate_tokens_prose`] for the description.
    pub tokens: usize,
}

impl ToolSurfaceCost {
    /// Compute one tool definition's cost from its wire-shape parts.
    pub fn compute(name: &str, description: &str, input_schema: &serde_json::Value) -> Self {
        let schema_text = input_schema.to_string();
        let bytes = name.len() + description.len() + schema_text.len();
        let tokens = estimate_tokens_source(name)
            + estimate_tokens_prose(description)
            + estimate_tokens_source(&schema_text);
        ToolSurfaceCost {
            tool: name.to_string(),
            bytes,
            tokens,
        }
    }
}

impl ContextPack {
    /// A per-section byte/token breakdown, so bloat is visible rather than inferred
    /// (`SPEC.md` §30.2: "An unattributed context is a bug"). Every admitted section is listed
    /// with its cost; every dropped section is listed too, clearly marked, with the cost it
    /// would have needed. When `tool_surface` is non-empty, its per-tool costs are listed as a
    /// second block and folded into a combined per-turn total, since a request's tool schemas
    /// (`SPEC.md` §30.1) are paid on every turn right alongside the pack itself.
    pub fn rent_report(&self, tool_surface: &[ToolSurfaceCost]) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "context pack: {} bytes, {} tokens across {} section(s)",
            self.bytes,
            self.tokens,
            self.sections.len()
        );
        for section in &self.sections {
            let _ = writeln!(
                out,
                "  {:?} {:?}: {} bytes, {} tokens",
                section.kind, section.title, section.bytes, section.tokens
            );
        }
        for dropped in &self.dropped {
            let _ = writeln!(
                out,
                "  DROPPED {:?}: needed {} bytes, {} tokens ({})",
                dropped.kind, dropped.bytes_needed, dropped.tokens_needed, dropped.reason
            );
        }
        if !tool_surface.is_empty() {
            let tool_bytes: usize = tool_surface.iter().map(|t| t.bytes).sum();
            let tool_tokens: usize = tool_surface.iter().map(|t| t.tokens).sum();
            let _ = writeln!(
                out,
                "tool surface: {} bytes, {} tokens across {} tool(s)",
                tool_bytes,
                tool_tokens,
                tool_surface.len()
            );
            for tool in tool_surface {
                let _ = writeln!(
                    out,
                    "  {}: {} bytes, {} tokens",
                    tool.tool, tool.bytes, tool.tokens
                );
            }
            let _ = writeln!(
                out,
                "total per-turn cost: {} bytes, {} tokens",
                self.bytes + tool_bytes,
                self.tokens + tool_tokens
            );
        }
        out
    }
}

/// Compile a [`ContextPack`] for `ticket`.
///
/// Sections are built in [`SectionKind::PRIORITY_ORDER`] and admitted whole-or-dropped against
/// a [`BudgetLedger`] derived from `budget`: on overflow the lowest-priority section is the
/// first to lose out, and every drop is recorded in [`ContextPack::dropped`] rather than
/// silently truncating a section's body. Deterministic given identical `(ticket, view,
/// ci-index-state, budget, roles)`, since every section builder is a pure function of its
/// inputs. `roles` prices [`SectionKind::Budget`]'s tier menu (`SPEC.md` §31.1,
/// `docs/audit-2026-09-18-fable.md` B-10) — pass `RoleTable::default_table()` when no
/// project-specific `providers.toml` is loaded.
///
/// Each section's rendered body is redacted for secret-shaped substrings (`tm_auth::redact`,
/// the pure/keyless form — see its docs) before it is charged against the budget or admitted:
/// a pack is both handed straight to a worker as prompt content (a `CompletionRequest` built
/// from it reaches `Fabric::execute`'s own redaction, but only *after* whatever already leaked
/// in here) and, when persisted (a snapshot artifact), goes through
/// `Store::store_artifact`'s redaction too late to matter if the plaintext already made it this
/// far. `docs/audit-2026-09-18-fable.md`'s "M-04" names "a context pack" explicitly as one of
/// the durable-persistence redaction targets; this is that boundary. Redacting here rather than
/// only downstream keeps this deterministic (`redact` is pure/keyless, so this does not break
/// the byte-identical guarantee above) and means `rent_report`'s byte/token accounting reflects
/// what a section actually costs once redacted, not its pre-redaction size.
pub fn compile(
    ticket: &Ticket,
    view: &ProjectView,
    ci: &CodeIntel,
    budget: TokenBudget,
    weights: SignalWeights,
    conventions: &[String],
    roles: &RoleTable,
) -> Result<ContextPack> {
    let raw_sections: Vec<(SectionKind, sections::RawSection)> = vec![
        (SectionKind::Objective, sections::build_objective(ticket)),
        (SectionKind::Budget, sections::build_budget(ticket, roles)),
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
            SectionKind::Wiki,
            sections::build_wiki(ticket, ci, weights)?,
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

    for (kind, mut raw) in raw_sections {
        raw.body = tm_auth::redact(&raw.body);
        let cost = estimate_tokens_prose(&raw.body);
        let byte_cost = raw.body.len();
        if ledger.spend(kind, cost) {
            provenance.extend(raw.provenance.iter().cloned());
            sections_out.push(Section {
                kind,
                title: raw.title,
                body: raw.body,
                tokens: cost,
                bytes: byte_cost,
                provenance: raw.provenance,
            });
        } else {
            dropped.push(DroppedSection {
                kind,
                reason: "exceeds remaining token budget".to_string(),
                tokens_needed: cost,
                bytes_needed: byte_cost,
            });
        }
    }

    let tokens = ledger.total_used();
    let bytes = sections_out.iter().map(|s| s.bytes).sum();

    Ok(ContextPack {
        sections: sections_out,
        tokens,
        bytes,
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
            &RoleTable::default_table(),
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

    /// The regression gate for M-04's "secret redaction ... a context pack": a real
    /// `ticket.objective` (the field a human most plausibly pastes a stray credential into)
    /// carrying a known fake-secret-shaped canary, compiled through the real [`compile`], must
    /// not reach any admitted section's body.
    #[test]
    fn compile_redacts_a_secret_shaped_substring_in_the_objective() {
        const CANARY: &str = "sk-PACKCANARY0123456789abcdefghijklmnopqr";

        let mut ticket = base_ticket();
        ticket.objective = format!("Rotate the leaked key {CANARY} in the config");
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
            &RoleTable::default_table(),
        )
        .expect("compile succeeds");

        for section in &pack.sections {
            assert!(
                !section.body.contains(CANARY),
                "canary leaked into {:?} section body: {}",
                section.kind,
                section.body
            );
        }
        let objective = pack
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::Objective)
            .expect("objective section admitted");
        assert!(
            objective.body.contains("<redacted:api_key>"),
            "{}",
            objective.body
        );
    }

    /// The false-positive half of the same gate: a legitimate long identifier this codebase
    /// already generates (a blake3 content hash, with no `key`/`token`/`secret`/`password`
    /// label nearby) must survive `compile` unredacted, and the ticket id in the objective
    /// section's own provenance must survive too.
    #[test]
    fn compile_does_not_redact_a_real_content_hash_or_the_ticket_id() {
        let content_hash = blake3::hash(b"some real file contents")
            .to_hex()
            .to_string();
        let mut ticket = base_ticket();
        ticket.objective = format!("Verify the build against content hash {content_hash}");
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
            &RoleTable::default_table(),
        )
        .expect("compile succeeds");

        let objective = pack
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::Objective)
            .expect("objective section admitted");
        assert!(
            objective.body.contains(&content_hash),
            "a real content hash must survive redaction unchanged: {}",
            objective.body
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
        // conventions) so it costs 0 tokens and is admitted regardless of share, except
        // `Budget` (always renders remaining spend/burn rate/tier menu, so it always costs
        // something) — only `Objective` gets a nonzero allotment, forcing both `Budget`'s and
        // `PriorFailures`' nonzero-cost bodies to be dropped.
        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Objective, 1.0);
        let budget = TokenBudget {
            total: 1_000,
            shares,
        };

        let pack = compile(
            &ticket,
            &view,
            &ci,
            budget,
            SignalWeights::default(),
            &[],
            &RoleTable::default_table(),
        )
        .expect("compile still succeeds when a section is dropped");

        assert_eq!(pack.dropped.len(), 2);
        assert!(pack
            .dropped
            .iter()
            .any(|d| d.kind == SectionKind::PriorFailures
                && d.reason == "exceeds remaining token budget"
                && d.tokens_needed > 0));
        assert!(pack
            .dropped
            .iter()
            .any(|d| d.kind == SectionKind::Budget && d.tokens_needed > 0));
        assert!(!pack
            .sections
            .iter()
            .any(|s| s.kind == SectionKind::PriorFailures));
        assert!(!pack.sections.iter().any(|s| s.kind == SectionKind::Budget));
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
            &RoleTable::default_table(),
        )
        .expect("first compile succeeds");
        let second = compile(
            &ticket,
            &view,
            &ci,
            budget,
            SignalWeights::default(),
            &[],
            &RoleTable::default_table(),
        )
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
            &RoleTable::default_table(),
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

    #[test]
    fn compile_reports_per_section_bytes_consistent_with_the_pack_total() {
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
            &RoleTable::default_table(),
        )
        .expect("compile with a generous budget succeeds");

        assert!(!pack.sections.is_empty());
        for section in &pack.sections {
            assert_eq!(section.bytes, section.body.len());
        }
        assert_eq!(
            pack.bytes,
            pack.sections.iter().map(|s| s.bytes).sum::<usize>()
        );
    }

    #[test]
    fn compile_reports_bytes_needed_for_a_dropped_section() {
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

        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Objective, 1.0);
        let budget = TokenBudget {
            total: 1_000,
            shares,
        };

        let pack = compile(
            &ticket,
            &view,
            &ci,
            budget,
            SignalWeights::default(),
            &[],
            &RoleTable::default_table(),
        )
        .expect("compile still succeeds when a section is dropped");

        // Both `PriorFailures` (nonzero body from the pushed failure) and `Budget` (always
        // renders something) are dropped here; only `Objective` has a nonzero share.
        assert_eq!(pack.dropped.len(), 2);
        let dropped = pack
            .dropped
            .iter()
            .find(|d| d.kind == SectionKind::PriorFailures)
            .expect("PriorFailures was dropped");
        assert!(dropped.bytes_needed > 0);
    }

    #[test]
    fn rent_report_lists_every_admitted_section_with_its_cost() {
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
            &RoleTable::default_table(),
        )
        .expect("compile with a generous budget succeeds");

        let report = pack.rent_report(&[]);
        for section in &pack.sections {
            assert!(
                report.contains(&format!("{} bytes", section.bytes)),
                "report is missing {:?}'s byte cost:\n{report}",
                section.kind
            );
            assert!(
                report.contains(&format!("{} tokens", section.tokens)),
                "report is missing {:?}'s token cost:\n{report}",
                section.kind
            );
        }
    }

    #[test]
    fn rent_report_clearly_marks_dropped_sections() {
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

        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Objective, 1.0);
        let budget = TokenBudget {
            total: 1_000,
            shares,
        };

        let pack = compile(
            &ticket,
            &view,
            &ci,
            budget,
            SignalWeights::default(),
            &[],
            &RoleTable::default_table(),
        )
        .expect("compile still succeeds when a section is dropped");

        let report = pack.rent_report(&[]);
        assert!(report.contains("DROPPED PriorFailures"));
        assert!(report.contains("DROPPED Budget"));
        assert!(!report.contains("DROPPED Objective"));
    }

    #[test]
    fn rent_report_folds_in_the_tool_surface_and_a_combined_total() {
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
            &RoleTable::default_table(),
        )
        .expect("compile with a generous budget succeeds");

        let tool_surface = vec![
            ToolSurfaceCost::compute(
                "fs.read",
                "Read a repository-relative file's full text content.",
                &serde_json::json!({"type": "object"}),
            ),
            ToolSurfaceCost::compute(
                "shell.run",
                "Run a command.",
                &serde_json::json!({"type": "object"}),
            ),
        ];
        let tool_bytes: usize = tool_surface.iter().map(|t| t.bytes).sum();
        let tool_tokens: usize = tool_surface.iter().map(|t| t.tokens).sum();

        let report = pack.rent_report(&tool_surface);
        assert!(report.contains("fs.read"));
        assert!(report.contains("shell.run"));
        assert!(report.contains(&format!(
            "total per-turn cost: {} bytes, {} tokens",
            pack.bytes + tool_bytes,
            pack.tokens + tool_tokens
        )));
    }

    #[test]
    fn tool_surface_cost_compute_is_nonzero_for_a_nonempty_tool() {
        let cost = ToolSurfaceCost::compute(
            "fs.read",
            "Read a repository-relative file's full text content.",
            &serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        );
        assert_eq!(cost.tool, "fs.read");
        assert!(cost.bytes > 0);
        assert!(cost.tokens > 0);
    }
}
