//! `tm ticket context <ID>`: makes a ticket's prefetched context pack and token cost observable
//! (`docs/tasks/TASKS.md`'s `p1-cli-ticket-context-command`).
//!
//! `tm_context::ContextPack` already carries sections (outlines/symbols/history/search hits) and
//! a `rent_report`, and `crate::dispatch::ProjectContextPackSource::compile` already builds one
//! per dispatched attempt — but that call site immediately renders it to a flat `String`
//! (`tm_agent::render_task_prompt`) for `ExecutorTask::context_pack`, so nothing keeps the
//! structured pack around for a person to inspect afterward. Rather than persisting a new
//! artifact per attempt (an extra `artifact.created` on every run, for a value `tm_context::pack
//! ::compile` is a pure function of its inputs — this crate's own `lib.rs` doc comment — and can
//! just as honestly be recomputed on demand), [`compile_for_ticket`] recompiles it against the
//! ticket's *current* state, using the exact same inputs `ProjectContextPackSource::compile`
//! does. That means `tm ticket context <ID>` shows the context the ticket's *next* attempt would
//! be given, not a frozen record of a specific past attempt — the one honest caveat: if the
//! ticket has failed an attempt since, that failure now shows up under `PriorFailures` too.
//!
//! [`ticket_context`] is the command handler, matching the shape every other `tm ticket <sub>`
//! handler in `crates/tm-cli/src/tickets.rs` uses (e.g. `ticket_show`, taking `args.ticket`
//! along with `&Project` and `&Renderer`). It is wired to `tm ticket context <ID>` via a
//! `TicketCommand::Context` variant in `crates/tm-cli/src/args.rs` and a matching dispatch arm
//! in `tickets.rs`'s `dispatch_ticket`.

use std::fmt::Write as _;

use tm_types::TicketId;

use crate::args::TicketRefArgs;
use crate::project::Project;
use crate::render::Renderer;

/// Recompile the [`tm_context::ContextPack`] ticket `ticket`'s next attempt would be given —
/// identical inputs to [`crate::dispatch::ProjectContextPackSource::compile`] (kept in sync
/// deliberately: a future change to that compile call's arguments should change here too),
/// stopping short of that function's final `render_task_prompt` flattening step so the structured
/// pack survives for a person to inspect.
pub fn compile_for_ticket(
    project: &Project,
    ticket: &TicketId,
) -> tm_types::Result<tm_context::ContextPack> {
    let view = project.store.view()?;
    let ticket_state = view
        .tickets
        .get(ticket)
        .ok_or_else(|| tm_types::TmError::not_found("ticket", ticket))?;
    let ci = project.code_intel()?;
    tm_context::compile(
        ticket_state,
        &view,
        &ci,
        tm_context::TokenBudget::even(10_000),
        tm_codeintel::SignalWeights::default(),
        &[],
        &crate::ops::load_role_table_for_state_dir(&project.state_dir)?,
    )
}

/// `tm ticket context <ID>`'s plain-words form: which kinds of material were prefetched, with a
/// rough token count each.
pub fn render_plain(ticket: &TicketId, pack: &tm_context::ContextPack) -> String {
    tm_context::render_context_pack_plain(ticket, pack)
}

/// `tm ticket context <ID> --json`'s form: the real `ContextPack` section data.
pub fn render_json(pack: &tm_context::ContextPack) -> serde_json::Value {
    tm_context::context_pack_to_json(pack)
}

/// `tm ticket context <ID>`: recompiles `args.ticket`'s context pack (see this module's doc
/// comment) and prints it — plain words naming the ticket by id and objective the way the
/// product does (`tm ticket show`'s own convention), or the real `ContextPack` section data under
/// `--json`.
pub fn ticket_context(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    let view = project.store.view()?;
    let objective = view
        .tickets
        .get(&ticket_id)
        .ok_or_else(|| tm_types::TmError::not_found("ticket", &ticket_id))?
        .objective
        .clone();
    let pack = compile_for_ticket(project, &ticket_id)?;

    let json = render_json(&pack);
    let mut human = String::new();
    let _ = writeln!(
        human,
        "Context ticket {ticket_id} ({objective}) will get on its next attempt:"
    );
    human.push_str(&render_plain(&ticket_id, &pack));
    renderer.emit(&json, human.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use tm_core::{ExecutorRequirements, RetryPolicy, Store, TicketKind, VerificationPolicy};
    use tm_types::{
        Authority, Budget, Clock, CounterIds, FixedClock, IdSource, ParticipantId, Role, Tolerance,
    };

    fn test_project(root: &std::path::Path) -> Project {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store =
            Arc::new(Store::open_with(root, clock.clone(), ids.clone()).expect("open store"));
        Project::for_test(root, store, clock, ids)
    }

    fn create_ticket(project: &Project, objective: &str) -> TicketId {
        let events = project
            .store
            .create_ticket(
                TicketKind::Investigation,
                objective.to_string(),
                None,
                None,
                Authority::root(),
                Vec::new(),
                ExecutorRequirements {
                    role: Role::CoderFast,
                    human_required: false,
                    min_capability: Tolerance::Preferred,
                },
                Vec::new(),
                Vec::new(),
                VerificationPolicy::None,
                Budget::new(200_000, 2_000_000, 600),
                RetryPolicy {
                    max_attempts: 1,
                    base_delay_seconds: 0,
                    backoff_multiplier: 1.0,
                    max_delay_seconds: 0,
                },
                0,
                ParticipantId::new("human:tester").expect("valid participant id"),
            )
            .expect("create ticket");
        events
            .iter()
            .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
            .expect("ticket.created event")
    }

    #[test]
    fn compile_for_ticket_returns_a_pack_with_at_least_the_objective_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let ticket = create_ticket(&project, "add a widget to the toolbar");

        let pack = compile_for_ticket(&project, &ticket).expect("compile");
        assert!(
            !pack.sections.is_empty(),
            "a freshly created ticket must still get at least its own objective section"
        );
        assert!(pack
            .sections
            .iter()
            .any(|s| s.kind == tm_context::SectionKind::Objective));
    }

    #[test]
    fn compile_for_ticket_errors_clearly_for_an_unknown_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let missing = TicketId::new("T-999").expect("valid ticket id shape");

        let err = compile_for_ticket(&project, &missing).expect_err("no such ticket");
        assert!(matches!(err, tm_types::TmError::NotFound { .. }));
    }

    #[test]
    fn render_plain_and_render_json_agree_on_the_same_compiled_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let ticket = create_ticket(&project, "fix the login redirect");

        let pack = compile_for_ticket(&project, &ticket).expect("compile");
        let plain = render_plain(&ticket, &pack);
        let json = render_json(&pack);

        assert!(plain.contains(ticket.to_string().as_str()));
        assert_eq!(json["tokens"], pack.tokens);
        assert_eq!(
            json["sections"].as_array().expect("array").len(),
            pack.sections.len()
        );
    }

    #[test]
    fn ticket_context_succeeds_for_a_real_ticket_and_fails_clearly_for_a_missing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let ticket = create_ticket(&project, "fix the login redirect");
        let renderer = Renderer::new(false, true, true, false);

        ticket_context(
            &TicketRefArgs {
                ticket: ticket.to_string(),
            },
            &project,
            &renderer,
        )
        .expect("a real ticket must render");

        let err = ticket_context(
            &TicketRefArgs {
                ticket: "T-999".to_string(),
            },
            &project,
            &renderer,
        )
        .expect_err("no such ticket");
        assert!(matches!(err, tm_types::TmError::NotFound { .. }));
    }
}
