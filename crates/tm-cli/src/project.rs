//! Locating and opening a project, plus the commands that operate before or across a ticket's
//! lifecycle: `init`, `attach`, `genesis`, `status`, and `doctor`.
//!
//! A Ticketmaster project is any directory containing a `.tm/` directory (`.tm/project.db`, the
//! event log `tm-core::Store` opens over). [`locate`] walks up from a starting directory to find
//! one, the same convention `git` uses for `.git`. Every other command module in this crate is
//! handed an already-opened [`Project`] by `main.rs`; this module is where that opening happens.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tm_events::{Event, EventKind, EventLog};
use tm_types::{
    Clock, CounterIds, IdSource, ParticipantId, SystemClock, TicketId, Timestamp, TmError,
};

use crate::args::{AttachArgs, DoctorArgs, GenesisArgs, InitArgs, StatusArgs};
use crate::render::{Renderer, Table};

/// An opened project: the authoritative [`tm_core::Store`] plus the collaborators every command
/// needs to call into it (clock, id source, project root). Every execution module in this crate
/// takes `&Project` rather than reopening the store itself.
pub struct Project {
    /// The project root directory (the one containing `.tm/`).
    pub root: PathBuf,
    /// The authoritative store: event log plus materialized state.
    pub store: Arc<tm_core::Store>,
    /// Injected wall clock; `SystemClock` outside tests.
    pub clock: Arc<dyn Clock>,
    /// Injected id source; `CounterIds` restored from persisted counters outside tests.
    pub ids: Arc<dyn IdSource>,
    /// The participant identity `tm` acts as for commands issued from this process, e.g.
    /// `human:<local user>`.
    pub actor: ParticipantId,
}

impl Project {
    /// The code index for this project (`.tm/index.db`), opened on demand by commands that need
    /// it (`search`, `symbol`, `history`, `doctor`) rather than eagerly here.
    pub fn code_intel(&self) -> tm_types::Result<tm_codeintel::CodeIntel> {
        tm_codeintel::CodeIntel::open(&self.root)
    }
}

/// Walk up from `start` looking for a `.tm` directory, the way `git` looks for `.git`.
pub fn locate(start: &Path) -> tm_types::Result<PathBuf> {
    let mut dir = start.canonicalize()?;
    loop {
        if dir.join(".tm").is_dir() {
            return Ok(dir);
        }
        dir = match dir.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return Err(TmError::not_found("project", start.display())),
        };
    }
}

/// Resolve the local actor identity from the environment: `human:<$TM_ACTOR, else $USER, else
/// "unknown">`. Never reads the wall clock or the network to "detect" identity.
fn resolve_actor() -> tm_types::Result<ParticipantId> {
    let non_empty = |v: Result<String, std::env::VarError>| v.ok().filter(|s| !s.is_empty());
    let handle = non_empty(std::env::var("TM_ACTOR"))
        .or_else(|| non_empty(std::env::var("USER")))
        .unwrap_or_else(|| "unknown".to_string());
    ParticipantId::new(format!("human:{handle}"))
}

/// Open the project at `root` (as returned by [`locate`]) with the real wall clock and a
/// restored [`tm_types::CounterIds`], and resolve the local actor identity.
pub fn open(root: &Path) -> tm_types::Result<Project> {
    let store = Arc::new(tm_core::Store::open(root)?);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    // `tm_core::Store` exposes no accessor for its internal id source, so this restores an
    // equivalent one from the same high-water marks `Store::open` itself just read, for the
    // collaborators (scheduler, server) that need to mint ids outside a `Store` command.
    let counters = store.view()?.counters.clone();
    let ids: Arc<dyn IdSource> = Arc::new(CounterIds::with_counters(counters, 0));
    let actor = resolve_actor()?;
    Ok(Project {
        root: root.to_path_buf(),
        store,
        clock,
        ids,
        actor,
    })
}

/// Resolve which directory to operate on: `--project`, else [`locate`] from the current
/// directory.
pub fn resolve_project_dir(explicit: Option<&Path>) -> tm_types::Result<PathBuf> {
    match explicit {
        Some(p) => Ok(p.to_path_buf()),
        None => locate(&std::env::current_dir()?),
    }
}

/// Create a new project's `.tm/` directory at `dir`, refusing if one already exists there, and
/// return the canonicalized root. Shared by [`init`] and [`attach`] (which creates one first
/// when assimilating a repository with no project yet).
fn create_project_dir(dir: &Path) -> tm_types::Result<PathBuf> {
    if dir.join(".tm").is_dir() {
        return Err(TmError::conflict(format!(
            "{} already contains a .tm project",
            dir.display()
        )));
    }
    // Creates `.tm/project.db` and runs migrations, materializing the empty project.
    tm_core::Store::open(dir)?;
    Ok(dir.canonicalize()?)
}

/// `tm init`: create a new project's `.tm/` directory in `args.path` (default: the current
/// directory).
///
/// # Errors
/// `TmError::Conflict` if a `.tm` directory already exists there.
pub fn init(args: &InitArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let dir = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let root = create_project_dir(&dir)?;
    let human = format!(
        "initialized a new Ticketmaster project at {}",
        root.display()
    );
    renderer.emit(&serde_json::json!({ "root": root }), &human)
}

/// `tm attach [path]`: assimilate an existing repository into a project, creating one first if
/// `args.path` has no `.tm` yet.
pub fn attach(args: &AttachArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let dir = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let root = if dir.join(".tm").is_dir() {
        dir.canonicalize()?
    } else {
        create_project_dir(&dir)?
    };
    let project = open(&root)?;
    let report = tm_genesis::attach::attach_repository(
        &root,
        project.store.as_ref(),
        project.clock.as_ref(),
        project.actor.clone(),
    )?;
    let human = render_attach_report(&report);
    renderer.emit(&report, &human)
}

