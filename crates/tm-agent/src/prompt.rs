//! System and task prompt assembly.
//!
//! What a model sees is derived entirely from harness config prompt fragments
//! (`tm_harness::config::PromptFragments`) plus the compiled context pack
//! (`tm_context::ContextPack`) — no hidden personality, no ad hoc string literals sprinkled
//! through the agent loop. Rendering is a pure function of its inputs, deterministic byte for
//! byte, so a snapshot test can pin the exact rendered text and catch accidental drift.

use tm_context::ContextPack;
use tm_harness::config::PromptFragments;
use tm_types::TicketId;

/// The two prompt strings a provider call needs: `system` (goes on
/// `tm_provider::CompletionRequest::system`) and `task` (the first user-turn message body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPrompt {
    /// The assembled system prompt.
    pub system: String,
    /// The assembled task/context prompt for the initial user turn.
    pub task: String,
}

/// Render the system prompt from `fragments` and the tool policy summary.
///
/// # Rendering contract (pinned by snapshot tests)
/// Output is, in order: `fragments.system_preamble`, a blank line, a fixed
/// `"You operate under a gated authority. Denied actions are returned to you as tool results;
/// adapt rather than retry the same call."` line, a blank line, then `fragments.closing_reminder`.
/// `fragments.extra` is not included here (callers splice named fragments into `render_task_prompt`
/// or their own templates as needed) so this function's output depends only on the two named
/// fields every role shares, keeping the pin stable across harness configs that only add
/// `extra` entries.
pub fn render_system_prompt(fragments: &PromptFragments) -> String {
    format!(
        "{}\n\nYou operate under a gated authority. Denied actions are returned to you as tool results; adapt rather than retry the same call.\n\n{}",
        fragments.system_preamble,
        fragments.closing_reminder
    )
}

/// Render the task prompt for `ticket` from its compiled `pack`.
///
/// # Rendering contract (pinned by snapshot tests)
/// Output is: a header line `"# Ticket <id>"`, then each of `pack.sections` in order as
/// `"## <title>\n<body>\n"`, then, if `pack.dropped` is non-empty, a trailing
/// `"## Omitted"` section listing one line per dropped section as
/// `"- <kind> (needed <tokens_needed> tokens): <reason>"`. This makes an agent aware that
/// material existed but didn't fit, rather than silently working from a partial picture.
pub fn render_task_prompt(ticket: &TicketId, pack: &ContextPack) -> String {
    format!("# Ticket {}\n{}", ticket, render_context_sections(pack))
}

/// [`render_task_prompt`] without its `# Ticket <id>` header line: each of `pack.sections` as
/// `"## <title>\n<body>\n"`, then the `## Omitted` list if anything was dropped. What a chat turn
/// with no ticket sends as its context.
pub fn render_context_sections(pack: &ContextPack) -> String {
    let mut output = String::new();

    for section in &pack.sections {
        output.push_str(&format!("## {}\n{}\n", section.title, section.body));
    }

    if !pack.dropped.is_empty() {
        output.push_str("## Omitted\n");
        for dropped in &pack.dropped {
            output.push_str(&format!(
                "- {:?} (needed {} tokens): {}\n",
                dropped.kind, dropped.tokens_needed, dropped.reason
            ));
        }
    }

    output
}

