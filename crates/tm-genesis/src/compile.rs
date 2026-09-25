//! Graph compilation: turning a [`crate::spec::Specification`] into an actual `tm-core` graph.
//!
//! This module is split into three pure-then-I/O phases so each is independently testable:
//!
//! 1. [`propose_graph`] — ask `planner.frontier` for a [`GraphCompilation`]: tickets,
//!    dependencies, milestones, authority domains, verification policies, resource
//!    declarations, executor roles and budgets, addressed by local [`TicketRef`]s (not real
//!    [`tm_types::TicketId`]s, which don't exist until commit).
//! 2. [`validate_graph`] — pure: projects the proposal onto the existing [`tm_core::ProjectView`]
//!    and runs `tm_core::check_invariants` against the result, *before* anything is committed.
//! 3. [`commit_graph`] — I/O: turns every ref into a real ticket/milestone id and commits the
//!    whole graph as one logical operation, or rejects it wholesale. [`compile_with_retry`]
//!    composes all three: an invalid graph is regenerated with the validation errors fed back,
//!    bounded by [`RetryPolicy::max_attempts`], then escalated as a [`tm_core::Decision`]
//!    requiring human input rather than looping forever.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tm_core::graph::{DependencyEdge, DependencyGraph};
use tm_core::invariants::{check_invariants, Violation};
use tm_core::ticket::{
    ContextRef, DependencyKind, ExecutorRequirements, ResourceClaim,
    RetryPolicy as TicketRetryPolicy, Ticket, TicketKind, TicketState, VerificationPolicy,
};
use tm_core::{ProjectView, Store};
use tm_templates::TemplateManifest;
use tm_types::{
    ArtifactId, Authority, Budget, Clock, IdSource, MilestoneId, ParticipantId, Predicate,
    Result as TmResult, TicketId, Timestamp, TmError,
};

use crate::spec::Specification;

/// A local, unresolved reference to a ticket or milestone within one [`GraphCompilation`]
/// proposal, e.g. `"t-core-store"`. Resolved to a real id only by [`commit_graph`], since real
/// ids don't exist until the ticket/milestone is actually created.
pub type Ref = String;

/// One proposed ticket, addressed by [`Ref`] rather than [`TicketId`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedTicket {
    /// This ticket's local reference.
    pub ticket_ref: Ref,
    /// What kind of work this is.
    pub kind: TicketKind,
    /// The objective, in prose.
    pub objective: String,
    /// The parent ticket's ref, if this is a child.
    pub parent_ref: Option<Ref>,
    /// The milestone's ref this ticket belongs to, if any.
    pub milestone_ref: Option<Ref>,
    /// Authority granted to this ticket's executor.
    pub authority: Authority,
    /// Resource claims this ticket's lease will hold.
    pub resources: Vec<ResourceClaim>,
    /// Executor requirements.
    pub executor: ExecutorRequirements,
    /// Context references a worker should load first.
    pub context_refs: Vec<ContextRef>,
    /// Success predicates.
    pub success: Vec<Predicate>,
    /// How submissions are verified.
    pub verification: VerificationPolicy,
    /// Budget ceiling.
    pub budget: Budget,
    /// Retry policy on failure.
    pub retry: TicketRetryPolicy,
    /// Scheduling priority.
    pub priority: i32,
}

/// One proposed dependency edge between two [`Ref`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedDependency {
    /// The dependent ticket's ref.
    pub from_ref: Ref,
    /// The dependency's ref.
    pub to_ref: Ref,
    /// Hard, soft or loop.
    pub kind: DependencyKind,
}

/// Which release line ([`crate::spec::Specification::v0`] or
/// [`crate::spec::Specification::v1`]) a [`ProposedMilestone`] is being tagged as gating, so
/// [`crate::stages::Stage::Ignition`]/[`crate::stages::Stage::MaturityGate`] can select the
/// right milestone by explicit tag instead of positional order (the milestone that happens to
/// come first/second in the proposal). Serialized as lowercase `"v0"`/`"v1"` to match the wire
/// shape [`build_compile_request`] asks the model for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseMarker {
    /// This milestone is the one v0's exit criteria gate on.
    V0,
    /// This milestone is the one v1's exit criteria gate on.
    V1,
}

/// One proposed milestone, as a set of ticket [`Ref`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedMilestone {
    /// This milestone's local reference.
    pub milestone_ref: Ref,
    /// Milestone title.
    pub title: String,
    /// Member ticket refs.
    pub ticket_refs: Vec<Ref>,
    /// Which release line(s) this milestone gates — usually zero or one, but v0 and v1 may both
    /// gate on the same milestone (e.g. a project where v1 is just v0 hardened), so this is a
    /// set rather than a single optional marker. `#[serde(default)]` so proposals/fixtures
    /// written before this field existed still deserialize, as empty — meaning "no marker;
    /// callers fall back to the positional heuristic" rather than a hard error.
    #[serde(default)]
    pub releases: Vec<ReleaseMarker>,
}

/// A named authority domain applied across a set of tickets, so the proposal can express "these
/// N tickets share this authority" once instead of repeating it per [`ProposedTicket`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorityDomain {
    /// Domain name, e.g. `"core-crates"`.
    pub name: String,
    /// The authority this domain grants.
    pub authority: Authority,
    /// Ticket refs this domain applies to.
    pub applies_to: Vec<Ref>,
}

/// The full compiled-but-uncommitted graph proposal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphCompilation {
    /// The artifact id of the [`Specification`] this graph was compiled from, once persisted.
    pub source_spec: Option<ArtifactId>,
    /// Every proposed ticket.
    pub tickets: Vec<ProposedTicket>,
    /// Every proposed dependency edge.
    pub dependencies: Vec<ProposedDependency>,
    /// Every proposed milestone.
    pub milestones: Vec<ProposedMilestone>,
    /// Named authority domains, applied on top of each ticket's own `authority`.
    pub authority_domains: Vec<AuthorityDomain>,
    /// Which attempt this is, starting at `1`. Fed back into the prompt on retry along with the
    /// prior attempt's [`Violation`]s.
    pub attempt: u32,
    /// The id of the [`tm_templates::TemplateManifest`] [`select_template`] picked for this
    /// proposal's spec, if any (`SPEC.md` §27.1: "the graph compiler selects a template and
    /// parameterizes it rather than emitting tickets to invent one"). `None` whenever
    /// [`compile_with_retry`] is called with an empty template catalog — which is every existing
    /// caller today — so this field is purely additive and never changes graph-compilation
    /// behavior on its own; nothing in [`validate_graph`] or [`commit_graph`] reads it.
    #[serde(default)]
    pub selected_template: Option<String>,
}