fn render_attach_report(report: &tm_genesis::attach::AttachReport) -> String {
    let mut sections = vec![
        format!("attached {}", report.project_root.display()),
        format!("root ticket: {}", report.root_ticket),
        format!(
            "indexed {} files, {} chunks, {} commits ingested (symbol graph built: {})",
            report.files_indexed,
            report.chunks_indexed,
            report.commits_ingested,
            report.symbol_graph_built
        ),
    ];
    if !report.docs.is_empty() {
        let rows = report
            .docs
            .iter()
            .map(|d| vec![d.path.clone(), format!("{:?}", d.kind)])
            .collect();
        sections.push(Table::new(vec!["doc".to_string(), "kind".to_string()], rows).render());
    }
    if !report.build_systems.is_empty() {
        let rows = report
            .build_systems
            .iter()
            .map(|b| vec![b.name.clone(), b.manifest_path.clone()])
            .collect();
        sections.push(
            Table::new(
                vec!["build system".to_string(), "manifest".to_string()],
                rows,
            )
            .render(),
        );
    }
    for t in &report.external_trackers {
        sections.push(format!("external tracker: {} ({})", t.name, t.evidence));
    }
    for c in &report.conventions {
        sections.push(format!("convention: {} ({})", c.text, c.evidence));
    }
    let blocking = report.blocking_open_questions();
    if !blocking.is_empty() {
        sections.push(format!(
            "{} open question(s) need a human before proceeding",
            blocking.len()
        ));
    }
    sections.join("\n\n")
}

/// Pure gating logic behind [`resolve_genesis_provider`]'s `ANTHROPIC_API_KEY` check, split out
/// so the presence check is testable without reading or writing the real process environment.
fn api_key_gate(key: Result<String, std::env::VarError>) -> tm_types::Result<()> {
    if key.is_ok() {
        Ok(())
    } else {
        Err(TmError::Provider(
            "ANTHROPIC_API_KEY is not set; Genesis needs a real model provider".to_string(),
        ))
    }
}

/// Resolve the Genesis seed prompt from `explicit` (the `--prompt` value), reading stdin when it
/// is `"-"` or absent (and stdin is not a terminal a human would be typing into blind).
fn resolve_genesis_prompt(
    explicit: Option<&str>,
    stdin_is_terminal: bool,
    mut stdin: impl Read,
) -> tm_types::Result<String> {
    let read_all = |r: &mut dyn Read| -> tm_types::Result<String> {
        let mut buf = String::new();
        r.read_to_string(&mut buf)?;
        Ok(buf)
    };
    match explicit {
        Some(text) if text != "-" => Ok(text.to_string()),
        Some(_) => read_all(&mut stdin),
        None if stdin_is_terminal => Err(TmError::parse(
            "no --prompt given and stdin is a terminal; pass --prompt <text> or pipe a prompt on stdin",
        )),
        None => read_all(&mut stdin),
    }
}

/// Run an async computation to completion on a dedicated single-threaded runtime, off the
/// current thread. Used by [`genesis`] and [`doctor`] so their public signatures stay
/// synchronous even though `GenesisDriver::advance` and `Backend::probe` are `async fn`s.
fn run_async<F, Fut, T>(f: F) -> tm_types::Result<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = tm_types::Result<T>>,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| TmError::storage(e.to_string()))?
            .block_on(f())
    })
    .join()
    .unwrap_or_else(|_| Err(TmError::invariant("background async task panicked")))
}

