//! [`ContextPack`]: the bounded, deterministic bundle [`compile`] hands to a worker instead of
//! raw files (`SPEC.md` §8.1). Sections are assembled in [`crate::tokens::SectionKind`]
//! priority order and admitted against a [`crate::tokens::TokenBudget`] via a
//! [`crate::tokens::BudgetLedger`]. An atomic section (no [`sections::RawSection::items`]) is
//! admitted or dropped whole; a retrieval-like section (search hits, wiki pages, symbol
//! outlines, git history, conventions) is fit to its share top-k, admitting as many of its
//! items as fit — trimming the boundary item's own content by the line, if even a truncated
//! head of it fits — rather than being dropped in full over one item that doesn't. Whatever
//! doesn't fit, at the section or item level, is recorded in [`ContextPack::dropped`] rather
//! than silently omitted. Given the same `(ticket, view, codeintel-state, budget)` *and* the same
//! on-disk `AGENTS.md`/`.tm/skills/**` content beneath `ci`'s project root, `compile` always
//! produces byte-identical output, so the pack is snapshot-testable — but as of
//! `docs/decisions/D-013-hooks-agents-skills.md`, `SectionKind::Conventions` reads those two
//! filesystem sources directly (not through `ci`'s index, so `codeintel-state` does not capture
//! them), making them a sixth real input this doc comment's parenthetical previously omitted:
//! editing an `AGENTS.md` between two `compile` calls changes the pack with none of the other
//! five inputs changing. Ordering within that content stays deterministic (sorted skill paths,
//! first-seen-order `AGENTS.md` dedup) — only the input set itself gained a filesystem
//! dependency.

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

/// A record of content that did not fit and was left out, so a consumer can see *that* something
/// was omitted rather than silently receiving a partial picture. For an atomic section (no
/// [`sections::RawSection::items`]), this is the whole section. For a retrieval-like section
/// admitted item by item (see [`assemble`]), it's only the items that didn't fit after the rest
/// of the section was admitted — the admitted prefix itself still appears in
/// [`ContextPack::sections`], not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedSection {
    /// Which section this drop is for.
    pub kind: SectionKind,
    /// Why the content was left out: a budget-overflow reason for an atomic section
    /// (`"exceeds remaining token budget"`), or how many of a retrieval-like section's items
    /// were trimmed (`"trimmed N of M items to fit"`) — the field is a `String` rather than a
    /// fixed enum so future drop reasons — e.g. an upstream fetch error a caller chooses to
    /// downgrade to "dropped" instead of failing the whole pack — don't need a signature change.
    pub reason: String,
    /// How many tokens the left-out content would have needed: the whole section, for an atomic
    /// drop, or just the items left out, for a retrieval-like section's partial drop.
    pub tokens_needed: usize,
    /// How many bytes the left-out content's rendered form would have needed, same scope as
    /// `tokens_needed`.
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
/// Sections are built in [`SectionKind::PRIORITY_ORDER`] and admitted against a [`BudgetLedger`]
/// derived from `budget` (see [`assemble`] for how an atomic section is admitted-or-dropped
/// whole while a retrieval-like section is fit to its share top-k): on overflow the
/// lowest-priority section is the first to lose out, and every drop is recorded in
/// [`ContextPack::dropped`] rather than silently omitted. Deterministic given identical
/// `(ticket, view,
/// ci-index-state, budget, roles)` and identical on-disk `AGENTS.md`/`.tm/skills/**` content
/// under `ci.project_root()` — see this module's doc comment for why that filesystem content is
/// a sixth real input, not covered by `ci-index-state`, since [`crate::sections::
/// build_conventions`] reads it directly rather than through `ci`'s index. Every section
/// builder is still a pure function of its *declared* inputs; `build_conventions` is simply the
/// one builder whose declared inputs include "whatever `AGENTS.md`/`.tm/skills/**` say right
/// now". `roles` prices [`SectionKind::Budget`]'s tier menu (`SPEC.md` §31.1,
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
            sections::build_conventions(ticket, ci, conventions),
        ),
    ];

    Ok(assemble(raw_sections, &budget))
}