/// Why compiling or committing a graph proposal failed.
#[derive(Debug, thiserror::Error)]
pub enum CompilationError {
    /// The proposal, projected onto the existing project state, violates one or more `tm-core`
    /// invariants.
    #[error("proposed ticket graph would break {} project rule(s); rejecting", .0.len())]
    InvalidGraph(Vec<Violation>),
    /// A [`Ref`] used by a dependency, milestone membership or `parent_ref` does not name any
    /// [`ProposedTicket::ticket_ref`]/[`ProposedMilestone::milestone_ref`] in the same proposal.
    #[error("proposed ticket graph refers to \"{0}\", which isn't in the same proposal")]
    DanglingRef(Ref),
    /// [`compile_with_retry`] exhausted its bounded attempts without producing a valid graph.
    #[error("ticket graph planning failed after {attempts} attempt(s); escalating to a human")]
    Exhausted {
        /// How many attempts were made.
        attempts: u32,
        /// The violations from the final attempt.
        last_violations: Vec<Violation>,
    },
}

/// Every [`Ref`] a [`ProposedTicket`], [`ProposedMilestone`], [`ProposedDependency`] or
/// [`AuthorityDomain`] names must resolve to a `ticket_ref`/`milestone_ref` declared in the same
/// `proposal`. Ref hygiene is `tm-genesis`'s concern (`tm-core` has no notion of a `Ref`), so it
/// is checked here rather than deferred to `validate_graph`.
fn check_dangling_refs(proposal: &GraphCompilation) -> Result<(), CompilationError> {
    let ticket_refs: BTreeSet<&Ref> = proposal.tickets.iter().map(|t| &t.ticket_ref).collect();
    let milestone_refs: BTreeSet<&Ref> = proposal
        .milestones
        .iter()
        .map(|m| &m.milestone_ref)
        .collect();

    for ticket in &proposal.tickets {
        if let Some(parent_ref) = &ticket.parent_ref {
            if !ticket_refs.contains(parent_ref) {
                return Err(CompilationError::DanglingRef(parent_ref.clone()));
            }
        }
        if let Some(milestone_ref) = &ticket.milestone_ref {
            if !milestone_refs.contains(milestone_ref) {
                return Err(CompilationError::DanglingRef(milestone_ref.clone()));
            }
        }
    }
    for dep in &proposal.dependencies {
        if !ticket_refs.contains(&dep.from_ref) {
            return Err(CompilationError::DanglingRef(dep.from_ref.clone()));
        }
        if !ticket_refs.contains(&dep.to_ref) {
            return Err(CompilationError::DanglingRef(dep.to_ref.clone()));
        }
    }
    for milestone in &proposal.milestones {
        for ticket_ref in &milestone.ticket_refs {
            if !ticket_refs.contains(ticket_ref) {
                return Err(CompilationError::DanglingRef(ticket_ref.clone()));
            }
        }
    }
    for domain in &proposal.authority_domains {
        for ticket_ref in &domain.applies_to {
            if !ticket_refs.contains(ticket_ref) {
                return Err(CompilationError::DanglingRef(ticket_ref.clone()));
            }
        }
    }
    Ok(())
}

/// `ticket`'s own declared authority, narrowed by every [`AuthorityDomain`] that names it —
/// a domain only ever attenuates, mirroring `tm-core`'s child-authority-contained invariant.
fn effective_authority(ticket: &ProposedTicket, domains: &[AuthorityDomain]) -> Authority {
    domains
        .iter()
        .filter(|domain| domain.applies_to.iter().any(|r| r == &ticket.ticket_ref))
        .fold(ticket.authority.clone(), |acc, domain| {
            acc.intersect(&domain.authority)
        })
}

/// The wire shape `propose_graph` asks the model for: a [`GraphCompilation`] minus the fields
/// only the caller can fill in (`source_spec`, `attempt`).
#[derive(Debug, Serialize, Deserialize)]
struct ProposedGraphPayload {
    tickets: Vec<ProposedTicket>,
    dependencies: Vec<ProposedDependency>,
    milestones: Vec<ProposedMilestone>,
    #[serde(default)]
    authority_domains: Vec<AuthorityDomain>,
}

