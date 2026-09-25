//! Ticketmaster Context: the crate that stops the rest of the system from paying twice for
//! what it already knows.
//!
//! Two jobs, per `SPEC.md` §8:
//!
//! 1. **Context compilation** ([`pack`], [`sections`]): build a bounded, deterministic context
//!    pack for a ticket out of retrieval, decisions, dependency outputs, symbol outlines, git
//!    history and prior failures, instead of dumping raw files at a worker. The pack has a
//!    fixed token budget ([`tokens`]); when the assembled sections don't fit, the
//!    lowest-priority section is dropped first and the drop is recorded in the pack's
//!    provenance, never silently truncated.
//! 2. **Command artifacts** ([`command`]): an expensive command runs at most once per distinct
//!    `(argv, cwd, env, repo state, declared inputs)` key ([`fingerprint`]). Its full stdout
//!    and stderr are stored as artifacts; later questions about the output (head, tail, grep,
//!    line range, JSON pointer) are answered by querying the stored artifact, never by
//!    re-executing.
//!
//! Both jobs are pure functions of their inputs wherever possible: no wall-clock reads, no
//! random ids. Time comes from an injected `&dyn tm_types::Clock`, ids from an injected
//! `&dyn tm_types::IdSource`, so compilation and caching are exactly reproducible in tests.
//!
//! See `SPEC.md` §8.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod command;
pub mod fingerprint;
pub mod pack;
pub mod sections;
pub mod skills;
pub mod tokens;

pub use command::{
    query_output as query_command_output, ArtifactStream, CommandCache, CommandExecutor,
    CommandResult, CommandSpec, ExecutionOutcome, Query as CommandQuery, QueryAnswer,
};
pub use fingerprint::{
    cache_key, repo_fingerprint, CacheKeyInputs, DeclaredInput, EnvAllowlist, GitInspector,
    RepoFingerprint,
};
pub use pack::{
    compile, compile_session, ContextPack, DroppedSection, ProvenanceRef, Section, ToolSurfaceCost,
};
pub use sections::{
    build_conventions, build_decisions, build_dependencies, build_git_history, build_objective,
    build_prior_failures, build_retrieval, build_symbol_outlines, RawSection, SectionItem,
};
pub use skills::{discover_skills, load_skill, SkillDoc, SkillMetadata};
pub use tokens::{
    estimate_tokens_prose, estimate_tokens_source, BudgetLedger, SectionAccount, SectionKind,
    TokenBudget,
};

use std::fmt::Write as _;

impl SectionKind {
    /// A short, person-facing label for this section kind — plain words, not the enum's own
    /// `Debug`/`{:?}` spelling (`SectionKind::SymbolOutlines` etc.), which
    /// [`ContextPack::rent_report`](crate::pack::ContextPack::rent_report) prints and which is
    /// fine for a developer-facing rent report but is exactly the "internal enum name" `tm
    /// ticket context`'s plain-words output must not surface (CLAUDE.md's voice rules: "no
    /// internal type, enum or struct names").
    pub fn plain_label(self) -> &'static str {
        match self {
            SectionKind::Objective => "Ticket objective",
            SectionKind::Budget => "Budget status",
            SectionKind::Decisions => "Active decisions",
            SectionKind::Dependencies => "Dependency outputs",
            SectionKind::Retrieval => "Search hits",
            SectionKind::Wiki => "Wiki pages",
            SectionKind::SymbolOutlines => "Symbol outlines",
            SectionKind::GitHistory => "Git history",
            SectionKind::PriorFailures => "Prior failures",
            SectionKind::Conventions => "Project conventions",
        }
    }
}

/// Render a [`ContextPack`] the way a person reads it: which kinds of material were prefetched,
/// each with a rough token count, then anything left out to fit the budget — `tm ticket context
/// <ID>`'s plain-words form (`docs/tasks/TASKS.md`'s `p1-cli-ticket-context-command`). Kept here
/// rather than only in the CLI so both `tm ticket context` and any future caller share one
/// rendering, per that task's "keep the rendering logic shared where practical" note. Deliberately
/// not just [`ContextPack::rent_report`]: that report prints `{:?}` enum/string debug forms
/// (`SectionKind::SymbolOutlines`, quoted titles) meant for a developer, not the plain-language
/// voice this repo's person-facing output uses.
pub fn render_context_pack_plain(ticket: &tm_types::TicketId, pack: &ContextPack) -> String {
    let mut out = String::new();
    if pack.sections.is_empty() {
        let _ = writeln!(out, "No context was prefetched for ticket {ticket}.");
        return out;
    }
    let _ = writeln!(
        out,
        "Context for ticket {ticket}: {} tokens across {} section{}.",
        pack.tokens,
        pack.sections.len(),
        if pack.sections.len() == 1 { "" } else { "s" }
    );
    for section in &pack.sections {
        let _ = writeln!(
            out,
            "  {} ({}) — ~{} tokens, {} bytes",
            section.kind.plain_label(),
            section.title,
            section.tokens,
            section.bytes
        );
    }
    if !pack.dropped.is_empty() {
        let _ = writeln!(out, "Left out to fit the budget:");
        for dropped in &pack.dropped {
            let _ = writeln!(
                out,
                "  {} — would have needed ~{} tokens ({})",
                dropped.kind.plain_label(),
                dropped.tokens_needed,
                dropped.reason
            );
        }
    }
    out
}

