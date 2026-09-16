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
// IMPL: pure string concatenation per the contract above; no dynamic values (clock/ids) may
// appear here since this is exactly the function snapshot tests pin byte-for-byte.
pub fn render_system_prompt(fragments: &PromptFragments) -> String {
    todo!("concatenate fragments.system_preamble, the fixed authority-gating line, and fragments.closing_reminder per the rendering contract")
}

/// Render the task prompt for `ticket` from its compiled `pack`.
///
/// # Rendering contract (pinned by snapshot tests)
/// Output is: a header line `"# Ticket <id>"`, then each of `pack.sections` in order as
/// `"## <title>\n<body>\n"`, then, if `pack.dropped` is non-empty, a trailing
/// `"## Omitted"` section listing one line per dropped section as
/// `"- <kind> (needed <tokens_needed> tokens): <reason>"`. This makes an agent aware that
/// material existed but didn't fit, rather than silently working from a partial picture.
// IMPL: deterministic given `ticket` and `pack` only (both already fully resolved, no I/O);
// section kinds render via their `Debug`/display form consistently with how `tm-context`
// itself names them, so this function must not invent its own section-name strings.
pub fn render_task_prompt(ticket: &TicketId, pack: &ContextPack) -> String {
    todo!("render header, each admitted section, and an Omitted summary for dropped sections per the rendering contract")
}

/// Render both prompts for a task in one call, the common entry point
/// [`crate::agent_loop::AgentLoop`] uses to build its first `tm_provider::CompletionRequest`.
pub fn render(
    ticket: &TicketId,
    pack: &ContextPack,
    fragments: &PromptFragments,
) -> RenderedPrompt {
    RenderedPrompt {
        system: render_system_prompt(fragments),
        task: render_task_prompt(ticket, pack),
    }
}