fn build_compile_request(
    spec: &Specification,
    prior_violations: &[Violation],
) -> tm_provider::CompletionRequest {
    let mut prompt = String::new();
    prompt.push_str(
        "Compile this Specification into a GraphCompilation: tickets, dependencies, milestones \
         and authority_domains, addressed by local string refs rather than real ids.\n\n",
    );
    prompt.push_str(&format!("Requirements: {:#?}\n", spec.requirements));
    prompt.push_str(&format!("Architecture: {}\n", spec.architecture));
    prompt.push_str(&format!("Interfaces: {:#?}\n", spec.interfaces));
    prompt.push_str(&format!("Data model: {}\n", spec.data_model));
    prompt.push_str(&format!(
        "Technology choices: {:#?}\n",
        spec.technology_choices
    ));
    prompt.push_str(&format!("Quality bar: {}\n", spec.quality_bar));
    prompt.push_str(&format!("Security model: {}\n", spec.security_model));
    prompt.push_str(&format!("Testing strategy: {}\n", spec.testing_strategy));
    prompt.push_str(&format!("Milestone outline: {:#?}\n", spec.milestones));
    prompt.push_str(&format!("v0: {:#?}\n", spec.v0));
    prompt.push_str(&format!("v1: {:#?}\n", spec.v1));
    prompt.push_str(
        "\nSet exactly one milestone's \"releases\" field to include \"v0\" — the milestone \
         whose completion satisfies v0's objective and exit_criteria above — and exactly one \
         milestone's \"releases\" field to include \"v1\", the same way for v1. If the same \
         milestone gates both v0 and v1, its \"releases\" field should be [\"v0\", \"v1\"]. \
         Every other milestone's \"releases\" field should be omitted or empty.\n",
    );

    if !prior_violations.is_empty() {
        prompt.push_str("\nThe previous attempt violated these tm-core invariants; address them specifically:\n");
        for violation in prior_violations {
            prompt.push_str(&format!(
                "- {} on {}: {}\n",
                violation.invariant, violation.subject, violation.detail
            ));
        }
    }

    prompt.push_str(
        "\nRespond with a single JSON object of the shape {\"tickets\": [...], \"dependencies\": \
         [...], \"milestones\": [...], \"authority_domains\": [...]}, matching ProposedTicket, \
         ProposedDependency, ProposedMilestone and AuthorityDomain exactly.",
    );

    tm_provider::CompletionRequest {
        system: Some(
            "You are planner.frontier: decompose a Ticketmaster Specification into a \
             dependency-ordered ticket graph."
                .to_string(),
        ),
        messages: vec![tm_provider::Message {
            role: tm_provider::MessageRole::User,
            content: vec![tm_provider::ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        max_tokens: 8192,
        temperature: Some(0.0),
        stop_sequences: vec![],
        stream: false,
        n: 1,
        model: None,
    }
}

fn first_text_block(completion: &tm_provider::Completion) -> TmResult<String> {
    completion
        .candidates
        .first()
        .and_then(|candidate| {
            candidate.content.iter().find_map(|block| match block {
                tm_provider::ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .ok_or_else(|| TmError::parse("the provider's response didn't include any text"))
}

/// Ask `planner.frontier` to propose a [`GraphCompilation`] for `spec`. `attempt` and
/// `prior_violations` are empty/`1` on a first try; [`compile_with_retry`] fills them in on
/// retries so the model can see exactly what was wrong last time.
///
/// # Errors
/// `TmError::Provider` on completion failure, `TmError::Parse` on malformed output,
/// `TmError::Invariant` wrapping [`CompilationError::DanglingRef`] on a dangling ref.
pub async fn propose_graph(
    spec: &Specification,
    prior_violations: &[Violation],
    attempt: u32,
    provider: &dyn tm_provider::Provider,
) -> TmResult<GraphCompilation> {
    let request = build_compile_request(spec, prior_violations);
    let completion = provider
        .complete(request)
        .await
        .map_err(|e| TmError::Provider(e.to_string()))?;
    let text = first_text_block(&completion)?;
    let payload: ProposedGraphPayload = serde_json::from_str(&text).map_err(|e| {
        TmError::parse(format!(
            "couldn't understand the provider's ticket graph response: {e}"
        ))
    })?;

    let proposal = GraphCompilation {
        source_spec: None,
        tickets: payload.tickets,
        dependencies: payload.dependencies,
        milestones: payload.milestones,
        authority_domains: payload.authority_domains,
        attempt,
        // `propose_graph` never sees a template catalog — [`compile_with_retry`] fills this in,
        // via [`select_template`], after this call returns.
        selected_template: None,
    };
    check_dangling_refs(&proposal)?;
    Ok(proposal)
}

/// Select the best-matching template for `spec` by capability tag, if any — the additive path
/// `SPEC.md` §27.1 describes: "when the spec calls for a documentation site or a terminal UI,
/// the graph compiler selects a template and parameterizes it rather than emitting tickets to
/// invent one." Purely mechanical (never asked of a model): scans `spec`'s prose (architecture,
/// technology choices, interfaces, requirements) for each template's [`TemplateManifest::tags`]
/// as case-insensitive substrings, and returns the template with the most tag hits. Returns
/// `None` when `templates` is empty or no template's tags appear anywhere in `spec`'s prose —
/// this is what makes the template path optional rather than a replacement for graph compilation:
/// [`compile_with_retry`] with an empty (or non-matching) catalog behaves exactly as it did
/// before this function existed.
pub fn select_template<'a>(
    spec: &Specification,
    templates: &'a [TemplateManifest],
) -> Option<&'a TemplateManifest> {
    if templates.is_empty() {
        return None;
    }
    let haystack = spec_prose(spec).to_lowercase();
    templates
        .iter()
        .map(|t| (t, tag_hits(&haystack, t)))
        .filter(|(_, hits)| *hits > 0)
        .max_by_key(|(_, hits)| *hits)
        .map(|(t, _)| t)
}

/// Every bit of prose in `spec` a stack name might plausibly appear in, concatenated with spaces.
fn spec_prose(spec: &Specification) -> String {
    let mut text = String::new();
    text.push_str(&spec.architecture);
    text.push(' ');
    for choice in &spec.technology_choices {
        text.push_str(&choice.area);
        text.push(' ');
        text.push_str(&choice.choice);
        text.push(' ');
        text.push_str(&choice.rationale);
        text.push(' ');
    }
    for interface in &spec.interfaces {
        text.push_str(&interface.name);
        text.push(' ');
        text.push_str(&interface.description);
        text.push(' ');
    }
    for requirement in &spec.requirements {
        text.push_str(&requirement.text);
        text.push(' ');
    }
    text
}

/// How many of `template`'s tags appear (case-insensitively) in `haystack_lower`, which must
/// already be lowercased.
fn tag_hits(haystack_lower: &str, template: &TemplateManifest) -> usize {
    template
        .tags
        .iter()
        .filter(|tag| haystack_lower.contains(&tag.to_lowercase()))
        .count()
}

/// A placeholder [`TicketId`] for `index`, used only within [`validate_graph`]'s scratch
/// projection. Offset far above any realistic counter-allocated id so it can never collide with
/// a real ticket already in the view being validated against.
fn placeholder_ticket_id(index: usize) -> TicketId {
    TicketId::new(format!("T-9{index:09}"))
        .expect("placeholder ticket id is always a well-formed T-<digits> string")
}

/// Project `proposal` onto `existing` (as if every ticket/milestone/dependency/authority grant
/// it describes had already been committed, using placeholder [`TicketId`]/[`MilestoneId`]
/// values derived from each [`Ref`]) and run `tm_core::check_invariants` against the result.
pub fn validate_graph(proposal: &GraphCompilation, existing: &ProjectView) -> Vec<Violation> {
    let placeholders: BTreeMap<&Ref, TicketId> = proposal
        .tickets
        .iter()
        .enumerate()
        .map(|(i, t)| (&t.ticket_ref, placeholder_ticket_id(i)))
        .collect();

    let mut scratch = existing.clone();

    let mut nodes: Vec<TicketId> = existing.graph.nodes().cloned().collect();
    let mut edges: Vec<DependencyEdge> = existing.graph.edges().to_vec();
    let mut children: Vec<(TicketId, TicketId)> = existing
        .graph
        .nodes()
        .filter_map(|n| existing.graph.parent_of(n).map(|p| (p.clone(), n.clone())))
        .collect();

    for dep in &proposal.dependencies {
        if let (Some(from), Some(to)) = (
            placeholders.get(&dep.from_ref),
            placeholders.get(&dep.to_ref),
        ) {
            edges.push(DependencyEdge {
                from: from.clone(),
                to: to.clone(),
                kind: dep.kind,
            });
        }
    }

    for ticket in &proposal.tickets {
        let id = placeholders[&ticket.ticket_ref].clone();
        nodes.push(id.clone());
        if let Some(parent_id) = ticket.parent_ref.as_ref().and_then(|p| placeholders.get(p)) {
            children.push((parent_id.clone(), id));
        }
    }

    scratch.graph = DependencyGraph::build(nodes, edges, children);

    for ticket in &proposal.tickets {
        let id = placeholders[&ticket.ticket_ref].clone();
        let parent = ticket
            .parent_ref
            .as_ref()
            .and_then(|p| placeholders.get(p))
            .cloned();
        let dependencies: Vec<TicketId> = proposal
            .dependencies
            .iter()
            .filter(|dep| dep.from_ref == ticket.ticket_ref)
            .filter_map(|dep| placeholders.get(&dep.to_ref).cloned())
            .collect();
        let authority = effective_authority(ticket, &proposal.authority_domains);
        scratch.tickets.insert(
            id.clone(),
            Ticket {
                id,
                kind: ticket.kind,
                objective: ticket.objective.clone(),
                state: TicketState::Draft,
                parent,
                children: vec![],
                dependencies,
                milestone: None,
                due: None,
                authority,
                resources: ticket.resources.clone(),
                executor: ticket.executor.clone(),
                context_refs: ticket.context_refs.clone(),
                success: ticket.success.clone(),
                verification: ticket.verification.clone(),
                budget: ticket.budget,
                retry: ticket.retry,
                cycle: None,
                attempts: 0,
                failures: vec![],
                priority: ticket.priority,
                created: Timestamp::EPOCH,
                updated: Timestamp::EPOCH,
            },
        );
    }

    check_invariants(&scratch)
}

/// What committing a validated [`GraphCompilation`] produced.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitOutcome {
    /// Every event emitted while committing.
    pub events: Vec<tm_events::Event>,
    /// Real ticket ids, keyed by the [`Ref`] they were proposed under.
    pub tickets: BTreeMap<Ref, TicketId>,
    /// Real milestone ids, keyed by the [`Ref`] they were proposed under.
    pub milestones: BTreeMap<Ref, MilestoneId>,
    /// Real milestone ids the proposal explicitly tagged with a [`ReleaseMarker`], keyed by that
    /// marker. Absent whenever the proposal (or an older fixture predating this field) didn't
    /// tag any milestone — callers fall back to the positional heuristic in that case.
    pub release_milestones: BTreeMap<ReleaseMarker, MilestoneId>,
    /// The committed proposal's [`GraphCompilation::selected_template`], carried forward
    /// unchanged so a caller doesn't need to hold onto the proposal separately just to learn
    /// which template (if any) was picked.
    pub selected_template: Option<String>,
}

/// A parent-before-child creation order for every ticket in `proposal`, or
/// [`CompilationError::DanglingRef`] if a `parent_ref` never resolves (names an undeclared ref,
/// or only appears among tickets that themselves can never resolve — i.e. a parent cycle).
fn ticket_creation_order(proposal: &GraphCompilation) -> Result<Vec<Ref>, CompilationError> {
    let mut remaining: Vec<&ProposedTicket> = proposal.tickets.iter().collect();
    let mut resolved: BTreeSet<Ref> = BTreeSet::new();
    let mut order = Vec::new();

    while !remaining.is_empty() {
        let (ready, blocked): (Vec<&ProposedTicket>, Vec<&ProposedTicket>) =
            remaining.into_iter().partition(|t| {
                t.parent_ref
                    .as_ref()
                    .map(|p| resolved.contains(p))
                    .unwrap_or(true)
            });

        if ready.is_empty() {
            let unresolved = blocked[0]
                .parent_ref
                .clone()
                .unwrap_or_else(|| blocked[0].ticket_ref.clone());
            return Err(CompilationError::DanglingRef(unresolved));
        }
        for ticket in &ready {
            resolved.insert(ticket.ticket_ref.clone());
            order.push(ticket.ticket_ref.clone());
        }
        remaining = blocked;
    }

    Ok(order)
}

/// Best-effort compensation for a `commit_graph` failure: cancel every ticket already created in
/// this call (ignoring cancellation failures — this is already the failure path) and return
/// `err` unchanged, so the caller sees the original cause.
fn rollback_tickets(
    store: &Store,
    tickets: &BTreeMap<Ref, TicketId>,
    actor: &ParticipantId,
    err: TmError,
) -> TmError {
    for id in tickets.values() {
        let _ = store.cancel(
            id,
            Some("Rolling back: the proposed ticket graph was rejected as a whole".to_string()),
            actor.clone(),
        );
    }
    err
}

/// Commit an already-[`validate_graph`]-clean `proposal` to `store`.
///
/// `tm-core::Store`'s individual command methods (`create_ticket`, `create_milestone`,
/// `add_dependency`) each commit their own SQLite transaction, and this crate does not own a
/// batch-commit API on `Store` (`tm-core` is owned by another agent) — so "commit in one
/// transaction or reject wholesale" is honored at this level instead. Tickets are created
/// *before* milestones, not after: `Store::create_milestone` requires every member ticket to
/// already exist in the view, so a milestone can't be created first even though the module docs'
/// "milestones, then tickets" ordering would be more natural. Order is: tickets in
/// parent-before-child order (a ticket's `milestone_ref` is resolved once milestones exist, via
/// `create_milestone`'s own linking, not at ticket-creation time), then milestones, then
/// dependency edges. Any failure best-effort cancels every ticket already created in this call
/// (see [`rollback_tickets`]) and returns the original error, so a failed compile never leaves a
/// half-built graph looking legitimate.
///
/// # Errors
/// Whatever the underlying `Store` calls return; `TmError::Invariant` wrapping
/// [`CompilationError::DanglingRef`] if `proposal` contains a `Ref` unresolved after every
/// ticket/milestone in it has been processed (should not happen if `propose_graph`'s
/// dangling-ref check ran, but re-checked here as a last line of defense before committing).
pub fn commit_graph(
    store: &Store,
    proposal: &GraphCompilation,
    actor: ParticipantId,
) -> TmResult<CommitOutcome> {
    let order = ticket_creation_order(proposal)?;
    let by_ref: BTreeMap<&Ref, &ProposedTicket> = proposal
        .tickets
        .iter()
        .map(|t| (&t.ticket_ref, t))
        .collect();

    let mut tickets: BTreeMap<Ref, TicketId> = BTreeMap::new();
    let mut milestones: BTreeMap<Ref, MilestoneId> = BTreeMap::new();
    let mut release_milestones: BTreeMap<ReleaseMarker, MilestoneId> = BTreeMap::new();
    let mut events: Vec<tm_events::Event> = Vec::new();

    for ticket_ref in &order {
        let proposed = by_ref[ticket_ref];
        let parent = match &proposed.parent_ref {
            Some(p) => match tickets.get(p) {
                Some(id) => Some(id.clone()),
                None => {
                    return Err(rollback_tickets(
                        store,
                        &tickets,
                        &actor,
                        CompilationError::DanglingRef(p.clone()).into(),
                    ))
                }
            },
            None => None,
        };
        let authority = effective_authority(proposed, &proposal.authority_domains);
        let result = store
            .create_ticket(
                proposed.kind,
                proposed.objective.clone(),
                parent,
                None,
                authority,
                proposed.resources.clone(),
                proposed.executor.clone(),
                proposed.context_refs.clone(),
                proposed.success.clone(),
                proposed.verification.clone(),
                proposed.budget,
                proposed.retry,
                proposed.priority,
                actor.clone(),
            )
            .and_then(|evs| TicketId::new(evs[0].subject.as_str()).map(|id| (id, evs)));
        match result {
            Ok((id, evs)) => {
                tickets.insert(ticket_ref.clone(), id);
                events.extend(evs);
            }
            Err(err) => return Err(rollback_tickets(store, &tickets, &actor, err)),
        }
    }

    for milestone in &proposal.milestones {
        let mut member_refs: BTreeSet<&Ref> = milestone.ticket_refs.iter().collect();
        for ticket in &proposal.tickets {
            if ticket.milestone_ref.as_ref() == Some(&milestone.milestone_ref) {
                member_refs.insert(&ticket.ticket_ref);
            }
        }
        let mut member_ids = Vec::with_capacity(member_refs.len());
        for ticket_ref in member_refs {
            match tickets.get(ticket_ref) {
                Some(id) => member_ids.push(id.clone()),
                None => {
                    let err = CompilationError::DanglingRef(ticket_ref.clone()).into();
                    return Err(rollback_tickets(store, &tickets, &actor, err));
                }
            }
        }
        let result = store
            .create_milestone(milestone.title.clone(), member_ids, vec![], actor.clone())
            .and_then(|evs| MilestoneId::new(evs[0].subject.as_str()).map(|id| (id, evs)));
        match result {
            Ok((id, evs)) => {
                for marker in &milestone.releases {
                    release_milestones.insert(*marker, id.clone());
                }
                milestones.insert(milestone.milestone_ref.clone(), id);
                events.extend(evs);
            }
            Err(err) => return Err(rollback_tickets(store, &tickets, &actor, err)),
        }
    }

    for dep in &proposal.dependencies {
        let from = match tickets.get(&dep.from_ref) {
            Some(id) => id.clone(),
            None => {
                let err = CompilationError::DanglingRef(dep.from_ref.clone()).into();
                return Err(rollback_tickets(store, &tickets, &actor, err));
            }
        };
        let to = match tickets.get(&dep.to_ref) {
            Some(id) => id.clone(),
            None => {
                let err = CompilationError::DanglingRef(dep.to_ref.clone()).into();
                return Err(rollback_tickets(store, &tickets, &actor, err));
            }
        };
        match store.add_dependency(&from, &to, dep.kind, actor.clone()) {
            Ok(evs) => events.extend(evs),
            Err(err) => return Err(rollback_tickets(store, &tickets, &actor, err)),
        }
    }

    Ok(CommitOutcome {
        events,
        tickets,
        milestones,
        release_milestones,
        selected_template: proposal.selected_template.clone(),
    })
}

/// Bounds on [`compile_with_retry`]'s regenerate-on-violation loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Maximum number of `propose_graph` attempts before escalating.
    pub max_attempts: u32,
}

impl RetryPolicy {
    /// A conservative default: try once, retry twice more with feedback, then escalate.
    pub fn default_bounded() -> Self {
        RetryPolicy { max_attempts: 3 }
    }
}

/// Compose [`propose_graph`], [`validate_graph`] and [`commit_graph`]: propose, validate against
/// `store`'s current view, and either commit or retry with the violations fed back, bounded by
/// `policy.max_attempts`. On exhaustion, records a `tm_core::Decision` requiring human input
/// (via `Store::record_decision`) rather than looping forever, and returns
/// [`CompilationError::Exhausted`].
///
/// `templates` is [`select_template`]'s catalog: an empty slice (every caller as of this
/// writing) makes template selection a no-op and leaves this function's behavior identical to
/// before that parameter existed — see [`GraphCompilation::selected_template`]'s doc comment.
#[allow(clippy::too_many_arguments)]
pub async fn compile_with_retry(
    spec: &Specification,
    provider: &dyn tm_provider::Provider,
    store: &Store,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    actor: ParticipantId,
    templates: &[TemplateManifest],
    policy: &RetryPolicy,
) -> TmResult<CommitOutcome> {
    let mut prior_violations: Vec<Violation> = Vec::new();
    let mut last_violations: Vec<Violation> = Vec::new();

    for attempt in 1..=policy.max_attempts {
        let mut proposal = propose_graph(spec, &prior_violations, attempt, provider).await?;
        proposal.selected_template = select_template(spec, templates).map(|t| t.id.clone());
        let violations = validate_graph(&proposal, &store.view()?);
        if violations.is_empty() {
            return commit_graph(store, &proposal, actor.clone());
        }
        last_violations = violations.clone();
        prior_violations = violations;
    }

    // Not used for identity (Store allocates its own ids for the Decision itself), but folded
    // into the escalation's `subject` so repeated escalations across retries of the surrounding
    // genesis stage stay distinguishable in `tm doctor`/audit output.
    let correlation = ids.random_hex(8);
    let summary = if last_violations.is_empty() {
        "no issues recorded on the final attempt".to_string()
    } else {
        last_violations
            .iter()
            .map(|v| format!("{} on {}: {}", v.invariant, v.subject, v.detail))
            .collect::<Vec<_>>()
            .join("; ")
    };
    store.record_decision(
        format!("Ticket graph planning failed after every attempt [{correlation}]"),
        format!("As of {}: {summary}", clock.now().to_rfc3339()),
        "Ran out of attempts; escalating to a human to review.".to_string(),
        vec![],
        vec![],
        vec![],
        actor,
    )?;

    Err(CompilationError::Exhausted {
        attempts: policy.max_attempts,
        last_violations,
    }
    .into())
}

impl From<CompilationError> for TmError {
    fn from(err: CompilationError) -> Self {
        TmError::invariant(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tm_provider::mock::ScriptedFailure;
    use tm_provider::{
        Candidate, Completion, ContentBlock, MockProvider, ModelId, ProviderError, StopReason,
        Usage,
    };
    use tm_types::{CounterIds, FixedClock, Role, Tolerance};

    use crate::spec::{MilestoneOutline, ReleaseDefinition, Requirement, TechnologyChoice};

    fn executor() -> ExecutorRequirements {
        ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Any,
        }
    }

    fn retry() -> TicketRetryPolicy {
        TicketRetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 60,
        }
    }

    fn ticket(ticket_ref: &str, parent_ref: Option<&str>) -> ProposedTicket {
        ProposedTicket {
            ticket_ref: ticket_ref.to_string(),
            kind: TicketKind::Work,
            objective: format!("work for {ticket_ref}"),
            parent_ref: parent_ref.map(str::to_string),
            milestone_ref: None,
            authority: Authority::none(),
            resources: vec![],
            executor: executor(),
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::None,
            budget: Budget::unlimited(),
            retry: retry(),
            priority: 0,
        }
    }

    fn proposal(tickets: Vec<ProposedTicket>) -> GraphCompilation {
        GraphCompilation {
            source_spec: None,
            tickets,
            dependencies: vec![],
            milestones: vec![],
            authority_domains: vec![],
            attempt: 1,
            selected_template: None,
        }
    }

    fn spec_fixture() -> Specification {
        Specification {
            source_vision: None,
            requirements: vec![Requirement {
                id: "R1".to_string(),
                text: "does the thing".to_string(),
                priority: 0,
            }],
            architecture: "one crate".to_string(),
            interfaces: vec![],
            data_model: "none".to_string(),
            technology_choices: vec![],
            quality_bar: "green CI".to_string(),
            security_model: "no secrets".to_string(),
            testing_strategy: "unit tests".to_string(),
            milestones: vec![MilestoneOutline {
                title: "v0".to_string(),
                objective: "ship it".to_string(),
                scope_hint: "the whole thing".to_string(),
            }],
            v0: ReleaseDefinition {
                objective: "works".to_string(),
                exit_criteria: vec![],
            },
            v1: ReleaseDefinition {
                objective: "works better".to_string(),
                exit_criteria: vec![],
            },
            created: Timestamp::EPOCH,
        }
    }

    fn open_store() -> (TempDir, Store) {
        let dir = TempDir::new().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Store::open_with(dir.path(), clock, ids).expect("open store");
        (dir, store)
    }

    fn completion_with(text: &str) -> Completion {
        Completion {
            model: ModelId::new("mock", "mock-1"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage::default(),
            latency: Duration::from_millis(0),
            received_at: Timestamp::EPOCH,
        }
    }

    fn payload_json(payload: &ProposedGraphPayload) -> String {
        serde_json::to_string(payload).expect("payload serializes")
    }

    // -- check_dangling_refs / effective_authority -------------------------------------------

    #[test]
    fn dangling_refs_are_rejected_in_a_parent_ref() {
        let p = proposal(vec![ticket("t1", Some("missing"))]);
        assert!(
            matches!(check_dangling_refs(&p), Err(CompilationError::DanglingRef(r)) if r == "missing")
        );
    }

    #[test]
    fn dangling_refs_are_rejected_in_a_dependency() {
        let mut p = proposal(vec![ticket("t1", None)]);
        p.dependencies.push(ProposedDependency {
            from_ref: "t1".to_string(),
            to_ref: "ghost".to_string(),
            kind: DependencyKind::Hard,
        });
        assert!(
            matches!(check_dangling_refs(&p), Err(CompilationError::DanglingRef(r)) if r == "ghost")
        );
    }

    #[test]
    fn a_fully_resolved_proposal_has_no_dangling_refs() {
        let p = proposal(vec![ticket("t1", None), ticket("t2", Some("t1"))]);
        assert!(check_dangling_refs(&p).is_ok());
    }

    #[test]
    fn authority_domain_narrows_but_never_widens_a_tickets_authority() {
        let mut t = ticket("t1", None);
        t.authority = Authority::root();
        let domain = AuthorityDomain {
            name: "read-only".to_string(),
            authority: Authority::read_only(),
            applies_to: vec!["t1".to_string()],
        };
        let effective = effective_authority(&t, std::slice::from_ref(&domain));
        assert!(!effective.git.commit);
        assert!(effective
            .repository
            .read
            .is_subset_of(&Authority::root().repository.read));
    }

    // -- ticket_creation_order -----------------------------------------------------------------

    #[test]
    fn ticket_creation_order_places_parents_before_children() {
        let p = proposal(vec![
            ticket("child", Some("parent")),
            ticket("parent", None),
        ]);
        let order = ticket_creation_order(&p).unwrap();
        assert_eq!(order, vec!["parent".to_string(), "child".to_string()]);
    }

    #[test]
    fn ticket_creation_order_rejects_a_parent_cycle() {
        let p = proposal(vec![ticket("a", Some("b")), ticket("b", Some("a"))]);
        assert!(matches!(
            ticket_creation_order(&p),
            Err(CompilationError::DanglingRef(_))
        ));
    }

    // -- validate_graph ---------------------------------------------------------------------

    #[test]
    fn validate_graph_accepts_an_acyclic_proposal_with_contained_authority() {
        let p = proposal(vec![ticket("t1", None), ticket("t2", Some("t1"))]);
        assert!(validate_graph(&p, &ProjectView::empty()).is_empty());
    }

    #[test]
    fn validate_graph_flags_a_child_authority_not_contained_by_its_parent() {
        let mut child = ticket("child", Some("parent"));
        child.authority = Authority::root();
        let parent = ticket("parent", None);
        let p = proposal(vec![parent, child]);
        let violations = validate_graph(&p, &ProjectView::empty());
        assert!(violations
            .iter()
            .any(|v| v.invariant == tm_core::invariants::names::CHILD_AUTHORITY_CONTAINED));
    }

    #[test]
    fn validate_graph_flags_an_illegal_hard_dependency_cycle() {
        let mut a = ticket("a", None);
        a.kind = TicketKind::Work;
        let mut b = ticket("b", None);
        b.kind = TicketKind::Work;
        let mut p = proposal(vec![a, b]);
        p.dependencies.push(ProposedDependency {
            from_ref: "a".to_string(),
            to_ref: "b".to_string(),
            kind: DependencyKind::Hard,
        });
        p.dependencies.push(ProposedDependency {
            from_ref: "b".to_string(),
            to_ref: "a".to_string(),
            kind: DependencyKind::Hard,
        });
        let violations = validate_graph(&p, &ProjectView::empty());
        assert!(violations
            .iter()
            .any(|v| v.invariant == tm_core::invariants::names::ACYCLIC_EXCEPT_LOOP));
    }

    #[test]
    fn validate_graph_never_mutates_the_existing_view() {
        let existing = ProjectView::empty();
        let before = existing.tickets.len();
        let p = proposal(vec![ticket("t1", None)]);
        let _ = validate_graph(&p, &existing);
        assert_eq!(existing.tickets.len(), before);
    }

    // -- commit_graph ------------------------------------------------------------------------

    #[test]
    fn commit_graph_creates_tickets_milestones_and_dependencies() {
        let (_dir, store) = open_store();
        let mut child = ticket("child", Some("parent"));
        child.milestone_ref = Some("m1".to_string());
        let mut parent = ticket("parent", None);
        parent.milestone_ref = Some("m1".to_string());
        let mut p = proposal(vec![parent, child]);
        p.milestones.push(ProposedMilestone {
            milestone_ref: "m1".to_string(),
            title: "milestone one".to_string(),
            ticket_refs: vec![],
            releases: vec![],
        });
        p.dependencies.push(ProposedDependency {
            from_ref: "child".to_string(),
            to_ref: "parent".to_string(),
            kind: DependencyKind::Hard,
        });

        let outcome = commit_graph(&store, &p, ParticipantId::system()).expect("commit succeeds");
        assert_eq!(outcome.tickets.len(), 2);
        assert_eq!(outcome.milestones.len(), 1);

        let view = store.view().unwrap();
        let parent_id = outcome.tickets["parent"].clone();
        let child_id = outcome.tickets["child"].clone();
        assert_eq!(view.tickets[&child_id].parent, Some(parent_id.clone()));
        assert_eq!(
            view.tickets[&child_id].milestone,
            Some(outcome.milestones["m1"].clone())
        );
        assert_eq!(
            view.tickets[&parent_id].milestone,
            Some(outcome.milestones["m1"].clone())
        );
        assert!(view
            .graph
            .edges()
            .iter()
            .any(|e| e.from == child_id && e.to == parent_id));
    }

    /// `genesis-explicit-v0-v1-milestone-marking`: `commit_graph` collects every milestone's
    /// [`ReleaseMarker`] tags into `CommitOutcome::release_milestones`, keyed by marker, and a
    /// milestone with no `releases` at all (the "no marker" fallback path) contributes nothing.
    #[test]
    fn commit_graph_collects_release_markers_by_milestone() {
        let (_dir, store) = open_store();
        let mut p = proposal(vec![]);
        p.milestones.push(ProposedMilestone {
            milestone_ref: "shared".to_string(),
            title: "does both".to_string(),
            ticket_refs: vec![],
            releases: vec![ReleaseMarker::V0, ReleaseMarker::V1],
        });
        p.milestones.push(ProposedMilestone {
            milestone_ref: "untagged".to_string(),
            title: "neither".to_string(),
            ticket_refs: vec![],
            releases: vec![],
        });

        let outcome = commit_graph(&store, &p, ParticipantId::system()).expect("commit succeeds");
        let shared_id = outcome.milestones["shared"].clone();
        assert_eq!(outcome.release_milestones.len(), 2);
        assert_eq!(outcome.release_milestones[&ReleaseMarker::V0], shared_id);
        assert_eq!(outcome.release_milestones[&ReleaseMarker::V1], shared_id);
    }

    /// `#[serde(default)]` on `ProposedMilestone::releases`: a milestone JSON object with no
    /// `"releases"` key at all (the shape every fixture/proposal predating this field has)
    /// deserializes as an empty `Vec`, not an error — the "older fixtures keep working"
    /// requirement.
    #[test]
    fn proposed_milestone_releases_defaults_to_empty_when_absent() {
        let json = serde_json::json!({
            "milestone_ref": "m1",
            "title": "legacy milestone",
            "ticket_refs": []
        });
        let milestone: ProposedMilestone =
            serde_json::from_value(json).expect("deserializes without a releases field");
        assert_eq!(milestone.releases, Vec::new());
    }

    /// A `"releases"` array round-trips through the lowercase wire shape
    /// [`build_compile_request`]'s prompt asks the model for.
    #[test]
    fn proposed_milestone_releases_round_trips_lowercase_markers() {
        let json = serde_json::json!({
            "milestone_ref": "m1",
            "title": "tagged milestone",
            "ticket_refs": [],
            "releases": ["v0", "v1"]
        });
        let milestone: ProposedMilestone =
            serde_json::from_value(json).expect("deserializes a tagged milestone");
        assert_eq!(
            milestone.releases,
            vec![ReleaseMarker::V0, ReleaseMarker::V1]
        );
    }

    #[test]
    fn commit_graph_rolls_back_created_tickets_on_a_dangling_dependency() {
        let (_dir, store) = open_store();
        let mut t1 = ticket("t1", None);
        t1.authority.tickets.cancel = true;
        let mut p = proposal(vec![t1]);
        p.dependencies.push(ProposedDependency {
            from_ref: "t1".to_string(),
            to_ref: "ghost".to_string(),
            kind: DependencyKind::Hard,
        });

        let err = commit_graph(&store, &p, ParticipantId::system()).unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));

        let view = store.view().unwrap();
        assert_eq!(view.tickets.len(), 1);
        let (_, t) = view.tickets.iter().next().unwrap();
        assert_eq!(t.state, TicketState::Cancelled);
    }

    // -- select_template ----------------------------------------------------------------------

    fn ratatui_template() -> TemplateManifest {
        TemplateManifest {
            id: "starter-ratatui".to_string(),
            version: "0.1.0".to_string(),
            license: None,
            tags: vec![
                "rust".to_string(),
                "tui".to_string(),
                "ratatui".to_string(),
                "cli".to_string(),
            ],
            params: vec![],
            checksum: String::new(),
        }
    }

    fn axum_template() -> TemplateManifest {
        TemplateManifest {
            id: "starter-axum".to_string(),
            version: "0.1.0".to_string(),
            license: None,
            tags: vec![
                "rust".to_string(),
                "web-api".to_string(),
                "axum".to_string(),
                "service".to_string(),
            ],
            params: vec![],
            checksum: String::new(),
        }
    }

    #[test]
    fn select_template_picks_the_template_named_by_the_spec() {
        let spec = Specification {
            architecture: "A single-binary Ratatui TUI for browsing local files.".to_string(),
            technology_choices: vec![TechnologyChoice {
                area: "ui".to_string(),
                choice: "Ratatui".to_string(),
                rationale: "immediate-mode terminal rendering".to_string(),
            }],
            ..spec_fixture()
        };
        let templates = vec![axum_template(), ratatui_template()];
        let selected = select_template(&spec, &templates).expect("a template matches");
        assert_eq!(selected.id, "starter-ratatui");
    }

    #[test]
    fn select_template_is_case_insensitive_and_prefers_more_tag_hits() {
        let spec = Specification {
            architecture: "An AXUM web-api service exposing a REST interface over Rust."
                .to_string(),
            ..spec_fixture()
        };
        let templates = vec![ratatui_template(), axum_template()];
        let selected = select_template(&spec, &templates).expect("a template matches");
        assert_eq!(selected.id, "starter-axum");
    }

    #[test]
    fn select_template_returns_none_when_no_tag_matches() {
        let spec = Specification {
            architecture: "A COBOL mainframe batch job.".to_string(),
            ..spec_fixture()
        };
        let templates = vec![ratatui_template(), axum_template()];
        assert!(select_template(&spec, &templates).is_none());
    }

    #[test]
    fn select_template_returns_none_for_an_empty_catalog() {
        let spec = Specification {
            architecture: "A Ratatui TUI.".to_string(),
            ..spec_fixture()
        };
        assert!(select_template(&spec, &[]).is_none());
    }

    // -- propose_graph -----------------------------------------------------------------------

    #[tokio::test]
    async fn propose_graph_parses_a_scripted_completion() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let provider = MockProvider::new("mock", ModelId::new("mock", "mock-1"), clock);
        let spec = spec_fixture();
        let request = build_compile_request(&spec, &[]);
        let payload = ProposedGraphPayload {
            tickets: vec![ticket("t1", None)],
            dependencies: vec![],
            milestones: vec![],
            authority_domains: vec![],
        };
        provider.script_response(&request, completion_with(&payload_json(&payload)));

        let graph = propose_graph(&spec, &[], 1, &provider)
            .await
            .expect("propose succeeds");
        assert_eq!(graph.tickets.len(), 1);
        assert_eq!(graph.attempt, 1);
        assert_eq!(graph.source_spec, None);
    }

    #[tokio::test]
    async fn propose_graph_rejects_a_dangling_ref_in_the_response() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let provider = MockProvider::new("mock", ModelId::new("mock", "mock-1"), clock);
        let spec = spec_fixture();
        let request = build_compile_request(&spec, &[]);
        let payload = ProposedGraphPayload {
            tickets: vec![ticket("t1", Some("ghost"))],
            dependencies: vec![],
            milestones: vec![],
            authority_domains: vec![],
        };
        provider.script_response(&request, completion_with(&payload_json(&payload)));

        let err = propose_graph(&spec, &[], 1, &provider).await.unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[tokio::test]
    async fn propose_graph_surfaces_malformed_json_as_a_parse_error() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let provider = MockProvider::new("mock", ModelId::new("mock", "mock-1"), clock);
        let spec = spec_fixture();
        let request = build_compile_request(&spec, &[]);
        provider.script_response(&request, completion_with("not json"));

        let err = propose_graph(&spec, &[], 1, &provider).await.unwrap_err();
        assert!(matches!(err, TmError::Parse(_)));
    }

    #[tokio::test]
    async fn propose_graph_surfaces_a_provider_failure() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let provider = MockProvider::new("mock", ModelId::new("mock", "mock-1"), clock);
        let spec = spec_fixture();
        let request = build_compile_request(&spec, &[]);
        provider.script_failure(
            &request,
            ScriptedFailure {
                times: None,
                error: ProviderError::InvalidRequest("boom".to_string()),
            },
        );

        let err = propose_graph(&spec, &[], 1, &provider).await.unwrap_err();
        assert!(matches!(err, TmError::Provider(_)));
    }

    // -- compile_with_retry -------------------------------------------------------------------

    #[tokio::test]
    async fn compile_with_retry_commits_on_a_valid_first_attempt() {
        let (_dir, store) = open_store();
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let provider = MockProvider::new("mock", ModelId::new("mock", "mock-1"), clock.clone());
        let spec = spec_fixture();

        let request = build_compile_request(&spec, &[]);
        let payload = ProposedGraphPayload {
            tickets: vec![ticket("t1", None)],
            dependencies: vec![],
            milestones: vec![],
            authority_domains: vec![],
        };
        provider.script_response(&request, completion_with(&payload_json(&payload)));

        let outcome = compile_with_retry(
            &spec,
            &provider,
            &store,
            clock.as_ref(),
            ids.as_ref(),
            ParticipantId::system(),
            &[],
            &RetryPolicy::default_bounded(),
        )
        .await
        .expect("compiles on the first attempt");
        assert_eq!(outcome.tickets.len(), 1);
    }

    #[tokio::test]
    async fn compile_with_retry_feeds_violations_back_and_commits_on_the_second_attempt() {
        let (_dir, store) = open_store();
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let provider = MockProvider::new("mock", ModelId::new("mock", "mock-1"), clock.clone());
        let spec = spec_fixture();

        let mut bad_child = ticket("child", Some("parent"));
        bad_child.authority = Authority::root();
        let first_request = build_compile_request(&spec, &[]);
        let bad_payload = ProposedGraphPayload {
            tickets: vec![ticket("parent", None), bad_child],
            dependencies: vec![],
            milestones: vec![],
            authority_domains: vec![],
        };
        provider.script_response(&first_request, completion_with(&payload_json(&bad_payload)));

        // Compute what `compile_with_retry` will feed back on attempt 2, to script that exact
        // follow-up request.
        let first_violations = validate_graph(
            &proposal(bad_payload.tickets.clone()),
            &store.view().unwrap(),
        );
        assert!(!first_violations.is_empty());
        let second_request = build_compile_request(&spec, &first_violations);
        let good_payload = ProposedGraphPayload {
            tickets: vec![ticket("t1", None)],
            dependencies: vec![],
            milestones: vec![],
            authority_domains: vec![],
        };
        provider.script_response(
            &second_request,
            completion_with(&payload_json(&good_payload)),
        );

        let outcome = compile_with_retry(
            &spec,
            &provider,
            &store,
            clock.as_ref(),
            ids.as_ref(),
            ParticipantId::system(),
            &[],
            &RetryPolicy::default_bounded(),
        )
        .await
        .expect("recovers on the second attempt");
        assert_eq!(outcome.tickets.len(), 1);
    }

    #[tokio::test]
    async fn compile_with_retry_escalates_via_a_decision_after_exhausting_attempts() {
        let (_dir, store) = open_store();
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let provider = MockProvider::new("mock", ModelId::new("mock", "mock-1"), clock.clone());
        let spec = spec_fixture();
        let policy = RetryPolicy { max_attempts: 2 };

        let mut prior: Vec<Violation> = Vec::new();
        for _ in 0..policy.max_attempts {
            let request = build_compile_request(&spec, &prior);
            let mut bad_child = ticket("child", Some("parent"));
            bad_child.authority = Authority::root();
            let bad_payload = ProposedGraphPayload {
                tickets: vec![ticket("parent", None), bad_child],
                dependencies: vec![],
                milestones: vec![],
                authority_domains: vec![],
            };
            provider.script_response(&request, completion_with(&payload_json(&bad_payload)));
            prior = validate_graph(&proposal(bad_payload.tickets), &store.view().unwrap());
        }

        let err = compile_with_retry(
            &spec,
            &provider,
            &store,
            clock.as_ref(),
            ids.as_ref(),
            ParticipantId::system(),
            &[],
            &policy,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));

        let view = store.view().unwrap();
        assert_eq!(view.decisions.len(), 1);
    }
}
