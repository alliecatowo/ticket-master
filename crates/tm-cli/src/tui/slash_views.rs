//! Read-only text renderers for slash commands that answer with a notice in the chat transcript,
//! kept separate from `chat_ops.rs`'s session/UI plumbing so the text is a pure function of its
//! inputs and easy to unit test without a live `AgentSession`.

use tm_codeintel::hybrid::RankedHit;
use tm_context::SectionKind;
use tm_tui::chat::status::format_tokens;
use tm_types::TicketId;

use crate::agent::{ContextReport, AUTO_COMPACT_TOKENS};

/// The most hits `/search` prints, matching `tm search`'s own default feel without letting one
/// broad query flood the transcript.
const MAX_SEARCH_HITS: usize = 10;

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

/// `/search <query>`'s rendering: the top hits from the same hybrid search `tm search --mode
/// hybrid` uses (`crates/tm-cli/src/search.rs`), one per line as `path:line  snippet`.
///
/// `project_indexed` distinguishes "nothing has ever been indexed for this project" from "the
/// index is current and this query just has no matches" — the caller (`chat_ops.rs::run_search`)
/// computes it from whether the index database existed before this call, or from this call's own
/// `update_incremental` having just added files, so a project that has never run `tm doctor` (or
/// any prior search) is told to build the index instead of being shown an empty list that looks
/// identical to a real no-match.
pub(super) fn search_results(hits: &[RankedHit], project_indexed: bool) -> String {
    if hits.is_empty() {
        return if project_indexed {
            "No matches for that search.".to_string()
        } else {
            "This project has no indexed code yet. Run `tm doctor` to see why.".to_string()
        };
    }
    hits.iter()
        .take(MAX_SEARCH_HITS)
        .map(|hit| {
            let location = match hit.line_start {
                Some(line) => format!("{}:{line}", hit.path),
                None => hit.path.clone(),
            };
            let snippet = hit.snippet.lines().next().unwrap_or("").trim();
            format!("{location}  {snippet}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The most `git diff HEAD` characters `/review` embeds directly in its prompt, past which the
/// agent reads the rest itself with its own shell tool rather than the turn opening with an
/// enormous diff — mirrors `agent.rs`'s `MAX_RESULT_CHARS` truncation of a tool call's own
/// result, just with a larger budget since this is the turn's actual subject, not one call's log.
const MAX_REVIEW_DIFF_CHARS: usize = 20_000;

/// The prompt `/review [focus]` sends: `diff` is `git diff HEAD`'s own output
/// (`chat_ops.rs::run_review` runs it, the way `/init` runs nothing and instead hands the agent a
/// task — here the diff is the point, so it's read once here rather than asked of the agent a
/// second time as a tool call).
pub(super) fn review_prompt(focus: &str, diff: &str) -> String {
    let focus = focus.trim();
    let scoped = if focus.is_empty() {
        String::new()
    } else {
        format!(" Focus on {focus}.")
    };
    let (diff, truncated) = match diff.char_indices().nth(MAX_REVIEW_DIFF_CHARS) {
        Some((cut, _)) => (&diff[..cut], true),
        None => (diff, false),
    };
    let note = if truncated {
        "\n\n[diff truncated — read the rest yourself with `git diff HEAD` if you need it]"
    } else {
        ""
    };
    format!(
        "Review these uncommitted changes against HEAD:{scoped}\n\n```diff\n{diff}\n```{note}\n\n\
         Point out real bugs, correctness risks, and anything unfinished or inconsistent with \
         the rest of the codebase — not style nitpicks. Check `git status` for untracked files \
         that might belong in this change if that seems relevant."
    )
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

    fn hit(path: &str, line: u32, snippet: &str) -> RankedHit {
        RankedHit {
            path: path.to_string(),
            line_start: Some(line),
            line_end: Some(line),
            snippet: snippet.to_string(),
            fused_score: 1.0,
            explain: Vec::new(),
        }
    }

    #[test]
    fn search_results_lists_path_line_and_snippet() {
        let hits = vec![
            hit("src/graph.rs", 42, "fn build_dependency_graph() {"),
            hit("src/graph.rs", 60, "    graph.add_edge(a, b);"),
        ];
        let text = search_results(&hits, true);
        assert_eq!(
            text,
            "src/graph.rs:42  fn build_dependency_graph() {\n\
             src/graph.rs:60  graph.add_edge(a, b);"
        );
    }

    #[test]
    fn search_results_caps_at_ten_hits() {
        let hits: Vec<RankedHit> = (0..15).map(|i| hit("f.rs", i, "line")).collect();
        assert_eq!(search_results(&hits, true).lines().count(), MAX_SEARCH_HITS);
    }

    #[test]
    fn search_results_distinguishes_no_matches_from_never_indexed() {
        assert_eq!(search_results(&[], true), "No matches for that search.");
        assert!(search_results(&[], false).contains("no indexed code yet"));
    }

    #[test]
    fn review_prompt_embeds_the_diff_and_an_optional_focus() {
        let plain = review_prompt("", "diff --git a/x b/x\n+added line\n");
        assert!(plain.contains("diff --git a/x b/x"));
        assert!(plain.contains("+added line"));
        assert!(!plain.contains("Focus on"));
        let focused = review_prompt("error handling", "diff --git a/x b/x\n");
        assert!(focused.contains("Focus on error handling."));
    }

    #[test]
    fn review_prompt_truncates_an_oversized_diff() {
        let huge = "x".repeat(MAX_REVIEW_DIFF_CHARS + 500);
        let text = review_prompt("", &huge);
        assert!(text.contains("[diff truncated"));
        assert!(
            text.len() < huge.len() + 500,
            "diff body itself was cut down"
        );
    }
}