/// Compile the context for a chat turn that has no ticket
/// (`docs/decisions/D-017-session-ticket-executor-model.md`): retrieval and wiki hits for `query`
/// (the human's message), the project's conventions (every `AGENTS.md`, discovered skills, and
/// `conventions`), and an index of the project's open tickets — the tickets running in the
/// background that this session can steer. Sections that only mean something for a specific
/// ticket (its objective, budget, dependencies, prior failures, path-scoped decisions, claimed-path
/// outlines and history) are left out rather than rendered empty.
pub fn compile_session(
    query: &str,
    view: &ProjectView,
    ci: &CodeIntel,
    budget: TokenBudget,
    weights: SignalWeights,
    conventions: &[String],
) -> Result<ContextPack> {
    let carrier = query_carrier(query)?;
    let raw_sections: Vec<(SectionKind, sections::RawSection)> = vec![
        (
            SectionKind::Dependencies,
            sections::build_open_tickets(view),
        ),
        (
            SectionKind::Retrieval,
            sections::build_retrieval(&carrier, ci, weights)?,
        ),
        (
            SectionKind::Wiki,
            sections::build_wiki(&carrier, ci, weights)?,
        ),
        (
            SectionKind::Conventions,
            sections::build_conventions(&carrier, ci, conventions),
        ),
    ];
    let raw_sections = raw_sections
        .into_iter()
        .filter(|(_, raw)| !raw.body.trim().is_empty())
        .collect();
    Ok(assemble(raw_sections, &budget))
}

/// A never-persisted [`Ticket`] whose only meaningful field is `objective = query`, so the
/// query-driven section builders (retrieval, wiki, conventions) can be reused verbatim for a
/// ticketless turn. It claims no paths and is never written to the store.
fn query_carrier(query: &str) -> Result<Ticket> {
    use tm_core::{ExecutorRequirements, RetryPolicy, TicketKind, TicketState, VerificationPolicy};
    Ok(Ticket {
        id: tm_types::TicketId::new("T-0")?,
        kind: TicketKind::Investigation,
        objective: query.to_string(),
        state: TicketState::Draft,
        parent: None,
        children: Vec::new(),
        dependencies: Vec::new(),
        milestone: None,
        due: None,
        authority: tm_types::Authority::none(),
        resources: Vec::new(),
        executor: ExecutorRequirements {
            role: tm_types::Role::CoderFast,
            human_required: false,
            min_capability: tm_types::Tolerance::Any,
        },
        context_refs: Vec::new(),
        success: Vec::new(),
        verification: VerificationPolicy::None,
        budget: tm_types::Budget::none(),
        retry: RetryPolicy {
            max_attempts: 1,
            base_delay_seconds: 0,
            backoff_multiplier: 1.0,
            max_delay_seconds: 0,
        },
        cycle: None,
        attempts: 0,
        failures: Vec::new(),
        priority: 0,
        created: tm_types::Timestamp::from_unix_nanos(0),
        updated: tm_types::Timestamp::from_unix_nanos(0),
    })
}