/// Resolve the single [`tm_provider::Provider`] [`genesis`] should use for this run.
///
/// Keeps the original behavior byte-for-byte when `ANTHROPIC_API_KEY` is set: build
/// `AnthropicProvider` from `RoleTable::default_table`'s `VisionFrontier` candidate, same as
/// before this function existed, and return `None` for the note (nothing changed, so genesis
/// stays silent about a choice that didn't move).
///
/// Only when `ANTHROPIC_API_KEY` is *not* set does this probe for a fallback: the three
/// zero-account local backends in [`tm_provider::LOCAL_PROVIDER_IDS`], in priority order, each a
/// single short-timeout `GET /v1/models` (`tm_provider::providers::local::PROBE_TIMEOUT`,
/// currently 750ms) — a brand-new user who has never heard of `ANTHROPIC_API_KEY` but happens to
/// have Ollama running locally should not be stuck with a bare "set this env var" error. The
/// first one that answers with at least one model pulled is used automatically (with a printed
/// note saying so, never silently); if none does, the error names what's actually true about
/// each local backend's state plus the fastest zero-cost next step, rather than only mentioning
/// Anthropic.
///
/// This gate (probe only when nothing else is configured) is deliberate: a slow-by-comparison
/// network probe has no business running on every `tm genesis` invocation that already has a
/// working `ANTHROPIC_API_KEY`.
fn resolve_genesis_provider(
    clock: Arc<dyn Clock>,
) -> tm_types::Result<(Arc<dyn tm_provider::Provider>, Option<String>)> {
    if api_key_gate(std::env::var("ANTHROPIC_API_KEY")).is_ok() {
        let table = tm_provider::RoleTable::default_table();
        let candidate = table
            .candidates_for(tm_types::Role::VisionFrontier)
            .first()
            .cloned()
            .ok_or_else(|| {
                TmError::invariant("default role table has no VisionFrontier candidate")
            })?;
        let model = tm_provider::ModelId::new(candidate.provider.clone(), candidate.model.clone());
        let provider = tm_provider::AnthropicProvider::from_env(model, clock)
            .map_err(|e| TmError::Provider(e.to_string()))?;
        return Ok((Arc::new(provider), None));
    }

    let probe_clock = clock.clone();
    let probes: Vec<(&'static str, tm_provider::LocalProbe)> = run_async(move || async move {
        let mut results = Vec::with_capacity(tm_provider::LOCAL_PROVIDER_IDS.len());
        for id in tm_provider::LOCAL_PROVIDER_IDS {
            let probe = tm_provider::Registry::probe_local(id, probe_clock.clone()).await;
            results.push((id, probe));
        }
        Ok(results)
    })?;

    if let Some((id, model)) = probes.iter().find_map(|(id, probe)| match probe {
        tm_provider::LocalProbe::Ready { first_model } => Some((*id, first_model.clone())),
        _ => None,
    }) {
        let candidate = tm_provider::RoleCandidate {
            provider: id.to_string(),
            model: model.clone(),
            max_concurrency: 1,
            degraded_ok: false,
            price: None,
            limits: tm_provider::role_config::Limits::unlimited(),
        };
        let provider = tm_provider::Registry::build_provider(&candidate, clock)
            .map_err(|e| TmError::Provider(e.to_string()))?;
        let note = format!(
            "ANTHROPIC_API_KEY is not set; using detected local provider `{id}` (model \
             `{model}`) for Genesis instead. Set ANTHROPIC_API_KEY to use Anthropic instead."
        );
        return Ok((provider, Some(note)));
    }

    let detail = probes
        .iter()
        .map(|(id, probe)| match probe {
            tm_provider::LocalProbe::Unreachable => format!("{id}: not running"),
            tm_provider::LocalProbe::ReachableNoModels => {
                format!("{id}: running, no models pulled yet")
            }
            tm_provider::LocalProbe::Ready { .. } => {
                unreachable!("a Ready probe would have returned above")
            }
        })
        .collect::<Vec<_>>()
        .join("; ");

    Err(TmError::Provider(format!(
        "ANTHROPIC_API_KEY is not set and no local model provider is reachable ({detail}). \
         Fastest zero-cost path: install Ollama (https://ollama.com) and run `ollama pull \
         <model>`, then re-run `tm genesis`. Or set ANTHROPIC_API_KEY, or (if you have a GitHub \
         account) run `gh auth login` and export GITHUB_TOKEN=$(gh auth token) to route through \
         GitHub Models instead."
    )))
}

/// `tm genesis [--prompt <text>|-]`: turn a prompt into a running project via the Genesis stage
/// driver.
pub fn genesis(args: &GenesisArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let prompt = resolve_genesis_prompt(
        args.prompt.as_deref(),
        std::io::stdin().is_terminal(),
        std::io::stdin(),
    )?;

    let dir = std::env::current_dir()?;
    let root = if dir.join(".tm").is_dir() {
        dir.canonicalize()?
    } else {
        create_project_dir(&dir)?
    };
    let project = open(&root)?;

    // A single candidate stands in for "the fabric": `GenesisDriver` takes one `Provider` for
    // its whole run (every stage, regardless of role), so there is no per-call routing decision
    // for a `Fabric` to make here. See `resolve_genesis_provider` for how that one provider gets
    // picked — unchanged (Anthropic via `RoleTable::default_table`) when `ANTHROPIC_API_KEY` is
    // set, falling back to a reachable zero-signup local provider when it is not.
    let (provider, fallback_note) = resolve_genesis_provider(project.clock.clone())?;
    if let Some(note) = &fallback_note {
        renderer.note(note);
    }

    // `GenesisState::project` is the only channel `GenesisDriver` has for threading the raw
    // prompt into `Stage::Seed`'s `analyze_prompt` call (it hands `state.project` straight to
    // it), so the seed prompt lives there rather than in a project name/slug.
    let initial_state = tm_genesis::GenesisState::new(prompt, project.clock.as_ref());
    let actor = project.actor.clone();
    let quiet = renderer.is_quiet();
    let progress = *renderer;

    let final_state = run_async(move || async move {
        let mut state = initial_state;
        let driver = tm_genesis::GenesisDriver::new(
            project.store.as_ref(),
            provider.as_ref(),
            project.clock.as_ref(),
            project.ids.as_ref(),
        );
        loop {
            if !quiet {
                progress.note(&format!("genesis: entering stage {:?}", state.stage));
            }
            if state.stage == tm_genesis::Stage::SteadyState {
                break;
            }
            state = driver.advance(&state, actor.clone()).await?;
        }
        Ok(state)
    })?;

    let human = format!(
        "genesis complete for {:?}: now at {:?}",
        final_state.project, final_state.stage
    );
    renderer.emit(&final_state, &human)
}

/// Open a fresh read handle onto `project`'s event log, independent of the one `project.store`
/// holds internally (which exposes no read accessor of its own).
fn open_event_log(project: &Project) -> tm_types::Result<EventLog> {
    let db_path = project.root.join(".tm").join("project.db");
    EventLog::open_with_clock(&db_path, project.clock.clone())
}

/// Read every event in the log, oldest first. Simple rather than clever: `tm status`/`tm doctor`
/// run against project-sized logs, not archives.
fn read_all_events(log: &EventLog) -> tm_types::Result<Vec<Event>> {
    const BATCH: usize = 1024;
    let mut out = Vec::new();
    let mut seq = 1u64;
    loop {
        let batch = log.read_from(seq, BATCH)?;
        if batch.is_empty() {
            break;
        }
        seq += batch.len() as u64;
        out.extend(batch);
    }
    Ok(out)
}

