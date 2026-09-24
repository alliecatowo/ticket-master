//! Read-only text renderers for slash commands that answer with a notice in the chat transcript,
//! kept separate from `chat_ops.rs`'s session/UI plumbing so the text is a pure function of its
//! inputs and easy to unit test without a live `AgentSession`.

use tm_context::SectionKind;
use tm_tui::chat::status::format_tokens;
use tm_types::TicketId;

use crate::agent::{ContextReport, AUTO_COMPACT_TOKENS};

/// `/context`'s table: context-window size, then tokens by system prompt, instructions
/// (AGENTS.md), tools and conversation, computed from `report` and `total_tokens` (the last
/// turn's own `AgentSession::last_context_report`/`context_tokens`), rather than estimated fresh
/// here. `report` is `None` before any turn has run, in which case there is honestly nothing to
/// show yet. When `attached` is `Some`, a second block lists every section of the last-compiled
/// context pack (the ticket's prefetched material: outlines, symbols, history, search hits) with
/// its own token count, reusing `tm_context::Section::tokens` directly (`SPEC.md` §30.2's "an
/// unattributed context is a bug") — those sections sum to the block's own `Total` line by
/// construction (summed here, not read back off `ContextPack::tokens`). If `report`'s pack was
/// compiled for a different ticket than `attached` (or for none, a plain chat turn), that pack
/// would mislabel whichever ticket is attached now, so the block says to send a message instead
/// of printing stale numbers under the wrong ticket's name.
pub(super) fn context_table(
    report: Option<&ContextReport>,
    total_tokens: u64,
    attached: Option<&str>,
) -> String {
    let Some(report) = report else {
        return "Nothing measured yet in this session. Send a message, then run /context again."
            .to_string();
    };
    let instructions_tokens = report
        .context_pack
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Conventions)
        .map_or(0, |s| s.tokens as u64);
    let accounted = report.system_tokens + instructions_tokens + report.tools_tokens;
    // Everything the last call actually spent that isn't accounted for above: prior-turn
    // history, plus any prefetched section other than instructions (listed on its own below when
    // the pack matches the attached ticket). A remainder of a measured total, not a fresh guess.
    let conversation_tokens = total_tokens.saturating_sub(accounted);
    let free_tokens = AUTO_COMPACT_TOKENS.saturating_sub(total_tokens);

    let mut out = format!(
        "Context window: {} tokens (this session auto-compacts above it)",
        format_tokens(AUTO_COMPACT_TOKENS)
    );
    out.push_str(&row("System prompt", report.system_tokens));
    out.push_str(&row("Instructions (AGENTS.md)", instructions_tokens));
    out.push_str(&row("Tools", report.tools_tokens));
    out.push_str(&row("Conversation", conversation_tokens));
    out.push_str(&row("Free space", free_tokens));

    if let Some(attached) = attached {
        let pack_ticket = report.ticket.as_ref().map(TicketId::as_str);
        if pack_ticket != Some(attached) {
            out.push_str(&format!(
                "\n\nSend a message to see what's prefetched for {attached}."
            ));
        } else if report.context_pack.sections.is_empty() {
            out.push_str(&format!(
                "\n\nNothing was prefetched for {attached} on the last turn."
            ));
        } else {
            out.push_str(&format!("\n\nPrefetched for {attached}:"));
            let mut sum = 0u64;
            for section in &report.context_pack.sections {
                let tokens = section.tokens as u64;
                sum += tokens;
                out.push_str(&row(&section.title, tokens));
            }
            out.push_str(&row("Total", sum));
        }
    }

    out
}

/// One `label: value` line, indented and column-aligned the same way in both the main table and
/// the per-ticket section list below it.
fn row(label: &str, tokens: u64) -> String {
    format!("\n  {label:<26} {:>7}", format_tokens(tokens))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_context::{ContextPack, Section};

    fn section(kind: SectionKind, title: &str, tokens: usize) -> Section {
        Section {
            kind,
            title: title.to_string(),
            body: String::new(),
            tokens,
            bytes: tokens * 4,
            provenance: Vec::new(),
        }
    }

    fn pack(sections: Vec<Section>) -> ContextPack {
        let tokens = sections.iter().map(|s| s.tokens).sum();
        let bytes = sections.iter().map(|s| s.bytes).sum();
        ContextPack {
            sections,
            tokens,
            bytes,
            provenance: Vec::new(),
            dropped: Vec::new(),
        }
    }

    fn ticket(id: &str) -> TicketId {
        TicketId::new(id).expect("valid ticket id")
    }

    #[test]
    fn no_report_says_so_instead_of_inventing_numbers() {
        let text = context_table(None, 0, None);
        assert!(text.contains("Nothing measured yet"));
    }

    #[test]
    fn table_breaks_down_the_measured_tokens() {
        let report = ContextReport {
            system_tokens: 500,
            tools_tokens: 300,
            ticket: None,
            context_pack: pack(vec![section(SectionKind::Conventions, "Conventions", 100)]),
        };
        let text = context_table(Some(&report), 2_000, None);
        assert!(text.contains(&format!(
            "Context window: {}",
            format_tokens(AUTO_COMPACT_TOKENS)
        )));
        assert!(text.contains(&row("System prompt", 500)));
        assert!(text.contains(&row("Instructions (AGENTS.md)", 100)));
        assert!(text.contains(&row("Tools", 300)));
        // 2000 - (500 + 100 + 300) = 1100 left over as conversation.
        assert!(text.contains(&row("Conversation", 1_100)));
        assert!(!text.contains("Prefetched for"));
    }

    #[test]
    fn attached_ticket_lists_sections_summing_to_the_pack_total() {
        let report = ContextReport {
            system_tokens: 500,
            tools_tokens: 300,
            ticket: Some(ticket("T-4")),
            context_pack: pack(vec![
                section(SectionKind::Objective, "Objective", 50),
                section(SectionKind::Retrieval, "Retrieval", 150),
                section(SectionKind::Conventions, "Conventions", 100),
            ]),
        };
        let text = context_table(Some(&report), 2_000, Some("T-4"));
        assert!(text.contains("Prefetched for T-4:"));
        assert!(text.contains(&row("Objective", 50)));
        assert!(text.contains(&row("Retrieval", 150)));
        // The three sections sum to 300, and the Total row is that same sum, not a separately
        // trusted `ContextPack::tokens` that could in principle disagree with it.
        assert!(text.contains(&row("Total", 300)));
    }

    #[test]
    fn empty_pack_says_nothing_was_prefetched() {
        let report = ContextReport {
            system_tokens: 500,
            tools_tokens: 300,
            ticket: Some(ticket("T-1")),
            context_pack: pack(vec![]),
        };
        let text = context_table(Some(&report), 800, Some("T-1"));
        assert!(text.contains("Nothing was prefetched for T-1"));
    }

    #[test]
    fn stale_pack_from_a_different_ticket_is_not_shown_as_the_attached_ticket_s() {
        // The last turn ran against T-4 (or no ticket at all); the conversation has since
        // attached to T-9. Printing T-4's sections under "Prefetched for T-9" would be wrong.
        let report = ContextReport {
            system_tokens: 500,
            tools_tokens: 300,
            ticket: Some(ticket("T-4")),
            context_pack: pack(vec![section(SectionKind::Objective, "Objective", 50)]),
        };
        let text = context_table(Some(&report), 800, Some("T-9"));
        assert!(text.contains("Send a message to see what's prefetched for T-9"));
        assert!(!text.contains("Prefetched for T-9"));
        assert!(!text.contains("Objective"));
    }
}