/// Try to fit a line-truncated head of `text` within `remaining` tokens, so one oversized item
/// (a long `AGENTS.md`, a snippet that still ran long after [`sections`]'s own per-item caps)
/// doesn't force [`assemble`] to drop it — and everything after it — entirely just because it
/// alone doesn't fit what's left of a section's share. Takes whole lines only, appending a
/// `"…truncated"` marker line when any lines were cut, and returns `None` when not even one line
/// fits. Pure, total.
fn trim_item_to_remaining(text: &str, remaining: usize) -> Option<String> {
    if remaining == 0 {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    for n in (1..=lines.len()).rev() {
        let mut candidate = lines[..n].join("\n");
        if n < lines.len() {
            candidate.push_str("\n…truncated");
        }
        if estimate_tokens_prose(&candidate) <= remaining {
            return Some(candidate);
        }
    }
    None
}

/// Spend `budget` across `raw_sections` in order, keeping each section that fits and recording
/// each one that doesn't as dropped. Shared by [`compile`] and [`compile_session`].
///
/// A section whose [`sections::RawSection::items`] is empty is admitted or dropped whole, as
/// before this function grew item-level accounting. A section with items (a retrieval-like
/// section — search hits, wiki pages, symbol outlines, git history, conventions) is instead
/// admitted item by item, in order, until one no longer fits its remaining share: whatever
/// admitted first becomes the section (never dropped, even if only a prefix fit). The first item
/// that doesn't fit whole gets one more chance via [`trim_item_to_remaining`] — a line-truncated
/// head of it, if even that fits what's left — before assembly gives up on the section; this
/// keeps a single oversized item (this repo's own `CLAUDE.md`, admitted as a `Conventions` item,
/// is exactly this shape) from forcing the whole section to drop rather than admitting a partial
/// head of it. Whatever still doesn't fit after that is recorded as a single [`DroppedSection`]
/// naming how many of the section's items were left out entirely — "top-k until full" rather
/// than "whole or nothing" for these sections.
fn assemble(
    raw_sections: Vec<(SectionKind, sections::RawSection)>,
    budget: &TokenBudget,
) -> ContextPack {
    let mut ledger = BudgetLedger::new(budget);
    let mut sections_out = Vec::with_capacity(raw_sections.len());
    let mut provenance = Vec::new();
    let mut dropped = Vec::new();

    for (kind, mut raw) in raw_sections {
        if raw.items.is_empty() {
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
            continue;
        }

        for item in &mut raw.items {
            item.text = tm_auth::redact(&item.text);
        }

        let total_items = raw.items.len();
        let mut admitted_texts = Vec::with_capacity(total_items);
        let mut admitted_provenance = Vec::new();
        let mut used_tokens = 0usize;
        let mut admitted_count = 0usize;

        for (idx, item) in raw.items.iter().enumerate() {
            let cost = estimate_tokens_prose(&item.text);
            if ledger.spend(kind, cost) {
                admitted_texts.push(item.text.clone());
                admitted_provenance.extend(item.provenance.iter().cloned());
                used_tokens += cost;
                admitted_count = idx + 1;
            } else {
                let remaining = ledger
                    .accounts
                    .iter()
                    .find(|a| a.kind == kind)
                    .map(|a| a.remaining())
                    .unwrap_or(0);
                if let Some(trimmed) = trim_item_to_remaining(&item.text, remaining) {
                    let trimmed_cost = estimate_tokens_prose(&trimmed);
                    if ledger.spend(kind, trimmed_cost) {
                        admitted_texts.push(trimmed);
                        admitted_provenance.extend(item.provenance.iter().cloned());
                        used_tokens += trimmed_cost;
                        admitted_count = idx + 1;
                    }
                }
                break;
            }
        }

        if admitted_count == 0 {
            let tokens_needed: usize = raw
                .items
                .iter()
                .map(|item| estimate_tokens_prose(&item.text))
                .sum();
            let bytes_needed: usize = raw.items.iter().map(|item| item.text.len()).sum();
            dropped.push(DroppedSection {
                kind,
                reason: "exceeds remaining token budget".to_string(),
                tokens_needed,
                bytes_needed,
            });
            continue;
        }

        provenance.extend(admitted_provenance.iter().cloned());
        let body = admitted_texts.join("\n");
        let bytes = body.len();
        sections_out.push(Section {
            kind,
            title: raw.title,
            body,
            tokens: used_tokens,
            bytes,
            provenance: admitted_provenance,
        });

        if admitted_count < total_items {
            let leftover = &raw.items[admitted_count..];
            let tokens_needed: usize = leftover
                .iter()
                .map(|item| estimate_tokens_prose(&item.text))
                .sum();
            let bytes_needed: usize = leftover.iter().map(|item| item.text.len()).sum();
            dropped.push(DroppedSection {
                kind,
                reason: format!("trimmed {} of {} items to fit", leftover.len(), total_items),
                tokens_needed,
                bytes_needed,
            });
        }
    }

    let tokens = ledger.total_used();
    let bytes = sections_out.iter().map(|s| s.bytes).sum();

    ContextPack {
        sections: sections_out,
        tokens,
        bytes,
        provenance,
        dropped,
    }
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
            due: None,
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

    /// `docs/audit-2026-09-18-fable.md` M-04's "done looks like": AGENTS.md content in an
    /// *assembled context pack*, not merely in `sections::build_conventions`'s raw output —
    /// `compile` still budget-admits/drops and redacts every section, and `Conventions` is
    /// `SectionKind::PRIORITY_ORDER`'s lowest-priority (first-dropped) entry, so a section-level
    /// test alone would not catch a real AGENTS.md failing to survive that far. A generous
    /// budget here isolates "does the content make it into the pack at all" from budget sizing,
    /// which `sections::tests` already covers at the builder level.
    #[test]
    fn compile_includes_agents_md_content_in_the_assembled_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("crates/tm-foo/src")).expect("mkdir");
        std::fs::write(
            dir.path().join("crates/tm-foo/AGENTS.md"),
            "SENTINEL-AGENTS-MD-IN-ASSEMBLED-PACK: use tabs in this crate\n",
        )
        .expect("write AGENTS.md");

        let mut ticket = base_ticket();
        ticket.resources = vec![tm_core::ResourceClaim {
            paths: tm_types::PatternSet::parse(["crates/tm-foo/src/bar.rs".to_string()])
                .expect("valid pattern"),
            mode: tm_core::ResourceMode::Shared,
        }];
        let view = ProjectView::empty();
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

        let all_bodies: String = pack.sections.iter().map(|s| s.body.as_str()).collect();
        assert!(
            all_bodies.contains("SENTINEL-AGENTS-MD-IN-ASSEMBLED-PACK"),
            "AGENTS.md content did not survive into the assembled pack: {all_bodies}"
        );
    }

    /// Same bar as above, for SKILL.md: metadata (name/description) must reach the assembled
    /// pack, and the body must not — checked against every section's body concatenated, not
    /// just `Conventions`, so the assertion actually proves the body never leaked in via
    /// `Retrieval`/`Wiki`/any other section, not merely that `Conventions` alone omits it.
    #[test]
    fn compile_includes_skill_metadata_but_not_body_in_the_assembled_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".tm/skills/demo")).expect("mkdir");
        std::fs::write(
            dir.path().join(".tm/skills/demo/SKILL.md"),
            "---\nname: demo-skill\ndescription: does a demo thing\n---\nSENTINEL-SKILL-BODY-MUST-NOT-LEAK-INTO-PACK\n",
        )
        .expect("write SKILL.md");

        let ticket = base_ticket();
        let view = ProjectView::empty();
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

        let all_bodies: String = pack.sections.iter().map(|s| s.body.as_str()).collect();
        assert!(
            all_bodies.contains("demo-skill") && all_bodies.contains("does a demo thing"),
            "skill metadata did not survive into the assembled pack: {all_bodies}"
        );
        assert!(
            !all_bodies.contains("SENTINEL-SKILL-BODY-MUST-NOT-LEAK-INTO-PACK"),
            "skill body leaked into the assembled pack before any skill.load call: {all_bodies}"
        );
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

    /// u1-context-pack-fit-sections's core regression gate: a truncatable section
    /// ([`sections::RawSection::items`] non-empty) with 100 items and a budget that can only
    /// fit a prefix of them must be admitted *partially*, not dropped whole — the failure mode
    /// this task fixes (`docs/tasks/TASKS.md`'s "either admits a section whole or drops it").
    #[test]
    fn assemble_admits_a_hundred_item_section_partially_instead_of_dropping_it_whole() {
        use crate::sections::SectionItem;

        const N: usize = 100;
        let items: Vec<SectionItem> = (0..N)
            .map(|i| SectionItem {
                text: format!("hit-{i:03}: {}", "x".repeat(20)),
                provenance: vec![ProvenanceRef {
                    locator: format!("file{i}.rs:1"),
                    detail: "hybrid retrieval hit".to_string(),
                }],
            })
            .collect();
        // Every item is the same length, so its token cost is uniform; derive `k` (how many fit
        // a 200-token share) from that cost instead of hardcoding it (CLAUDE.md's
        // derive-from-cardinality convention), so this test tracks `estimate_tokens_prose` and
        // this fixture's text length rather than an independently-guessed number.
        let per_item_cost = estimate_tokens_prose(&items[0].text);
        let raw = sections::RawSection {
            kind: SectionKind::Retrieval,
            title: "Retrieval Results".to_string(),
            body: String::new(),
            provenance: Vec::new(),
            items,
        };

        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Retrieval, 1.0);
        let budget = TokenBudget { total: 200, shares };
        let k = budget.share_tokens(SectionKind::Retrieval) / per_item_cost;
        assert!(
            k > 0 && k < N,
            "fixture must fit some but not all items for this test to be meaningful: k={k}"
        );

        let pack = assemble(vec![(SectionKind::Retrieval, raw)], &budget);

        let section = pack
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::Retrieval)
            .expect("a prefix of the 100 items is admitted, not the whole section dropped");
        assert!(section.body.contains("hit-000"), "{}", section.body);
        assert!(
            section.body.contains(&format!("hit-{:03}", k - 1)),
            "expected exactly {k} items admitted: {}",
            section.body
        );
        assert!(
            !section.body.contains(&format!("hit-{k:03}")),
            "expected the {k}th item (0-indexed) not to be admitted: {}",
            section.body
        );

        let dropped = pack
            .dropped
            .iter()
            .find(|d| d.kind == SectionKind::Retrieval)
            .expect("the items that did not fit are recorded as dropped");
        assert_eq!(
            dropped.reason,
            format!("trimmed {} of {N} items to fit", N - k)
        );
        assert!(dropped.tokens_needed > 0);
        assert!(dropped.bytes_needed > 0);
    }

    /// The boundary case `assemble_admits_a_hundred_item_section_partially_instead_of_dropping_it_whole`
    /// doesn't exercise: a single oversized multi-line item (a long `AGENTS.md`, in practice)
    /// that doesn't fit its section's remaining share whole still gets a line-truncated head of
    /// itself admitted, via [`trim_item_to_remaining`], rather than the section falling back to
    /// fully empty.
    #[test]
    fn assemble_trims_a_single_oversized_multiline_item_to_its_remaining_share() {
        use crate::sections::SectionItem;

        let long_text = (0..500)
            .map(|i| format!("line {i:04} of a long AGENTS.md"))
            .collect::<Vec<_>>()
            .join("\n");
        let raw = sections::RawSection {
            kind: SectionKind::Conventions,
            title: "Conventions".to_string(),
            body: String::new(),
            provenance: Vec::new(),
            items: vec![SectionItem {
                text: long_text,
                provenance: vec![ProvenanceRef {
                    locator: "AGENTS.md".to_string(),
                    detail: String::new(),
                }],
            }],
        };

        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Conventions, 1.0);
        let budget = TokenBudget { total: 300, shares };

        let pack = assemble(vec![(SectionKind::Conventions, raw)], &budget);

        let section = pack
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::Conventions)
            .expect("a truncated head of the one oversized item is admitted, not nothing");
        assert!(!section.body.is_empty());
        assert!(section.body.ends_with("…truncated"), "{}", section.body);
        assert!(
            section.tokens <= budget.share_tokens(SectionKind::Conventions),
            "admitted content must fit the section's share: {} tokens for a {}-token share",
            section.tokens,
            budget.share_tokens(SectionKind::Conventions)
        );
        // No dropped record: the item was trimmed to fit, not left out — there is exactly one
        // item, and it *was* admitted (in truncated form).
        assert!(!pack
            .dropped
            .iter()
            .any(|d| d.kind == SectionKind::Conventions));
    }

    /// The complementary case: when every item fits, the section is admitted in full and no
    /// drop is recorded for it at all.
    #[test]
    fn assemble_admits_every_item_when_the_whole_section_fits() {
        use crate::sections::SectionItem;

        let items: Vec<SectionItem> = (0..5)
            .map(|i| SectionItem {
                text: format!("hit-{i}"),
                provenance: Vec::new(),
            })
            .collect();
        let raw = sections::RawSection {
            kind: SectionKind::Retrieval,
            title: "Retrieval Results".to_string(),
            body: String::new(),
            provenance: Vec::new(),
            items,
        };

        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Retrieval, 1.0);
        let budget = TokenBudget {
            total: 100_000,
            shares,
        };

        let pack = assemble(vec![(SectionKind::Retrieval, raw)], &budget);

        assert!(pack.dropped.is_empty());
        let section = &pack.sections[0];
        for i in 0..5 {
            assert!(section.body.contains(&format!("hit-{i}")));
        }
    }
}