/// The "since you left" report: a snapshot a returning human can read in a few seconds and know
/// exactly what happened and what needs them.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    /// Tickets that closed since the report window started.
    pub closed: Vec<String>,
    /// Tickets newly blocked (retry exhausted, cycle budget spent, failure needing a human).
    pub blocked: Vec<String>,
    /// Tickets awaiting verification or audit right now.
    pub awaiting_review: Vec<String>,
    /// Decisions recorded since the window started.
    pub decisions: Vec<String>,
    /// Docs that went `Stale` since the window started.
    pub stale_docs: Vec<String>,
    /// Budget scopes that are exhausted or within a warn threshold of exhaustion.
    pub budget_warnings: Vec<String>,
    /// Approvals currently pending a human decision.
    pub pending_approvals: Vec<String>,
}

/// Extract the summary's `subject`/`decision` fields for readable prose, falling back to the raw
/// summary text if it isn't the JSON blob `Store::record_decision` writes.
fn decision_summary_text(summary: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(summary) {
        Ok(v) => {
            let subject = v.get("subject").and_then(|x| x.as_str()).unwrap_or("");
            let decision = v.get("decision").and_then(|x| x.as_str());
            match decision {
                Some(d) if !subject.is_empty() => format!("{subject}: {d}"),
                Some(d) => d.to_string(),
                None => summary.to_string(),
            }
        }
        Err(_) => summary.to_string(),
    }
}

fn budget_scope_label(scope: &tm_core::BudgetScope) -> String {
    match scope {
        tm_core::BudgetScope::Project => "project".to_string(),
        tm_core::BudgetScope::Milestone(id) => format!("milestone {id}"),
        tm_core::BudgetScope::Ticket(id) => format!("ticket {id}"),
        tm_core::BudgetScope::Lease(id) => format!("lease {id}"),
    }
}

/// A warn-or-worse note for one budget scope, or `None` when it has ample headroom left.
/// `u64::MAX`/`0` limits are treated as "not tracked" for that dimension (unlimited, or never
/// configured) rather than always-exhausted or always-warning.
fn budget_warning(scoped: &tm_core::budget::ScopedBudget) -> Option<String> {
    const WARN_RATIO: f64 = 0.8;
    let b = &scoped.budget;
    let dimensions = [
        ("tokens", b.spent.tokens, b.tokens),
        ("dollars", b.spent.dollars_micros, b.dollars_micros),
        ("wall time", b.spent.wall_seconds, b.wall_seconds),
    ];
    let mut notes = Vec::new();
    for (label, spent, limit) in dimensions {
        if limit == 0 || limit == u64::MAX {
            continue;
        }
        let ratio = spent as f64 / limit as f64;
        if ratio >= 1.0 {
            notes.push(format!("{label} exhausted ({spent}/{limit})"));
        } else if ratio >= WARN_RATIO {
            notes.push(format!(
                "{label} at {:.0}% ({spent}/{limit})",
                ratio * 100.0
            ));
        }
    }
    if notes.is_empty() {
        None
    } else {
        Some(format!(
            "{}: {}",
            budget_scope_label(&scoped.scope),
            notes.join(", ")
        ))
    }
}

/// The `ticket` field carried by whichever "review resolved" event kind `event` is, if any.
/// Used to drop a ticket out of `awaiting_review` once its submission has been settled.
fn review_resolution_ticket(payload: &tm_events::Payload) -> Option<TicketId> {
    payload
        .as_ticket_verified()
        .map(|p| p.ticket.clone())
        .or_else(|| {
            payload
                .as_ticket_verification_failed()
                .map(|p| p.ticket.clone())
        })
        .or_else(|| payload.as_ticket_audited().map(|p| p.ticket.clone()))
        .or_else(|| payload.as_ticket_audit_rejected().map(|p| p.ticket.clone()))
        .or_else(|| payload.as_ticket_closed().map(|p| p.ticket.clone()))
        .or_else(|| payload.as_ticket_cancelled().map(|p| p.ticket.clone()))
        .or_else(|| payload.as_ticket_reopened().map(|p| p.ticket.clone()))
}