/// Render both prompts for a task in one call, the common entry point
/// [`crate::agent_loop::AgentLoop`] uses to build its first `tm_provider::CompletionRequest`.
pub fn render(
    ticket: Option<&TicketId>,
    pack: &ContextPack,
    fragments: &PromptFragments,
) -> RenderedPrompt {
    RenderedPrompt {
        system: render_system_prompt(fragments),
        task: match ticket {
            Some(ticket) => render_task_prompt(ticket, pack),
            None => render_context_sections(pack),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_context::{DroppedSection, Section};
    use tm_types::TicketId;

    #[test]
    fn test_render_system_prompt_basic() {
        let fragments = PromptFragments {
            system_preamble: "You are a helpful assistant.".to_string(),
            closing_reminder: "Remember to follow the rules.".to_string(),
            extra: Default::default(),
        };

        let result = render_system_prompt(&fragments);

        assert_eq!(
            result,
            "You are a helpful assistant.\n\nYou operate under a gated authority. Denied actions are returned to you as tool results; adapt rather than retry the same call.\n\nRemember to follow the rules."
        );
    }

    #[test]
    fn test_render_system_prompt_with_empty_preamble() {
        let fragments = PromptFragments {
            system_preamble: String::new(),
            closing_reminder: "Close".to_string(),
            extra: Default::default(),
        };

        let result = render_system_prompt(&fragments);

        assert_eq!(
            result,
            "\n\nYou operate under a gated authority. Denied actions are returned to you as tool results; adapt rather than retry the same call.\n\nClose"
        );
    }

    #[test]
    fn test_render_system_prompt_with_empty_reminder() {
        let fragments = PromptFragments {
            system_preamble: "Preamble".to_string(),
            closing_reminder: String::new(),
            extra: Default::default(),
        };

        let result = render_system_prompt(&fragments);

        assert_eq!(
            result,
            "Preamble\n\nYou operate under a gated authority. Denied actions are returned to you as tool results; adapt rather than retry the same call.\n\n"
        );
    }

    #[test]
    fn test_render_system_prompt_ignores_extra() {
        let extra = Default::default();
        let fragments = PromptFragments {
            system_preamble: "Preamble".to_string(),
            closing_reminder: "Close".to_string(),
            extra,
        };

        let result = render_system_prompt(&fragments);

        // Should not contain any "extra" content
        assert!(!result.contains("extra"));
        assert_eq!(
            result,
            "Preamble\n\nYou operate under a gated authority. Denied actions are returned to you as tool results; adapt rather than retry the same call.\n\nClose"
        );
    }

    #[test]
    fn test_render_task_prompt_header_only() {
        let ticket = TicketId::new("T-1").unwrap();
        let pack = ContextPack {
            sections: vec![],
            tokens: 0,
            bytes: 0,
            provenance: vec![],
            dropped: vec![],
        };

        let result = render_task_prompt(&ticket, &pack);

        assert_eq!(result, "# Ticket T-1\n");
    }

    #[test]
    fn test_render_task_prompt_with_single_section() {
        let ticket = TicketId::new("T-42").unwrap();
        let pack = ContextPack {
            sections: vec![Section {
                kind: tm_context::tokens::SectionKind::Objective,
                title: "Objective".to_string(),
                body: "Fix the bug.".to_string(),
                tokens: 10,
                bytes: 10,
                provenance: vec![],
            }],
            tokens: 10,
            bytes: 10,
            provenance: vec![],
            dropped: vec![],
        };

        let result = render_task_prompt(&ticket, &pack);

        assert_eq!(result, "# Ticket T-42\n## Objective\nFix the bug.\n");
    }

    #[test]
    fn test_render_task_prompt_with_multiple_sections() {
        let ticket = TicketId::new("T-5").unwrap();
        let pack = ContextPack {
            sections: vec![
                Section {
                    kind: tm_context::tokens::SectionKind::Objective,
                    title: "Objective".to_string(),
                    body: "Do task A.".to_string(),
                    tokens: 10,
                    bytes: 10,
                    provenance: vec![],
                },
                Section {
                    kind: tm_context::tokens::SectionKind::Decisions,
                    title: "Decisions".to_string(),
                    body: "Decided on approach B.".to_string(),
                    tokens: 15,
                    bytes: 15,
                    provenance: vec![],
                },
            ],
            tokens: 25,
            bytes: 25,
            provenance: vec![],
            dropped: vec![],
        };

        let result = render_task_prompt(&ticket, &pack);

        assert_eq!(
            result,
            "# Ticket T-5\n## Objective\nDo task A.\n## Decisions\nDecided on approach B.\n"
        );
    }

    #[test]
    fn test_render_task_prompt_with_dropped_sections() {
        let ticket = TicketId::new("T-99").unwrap();
        let pack = ContextPack {
            sections: vec![Section {
                kind: tm_context::tokens::SectionKind::Objective,
                title: "Objective".to_string(),
                body: "Task.".to_string(),
                tokens: 5,
                bytes: 5,
                provenance: vec![],
            }],
            tokens: 5,
            bytes: 5,
            provenance: vec![],
            dropped: vec![
                DroppedSection {
                    kind: tm_context::tokens::SectionKind::Retrieval,
                    reason: "exceeds remaining token budget".to_string(),
                    tokens_needed: 500,
                    bytes_needed: 500,
                },
                DroppedSection {
                    kind: tm_context::tokens::SectionKind::GitHistory,
                    reason: "exceeds remaining token budget".to_string(),
                    tokens_needed: 300,
                    bytes_needed: 300,
                },
            ],
        };

        let result = render_task_prompt(&ticket, &pack);

        assert!(result.starts_with("# Ticket T-99\n## Objective\nTask.\n"));
        assert!(result.contains("## Omitted\n"));
        assert!(
            result.contains("- Retrieval (needed 500 tokens): exceeds remaining token budget\n")
        );
        assert!(
            result.contains("- GitHistory (needed 300 tokens): exceeds remaining token budget\n")
        );
    }

    #[test]
    fn test_render_task_prompt_dropped_list_order() {
        let ticket = TicketId::new("T-1").unwrap();
        let pack = ContextPack {
            sections: vec![],
            tokens: 0,
            bytes: 0,
            provenance: vec![],
            dropped: vec![
                DroppedSection {
                    kind: tm_context::tokens::SectionKind::PriorFailures,
                    reason: "budget".to_string(),
                    tokens_needed: 100,
                    bytes_needed: 100,
                },
                DroppedSection {
                    kind: tm_context::tokens::SectionKind::Conventions,
                    reason: "budget".to_string(),
                    tokens_needed: 200,
                    bytes_needed: 200,
                },
            ],
        };

        let result = render_task_prompt(&ticket, &pack);

        // Check that dropped sections appear in the Omitted section in order
        let omitted_idx = result.find("## Omitted\n").unwrap();
        let prior_failures_idx = result.find("- PriorFailures").unwrap();
        let conventions_idx = result.find("- Conventions").unwrap();

        assert!(omitted_idx < prior_failures_idx);
        assert!(prior_failures_idx < conventions_idx);
    }

    #[test]
    fn test_render_full_prompts() {
        let ticket = TicketId::new("T-10").unwrap();
        let pack = ContextPack {
            sections: vec![Section {
                kind: tm_context::tokens::SectionKind::Objective,
                title: "What to do".to_string(),
                body: "Complete task X.".to_string(),
                tokens: 20,
                bytes: 20,
                provenance: vec![],
            }],
            tokens: 20,
            bytes: 20,
            provenance: vec![],
            dropped: vec![],
        };
        let fragments = PromptFragments {
            system_preamble: "System rules.".to_string(),
            closing_reminder: "Good luck.".to_string(),
            extra: Default::default(),
        };

        let rendered = render(Some(&ticket), &pack, &fragments);

        assert_eq!(
            rendered.system,
            "System rules.\n\nYou operate under a gated authority. Denied actions are returned to you as tool results; adapt rather than retry the same call.\n\nGood luck."
        );
        assert_eq!(
            rendered.task,
            "# Ticket T-10\n## What to do\nComplete task X.\n"
        );
    }

    #[test]
    fn test_render_task_prompt_multiline_body() {
        let ticket = TicketId::new("T-3").unwrap();
        let pack = ContextPack {
            sections: vec![Section {
                kind: tm_context::tokens::SectionKind::Objective,
                title: "Objective".to_string(),
                body: "Line 1\nLine 2\nLine 3".to_string(),
                tokens: 30,
                bytes: 30,
                provenance: vec![],
            }],
            tokens: 30,
            bytes: 30,
            provenance: vec![],
            dropped: vec![],
        };

        let result = render_task_prompt(&ticket, &pack);

        assert_eq!(
            result,
            "# Ticket T-3\n## Objective\nLine 1\nLine 2\nLine 3\n"
        );
    }

    #[test]
    fn test_render_task_prompt_empty_body() {
        let ticket = TicketId::new("T-2").unwrap();
        let pack = ContextPack {
            sections: vec![Section {
                kind: tm_context::tokens::SectionKind::Objective,
                title: "Empty".to_string(),
                body: String::new(),
                tokens: 0,
                bytes: 0,
                provenance: vec![],
            }],
            tokens: 0,
            bytes: 0,
            provenance: vec![],
            dropped: vec![],
        };

        let result = render_task_prompt(&ticket, &pack);

        // Empty body still gets a trailing newline from the section format
        assert_eq!(result, "# Ticket T-2\n## Empty\n\n");
    }

    #[test]
    fn test_render_task_prompt_validation_no_tickets() {
        let ticket = TicketId::new("V-7").unwrap();
        let pack = ContextPack {
            sections: vec![],
            tokens: 0,
            bytes: 0,
            provenance: vec![],
            dropped: vec![],
        };

        let result = render_task_prompt(&ticket, &pack);

        // Non-T- tickets should still render correctly
        assert_eq!(result, "# Ticket V-7\n");
    }
}