/// Serialize a [`ContextPack`] into the shape `tm ticket context <ID> --json` prints: every
/// admitted section (kind, title, token/byte cost, body) in priority order, plus dropped sections
/// and pack totals. `ContextPack`/[`Section`]/[`DroppedSection`] derive no `serde::Serialize`
/// (`pack.rs` keeps its fields plain Rust types, and isn't this crate's `lib.rs`), so this is the
/// one hand-built conversion point rather than every caller re-deriving the same shape. Every
/// field read here (`pack.sections`, `section.kind`/`title`/`tokens`/`bytes`/`body`, `pack.dropped`,
/// `dropped.kind`/`reason`/`tokens_needed`/`bytes_needed`) is `pub`, so this is a mechanical
/// projection, not a re-derivation of pack.rs's own logic. [`SectionKind`] itself does derive
/// `Serialize` (snake_case), so it serializes as e.g. `"symbol_outlines"`, not a `{:?}` form.
pub fn context_pack_to_json(pack: &ContextPack) -> serde_json::Value {
    serde_json::json!({
        "tokens": pack.tokens,
        "bytes": pack.bytes,
        "sections": pack.sections.iter().map(|s| serde_json::json!({
            "kind": s.kind,
            "title": s.title,
            "tokens": s.tokens,
            "bytes": s.bytes,
            "body": s.body,
        })).collect::<Vec<_>>(),
        "dropped": pack.dropped.iter().map(|d| serde_json::json!({
            "kind": d.kind,
            "reason": d.reason,
            "tokens_needed": d.tokens_needed,
            "bytes_needed": d.bytes_needed,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> tm_types::TicketId {
        tm_types::TicketId::new("T-1").expect("literal id matches T-n")
    }

    fn pack_with(sections: Vec<Section>, dropped: Vec<DroppedSection>) -> ContextPack {
        let tokens = sections.iter().map(|s| s.tokens).sum();
        let bytes = sections.iter().map(|s| s.bytes).sum();
        ContextPack {
            sections,
            tokens,
            bytes,
            provenance: Vec::new(),
            dropped,
        }
    }

    fn section(kind: SectionKind, title: &str, tokens: usize, bytes: usize) -> Section {
        Section {
            kind,
            title: title.to_string(),
            body: "x".repeat(bytes),
            tokens,
            bytes,
            provenance: Vec::new(),
        }
    }

    #[test]
    fn plain_render_lists_every_admitted_section_with_a_token_count_and_no_debug_forms() {
        let pack = pack_with(
            vec![
                section(SectionKind::SymbolOutlines, "src/lib.rs", 40, 120),
                section(SectionKind::Retrieval, "hybrid search", 20, 60),
            ],
            Vec::new(),
        );
        let rendered = render_context_pack_plain(&ticket(), &pack);
        assert!(rendered.contains("Symbol outlines"));
        assert!(rendered.contains("Search hits"));
        assert!(rendered.contains("40 tokens"));
        assert!(rendered.contains("20 tokens"));
        assert!(
            !rendered.contains("SymbolOutlines"),
            "must not leak the enum's own Debug spelling: {rendered}"
        );
    }

    #[test]
    fn plain_render_lists_dropped_sections_separately_from_admitted_ones() {
        let pack = pack_with(
            vec![section(SectionKind::Objective, "objective", 10, 30)],
            vec![DroppedSection {
                kind: SectionKind::PriorFailures,
                reason: "over budget".to_string(),
                tokens_needed: 500,
                bytes_needed: 1500,
            }],
        );
        let rendered = render_context_pack_plain(&ticket(), &pack);
        assert!(rendered.contains("Left out to fit the budget"));
        assert!(rendered.contains("Prior failures"));
        assert!(rendered.contains("500 tokens"));
    }

    #[test]
    fn plain_render_says_so_when_nothing_was_prefetched() {
        let pack = pack_with(Vec::new(), Vec::new());
        let rendered = render_context_pack_plain(&ticket(), &pack);
        assert!(rendered.contains("No context was prefetched"));
    }

    #[test]
    fn json_form_carries_real_section_data_not_an_empty_shell() {
        let pack = pack_with(
            vec![section(SectionKind::GitHistory, "recent commits", 15, 45)],
            Vec::new(),
        );
        let json = context_pack_to_json(&pack);
        assert_eq!(json["tokens"], 15);
        assert_eq!(json["sections"][0]["kind"], "git_history");
        assert_eq!(json["sections"][0]["title"], "recent commits");
        assert_eq!(json["sections"][0]["tokens"], 15);
    }

    #[test]
    fn json_form_lists_dropped_sections_too() {
        let pack = pack_with(
            Vec::new(),
            vec![DroppedSection {
                kind: SectionKind::Conventions,
                reason: "over budget".to_string(),
                tokens_needed: 12,
                bytes_needed: 36,
            }],
        );
        let json = context_pack_to_json(&pack);
        assert_eq!(json["dropped"][0]["kind"], "conventions");
        assert_eq!(json["dropped"][0]["tokens_needed"], 12);
    }
}