/// Compose a [`StatusReport`] from the event log and project view. Split out from [`status`] so
/// the bucketing logic is directly assertable in tests without capturing rendered output.
fn build_status_report(project: &Project, args: &StatusArgs) -> tm_types::Result<StatusReport> {
    let log = open_event_log(project)?;
    let events = read_all_events(&log)?;
    let now = project.clock.now();

    let start = match args.since_hours {
        Some(hours) => now.plus_seconds(-(hours as i64) * 3600),
        None => events
            .iter()
            .rev()
            .find(|e| {
                e.kind == EventKind::PresenceUpdated
                    && e.payload
                        .as_presence_updated()
                        .map(|p| p.participant == project.actor)
                        .unwrap_or(false)
            })
            .or_else(|| events.first())
            .map(|e| e.ts)
            .unwrap_or(Timestamp::EPOCH),
    };

    let mut report = StatusReport {
        closed: Vec::new(),
        blocked: Vec::new(),
        awaiting_review: Vec::new(),
        decisions: Vec::new(),
        stale_docs: Vec::new(),
        budget_warnings: Vec::new(),
        pending_approvals: Vec::new(),
    };

    // "Awaiting review right now" and "still pending" reflect current state, built from the
    // whole log; the time-bounded buckets below only collect what happened inside the window.
    let mut awaiting_review: BTreeSet<TicketId> = BTreeSet::new();
    let mut pending_approvals: BTreeMap<TicketId, String> = BTreeMap::new();
    let mut untracked_approvals: Vec<String> = Vec::new();

    for event in &events {
        match event.kind {
            EventKind::TicketSubmitted => {
                if let Some(p) = event.payload.as_ticket_submitted() {
                    awaiting_review.insert(p.ticket.clone());
                }
            }
            EventKind::TicketVerified
            | EventKind::TicketVerificationFailed
            | EventKind::TicketAudited
            | EventKind::TicketAuditRejected
            | EventKind::TicketClosed
            | EventKind::TicketCancelled
            | EventKind::TicketReopened => {
                if let Some(ticket) = review_resolution_ticket(&event.payload) {
                    awaiting_review.remove(&ticket);
                }
            }
            EventKind::ApprovalRequested => {
                if let Some(p) = event.payload.as_approval_requested() {
                    match &p.ticket {
                        Some(ticket) => {
                            pending_approvals.insert(ticket.clone(), p.note.clone());
                        }
                        // No ticket to key on; the payload carries nothing else to correlate a
                        // later decision back to this request, so it can only ever be reported,
                        // never cleared.
                        None => untracked_approvals.push(p.note.clone()),
                    }
                }
            }
            EventKind::ApprovalDecided => {
                if let Some(p) = event.payload.as_approval_decided() {
                    if let Some(ticket) = &p.ticket {
                        pending_approvals.remove(ticket);
                    }
                }
            }
            _ => {}
        }

        if event.ts < start {
            continue;
        }

        match event.kind {
            EventKind::TicketClosed => {
                if let Some(p) = event.payload.as_ticket_closed() {
                    report.closed.push(p.ticket.to_string());
                }
            }
            EventKind::TicketEscalated => {
                if let Some(p) = event.payload.as_ticket_escalated() {
                    report
                        .blocked
                        .push(format!("{} escalated: {}", p.ticket, p.reason));
                }
            }
            EventKind::TicketBudgetExhausted => {
                if let Some(p) = event.payload.as_ticket_budget_exhausted() {
                    report.blocked.push(format!(
                        "{} budget exhausted ({}: {}/{})",
                        p.ticket, p.dimension, p.spent, p.limit
                    ));
                }
            }
            EventKind::DecisionCreated => {
                if let Some(p) = event.payload.as_decision_created() {
                    report.decisions.push(decision_summary_text(&p.summary));
                }
            }
            EventKind::DocInvalidated => {
                if let Some(p) = event.payload.as_doc_invalidated() {
                    report.stale_docs.push(format!("{} ({})", p.path, p.reason));
                }
            }
            _ => {}
        }
    }

    report.awaiting_review = awaiting_review.into_iter().map(|t| t.to_string()).collect();
    report.pending_approvals = pending_approvals
        .into_iter()
        .map(|(ticket, note)| format!("{ticket}: {note}"))
        .chain(untracked_approvals)
        .collect();

    for scoped in &project.store.view()?.budgets {
        if let Some(warning) = budget_warning(scoped) {
            report.budget_warnings.push(warning);
        }
    }

    Ok(report)
}

fn render_status_report(report: &StatusReport) -> String {
    let sections: Vec<String> = [
        ("Closed", &report.closed),
        ("Blocked", &report.blocked),
        ("Awaiting review", &report.awaiting_review),
        ("Decisions", &report.decisions),
        ("Stale docs", &report.stale_docs),
        ("Budget warnings", &report.budget_warnings),
        ("Pending approvals", &report.pending_approvals),
    ]
    .into_iter()
    .filter(|(_, items)| !items.is_empty())
    .map(|(title, items)| {
        let body = items
            .iter()
            .map(|i| format!("  - {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{title}\n{body}")
    })
    .collect();

    if sections.is_empty() {
        "nothing happened since you left — the project is quiet.".to_string()
    } else {
        sections.join("\n\n")
    }
}

/// `tm status`: compose a [`StatusReport`] from the event log and project view.
pub fn status(project: &Project, args: &StatusArgs, renderer: &Renderer) -> tm_types::Result<()> {
    let report = build_status_report(project, args)?;
    let human = render_status_report(&report);
    renderer.emit(&report, &human)
}

/// One check `tm doctor` performs.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    /// The check's name, e.g. `"invariants"`, `"hash-chain"`, `"index-health"`.
    pub name: String,
    /// Whether the check passed.
    pub ok: bool,
    /// Human-readable detail, especially on failure.
    pub detail: String,
}

/// The full `tm doctor` report.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// Every check that ran, in run order.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// Whether every check passed.
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

#[cfg(target_os = "macos")]
async fn probe_computer_backend(
    kind: tm_computer::BackendKind,
) -> tm_types::Result<tm_computer::Capabilities> {
    use tm_computer::Backend;
    match kind {
        tm_computer::BackendKind::Macos => tm_computer::macos::MacosBackend::new().probe().await,
        other => Err(TmError::storage(format!(
            "selected backend {other:?} is not available on this platform"
        ))),
    }
}

#[cfg(target_os = "linux")]
async fn probe_computer_backend(
    kind: tm_computer::BackendKind,
) -> tm_types::Result<tm_computer::Capabilities> {
    use tm_computer::Backend;
    match kind {
        tm_computer::BackendKind::X11 => {
            tm_computer::linux::X11Backend::connect(None)?.probe().await
        }
        tm_computer::BackendKind::Wayland => {
            tm_computer::linux::WaylandBackend::connect()?.probe().await
        }
        other => Err(TmError::storage(format!(
            "selected backend {other:?} is not available on this platform"
        ))),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn probe_computer_backend(
    kind: tm_computer::BackendKind,
) -> tm_types::Result<tm_computer::Capabilities> {
    Err(TmError::storage(format!(
        "no computer-use backend is available on this platform (selected {kind:?})"
    )))
}

fn render_doctor_report(report: &DoctorReport) -> String {
    let rows = report
        .checks
        .iter()
        .map(|c| {
            vec![
                c.name.clone(),
                if c.ok {
                    "ok".to_string()
                } else {
                    "FAIL".to_string()
                },
                c.detail.clone(),
            ]
        })
        .collect();
    Table::new(
        vec![
            "check".to_string(),
            "status".to_string(),
            "detail".to_string(),
        ],
        rows,
    )
    .render()
}

/// Build `tm doctor`'s `"providers"` check: whether any model provider is actually usable right
/// now, prioritizing the genuinely free/zero-signup options ahead of anything that needs a new
/// signup — matching this crate's onboarding goal of not leaving a fresh install stuck with no
/// guidance toward a free path. Ordering, deliberately: the three local backends
/// ([`tm_provider::LOCAL_PROVIDER_IDS`] — Ollama, LM Studio, llama.cpp: no account at all, and the
/// only backends this function actually probes for reachability, via
/// [`tm_provider::Registry::probe_local`]), then GitHub Models (a token most developers already
/// have from `gh auth login`, checked by env-var presence only — this deliberately does not shell
/// out to `gh auth status`, which validates against the network with no timeout this crate
/// controls), then every other known backend, reported as configured/not by env-var presence
/// alone (this crate has no free way to probe a paid third-party API's reachability).
///
/// This check is always `ok: true` — it is advisory ("here's what's available, here's the
/// fastest free path if nothing is"), not a correctness invariant like `"invariants"` or
/// `"hash-chain"`; a freshly-`tm init`'d project genuinely has no provider configured yet, and
/// that is expected, not a doctor failure.
async fn provider_doctor_detail(clock: Arc<dyn Clock>) -> DoctorCheck {
    let known = tm_provider::Registry::known_providers();

    let mut local_lines = Vec::with_capacity(tm_provider::LOCAL_PROVIDER_IDS.len());
    let mut any_local_ready = false;
    for id in tm_provider::LOCAL_PROVIDER_IDS {
        let probe = tm_provider::Registry::probe_local(id, clock.clone()).await;
        local_lines.push(match &probe {
            tm_provider::LocalProbe::Unreachable => format!("{id}: not running"),
            tm_provider::LocalProbe::ReachableNoModels => {
                format!("{id}: running, no models pulled yet")
            }
            tm_provider::LocalProbe::Ready { first_model } => {
                any_local_ready = true;
                format!("{id}: ready (model \"{first_model}\")")
            }
        });
    }

    let github_configured = known
        .iter()
        .find(|info| info.id == "github-models")
        .is_some_and(tm_provider::ProviderInfo::is_configured);
    let github_line = if github_configured {
        "github-models: ready (GITHUB_TOKEN set)".to_string()
    } else {
        "github-models: GITHUB_TOKEN not set (if you use the gh CLI: export \
         GITHUB_TOKEN=$(gh auth token))"
            .to_string()
    };

    let mut other_lines = Vec::new();
    let mut any_other_ready = false;
    for info in &known {
        if tm_provider::LOCAL_PROVIDER_IDS.contains(&info.id) || info.id == "github-models" {
            continue;
        }
        if info.is_configured() {
            any_other_ready = true;
            other_lines.push(format!("{}: ready", info.id));
        }
    }

    let any_ready = any_local_ready || github_configured || any_other_ready;

    let mut detail = local_lines.join("; ");
    detail.push_str("; ");
    detail.push_str(&github_line);
    if !other_lines.is_empty() {
        detail.push_str("; ");
        detail.push_str(&other_lines.join("; "));
    }
    if !any_ready {
        detail.push_str(
            "; no model provider is ready. Fastest zero-cost path: install Ollama \
             (https://ollama.com) and run `ollama pull <model>`, or, if you have a GitHub \
             account, run `gh auth login` and export GITHUB_TOKEN=$(gh auth token) to use \
             GitHub Models.",
        );
    }

    DoctorCheck {
        name: "providers".to_string(),
        ok: true,
        detail,
    }
}

/// `tm doctor`: invariants, the hash-chain check, index health, provider availability, and
/// (unless `--skip-computer-probe`) the computer-use permission probes.
pub fn doctor(
    project: &Project,
    args: &DoctorArgs,
    renderer: &Renderer,
) -> tm_types::Result<DoctorReport> {
    let mut checks = Vec::new();

    let violations = project.store.check_invariants()?;
    checks.push(DoctorCheck {
        name: "invariants".to_string(),
        ok: violations.is_empty(),
        detail: if violations.is_empty() {
            "no invariant violations".to_string()
        } else {
            violations
                .iter()
                .map(|v| format!("{} ({}): {}", v.invariant, v.subject, v.detail))
                .collect::<Vec<_>>()
                .join("; ")
        },
    });

    let log = open_event_log(project)?;
    let chain = log.verify_chain()?;
    checks.push(DoctorCheck {
        name: "hash-chain".to_string(),
        ok: chain.is_valid(),
        detail: if chain.is_valid() {
            format!("{} events verified", chain.events_checked)
        } else {
            chain
                .detail
                .clone()
                .unwrap_or_else(|| "chain broken".to_string())
        },
    });

    let index_check = match project
        .code_intel()
        .and_then(|ci| ci.update_incremental(project.clock.as_ref()))
    {
        Ok(delta) => DoctorCheck {
            name: "index-health".to_string(),
            ok: true,
            detail: format!(
                "repaired incremental drift: {} added, {} modified, {} removed, {} chunks written, \
                 {} commits ingested",
                delta.files_added, delta.files_modified, delta.files_removed, delta.chunks_written,
                delta.commits_ingested
            ),
        },
        Err(e) => DoctorCheck {
            name: "index-health".to_string(),
            ok: false,
            detail: e.to_string(),
        },
    };
    checks.push(index_check);

    let provider_check = run_async(|| async {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Ok(provider_doctor_detail(clock).await)
    })?;
    checks.push(provider_check);

    if !args.skip_computer_probe {
        let probed = run_async(|| async {
            let env = tm_computer::SelectionEnv::from_process();
            let kind = tm_computer::select_backend(&env)?;
            probe_computer_backend(kind).await
        });
        checks.push(match probed {
            Ok(caps) => DoctorCheck {
                name: "computer-use".to_string(),
                ok: caps.input && caps.capture,
                detail: if caps.notes.is_empty() {
                    format!(
                        "{:?} backend ready (input={}, capture={}, element_tree={}, headless={})",
                        caps.backend, caps.input, caps.capture, caps.element_tree, caps.headless
                    )
                } else {
                    caps.notes.join("; ")
                },
            },
            Err(e) => DoctorCheck {
                name: "computer-use".to_string(),
                ok: false,
                detail: e.to_string(),
            },
        });
    }

    let report = DoctorReport { checks };
    let human = render_doctor_report(&report);
    renderer.emit(&report, &human)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TM_ACTOR`/`USER` are process-global; serialize every test that touches them.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_renderer() -> Renderer {
        Renderer::new(true, true, true, false)
    }

    #[test]
    fn locate_finds_a_tm_directory_from_a_nested_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".tm")).unwrap();
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(locate(&nested).unwrap(), root);
    }

    #[test]
    fn locate_errors_when_no_tm_directory_exists_up_to_the_filesystem_root() {
        let tmp = tempfile::tempdir().unwrap();
        let err = locate(tmp.path()).unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn resolve_project_dir_prefers_the_explicit_path_over_locate() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("wherever-even-without-a-tm-dir");
        assert_eq!(resolve_project_dir(Some(&explicit)).unwrap(), explicit);
    }

    #[test]
    fn open_resolves_actor_from_tm_actor_over_user() {
        let _guard = ENV_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".tm")).unwrap();
        std::env::set_var("TM_ACTOR", "alice");
        std::env::set_var("USER", "bob");
        let project = open(tmp.path()).unwrap();
        std::env::remove_var("TM_ACTOR");
        std::env::remove_var("USER");
        assert_eq!(project.actor.as_str(), "human:alice");
    }

    #[test]
    fn open_falls_back_to_unknown_when_no_identity_env_var_is_set() {
        let _guard = ENV_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".tm")).unwrap();
        std::env::remove_var("TM_ACTOR");
        std::env::remove_var("USER");
        let project = open(tmp.path()).unwrap();
        assert_eq!(project.actor.as_str(), "human:unknown");
    }

    #[test]
    fn init_creates_a_tm_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let args = InitArgs {
            path: Some(tmp.path().to_path_buf()),
        };
        init(&args, &test_renderer()).unwrap();
        assert!(tmp.path().join(".tm").is_dir());
    }

    #[test]
    fn init_refuses_to_reinitialize_an_existing_project() {
        let tmp = tempfile::tempdir().unwrap();
        let args = InitArgs {
            path: Some(tmp.path().to_path_buf()),
        };
        init(&args, &test_renderer()).unwrap();
        let err = init(&args, &test_renderer()).unwrap_err();
        assert!(matches!(err, TmError::Conflict(_)));
    }

    #[test]
    fn attach_initializes_a_fresh_directory_and_indexes_it() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("README.md"), "# hi\n").unwrap();
        let args = AttachArgs {
            path: Some(tmp.path().to_path_buf()),
        };
        attach(&args, &test_renderer()).unwrap();
        assert!(tmp.path().join(".tm").is_dir());
    }

    #[test]
    fn require_anthropic_api_key_errors_precisely_when_unset() {
        let err = api_key_gate(Err(std::env::VarError::NotPresent)).unwrap_err();
        assert!(matches!(err, TmError::Provider(_)));
    }

    #[test]
    fn require_anthropic_api_key_passes_when_set() {
        let result = api_key_gate(Ok("test-key".to_string()));
        assert!(result.is_ok());
    }

    #[test]
    fn resolve_genesis_prompt_uses_the_explicit_text_verbatim() {
        let prompt =
            resolve_genesis_prompt(Some("build a todo app"), false, std::io::empty()).unwrap();
        assert_eq!(prompt, "build a todo app");
    }

    #[test]
    fn resolve_genesis_prompt_reads_stdin_when_explicit_is_a_dash() {
        let prompt = resolve_genesis_prompt(Some("-"), false, "from stdin".as_bytes()).unwrap();
        assert_eq!(prompt, "from stdin");
    }

    #[test]
    fn resolve_genesis_prompt_reads_stdin_when_absent_and_piped() {
        let prompt = resolve_genesis_prompt(None, false, "piped prompt".as_bytes()).unwrap();
        assert_eq!(prompt, "piped prompt");
    }

    #[test]
    fn resolve_genesis_prompt_refuses_a_bare_terminal_with_no_prompt() {
        let err = resolve_genesis_prompt(None, true, std::io::empty()).unwrap_err();
        assert!(matches!(err, TmError::Parse(_)));
    }

    /// `git init` a directory so `CodeIntel`'s history ingest (which `doctor`'s index-health
    /// check and `attach` both depend on) has a repository to open instead of failing cleanly
    /// per `tm-genesis`'s documented "no git repo -> clean error" contract.
    fn init_git_repo(root: &Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("git should run in test environment");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "--quiet", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--quiet", "--allow-empty", "-m", "init"]);
    }

    fn open_test_project(root: &Path) -> Project {
        std::fs::create_dir_all(root.join(".tm")).unwrap();
        init_git_repo(root);
        let clock: Arc<dyn Clock> = Arc::new(tm_types::FixedClock::new(
            Timestamp::from_unix_seconds(1_000_000),
        ));
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(1));
        let store = Arc::new(tm_core::Store::open_with(root, clock.clone(), ids.clone()).unwrap());
        Project {
            root: root.to_path_buf(),
            store,
            clock,
            ids,
            actor: ParticipantId::new("human:tester").unwrap(),
        }
    }

    #[test]
    fn build_status_report_buckets_a_closed_ticket_and_a_decision_since_the_window_start() {
        let tmp = tempfile::tempdir().unwrap();
        let project = open_test_project(tmp.path());

        project
            .store
            .record_decision(
                "storage engine".to_string(),
                "use sqlite".to_string(),
                "already a dependency".to_string(),
                vec![],
                vec![],
                vec![],
                project.actor.clone(),
            )
            .unwrap();

        let log = open_event_log(&project).unwrap();
        let ticket = TicketId::new("T-000000000001").unwrap();
        log.append_all(vec![tm_events::EventDraft::new(
            project.actor.clone(),
            tm_types::Id::from(ticket.clone()),
            tm_events::Payload::from(tm_events::payload::TicketClosedPayload {
                ticket: ticket.clone(),
                reason: None,
            }),
        )])
        .unwrap();

        let report = build_status_report(
            &project,
            &StatusArgs {
                since_hours: Some(24),
            },
        )
        .unwrap();
        assert_eq!(report.closed, vec![ticket.to_string()]);
        assert_eq!(report.decisions.len(), 1);
        assert!(report.decisions[0].contains("use sqlite"));
    }

    #[test]
    fn build_status_report_tracks_an_unresolved_approval_as_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let project = open_test_project(tmp.path());
        let ticket = TicketId::new("T-000000000002").unwrap();

        let log = open_event_log(&project).unwrap();
        log.append_all(vec![tm_events::EventDraft::new(
            project.actor.clone(),
            tm_types::Id::from(ticket.clone()),
            tm_events::Payload::from(tm_events::payload::ApprovalRequestedPayload {
                ticket: Some(ticket.clone()),
                requested_of: project.actor.clone(),
                note: "please review the migration".to_string(),
            }),
        )])
        .unwrap();

        let report = build_status_report(&project, &StatusArgs { since_hours: None }).unwrap();
        assert_eq!(report.pending_approvals.len(), 1);
        assert!(report.pending_approvals[0].contains("please review the migration"));
    }

    #[test]
    fn build_status_report_clears_an_approval_once_decided() {
        let tmp = tempfile::tempdir().unwrap();
        let project = open_test_project(tmp.path());
        let ticket = TicketId::new("T-000000000003").unwrap();

        let log = open_event_log(&project).unwrap();
        log.append_all(vec![
            tm_events::EventDraft::new(
                project.actor.clone(),
                tm_types::Id::from(ticket.clone()),
                tm_events::Payload::from(tm_events::payload::ApprovalRequestedPayload {
                    ticket: Some(ticket.clone()),
                    requested_of: project.actor.clone(),
                    note: "ok to deploy?".to_string(),
                }),
            ),
            tm_events::EventDraft::new(
                project.actor.clone(),
                tm_types::Id::from(ticket.clone()),
                tm_events::Payload::from(tm_events::payload::ApprovalDecidedPayload {
                    ticket: Some(ticket.clone()),
                    decided_by: project.actor.clone(),
                    approved: true,
                    note: None,
                }),
            ),
        ])
        .unwrap();

        let report = build_status_report(&project, &StatusArgs { since_hours: None }).unwrap();
        assert!(report.pending_approvals.is_empty());
    }

    #[test]
    fn decision_summary_text_prefers_subject_and_decision_over_the_raw_json() {
        let raw = decision_summary_json_for_test("scope", "ship it");
        assert_eq!(decision_summary_text(&raw), "scope: ship it");
    }

    #[test]
    fn decision_summary_text_falls_back_to_raw_text_when_not_json() {
        assert_eq!(decision_summary_text("not json"), "not json");
    }

    fn decision_summary_json_for_test(subject: &str, decision: &str) -> String {
        serde_json::json!({ "subject": subject, "decision": decision, "reason": "" }).to_string()
    }

    #[test]
    fn budget_warning_is_none_below_the_warn_threshold() {
        let scoped = tm_core::budget::ScopedBudget {
            scope: tm_core::BudgetScope::Project,
            budget: tm_types::Budget {
                tokens: 1000,
                dollars_micros: u64::MAX,
                wall_seconds: u64::MAX,
                spent: tm_types::Spend::tokens(100),
            },
        };
        assert_eq!(budget_warning(&scoped), None);
    }

    #[test]
    fn budget_warning_fires_above_the_warn_threshold() {
        let scoped = tm_core::budget::ScopedBudget {
            scope: tm_core::BudgetScope::Project,
            budget: tm_types::Budget {
                tokens: 1000,
                dollars_micros: u64::MAX,
                wall_seconds: u64::MAX,
                spent: tm_types::Spend::tokens(900),
            },
        };
        let warning = budget_warning(&scoped).unwrap();
        assert!(warning.contains("project"));
        assert!(warning.contains("tokens"));
    }

    #[test]
    fn doctor_reports_healthy_invariants_and_chain_for_a_fresh_project_and_skips_the_computer_probe(
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let project = open_test_project(tmp.path());
        let args = DoctorArgs {
            skip_computer_probe: true,
        };

        let report = doctor(&project, &args, &test_renderer()).unwrap();

        assert!(report.checks.iter().any(|c| c.name == "invariants" && c.ok));
        assert!(report.checks.iter().any(|c| c.name == "hash-chain" && c.ok));
        assert!(report.checks.iter().any(|c| c.name == "index-health"));
        assert!(!report.checks.iter().any(|c| c.name == "computer-use"));
        assert!(report.all_ok());
    }

    #[test]
    fn run_async_propagates_the_inner_error() {
        let err =
            run_async(|| async { Err::<(), TmError>(TmError::invariant("boom")) }).unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }
}
