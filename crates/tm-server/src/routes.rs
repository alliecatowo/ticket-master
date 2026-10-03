//! The axum router and every handler in `SPEC.md` §14.
//!
//! This module is deliberately a thin translation layer: every handler parses a typed request
//! body, calls straight into [`tm_core::store::Store`] (or the server-local [`crate::presence`]/
//! [`crate::approvals`] state), and renders the result as JSON. No business logic lives here —
//! legality of a ticket transition, budget accounting, lease conflict detection, all of that is
//! `tm-core`'s job; this module's only responsibilities are wire shape and HTTP status mapping
//! (via [`crate::state::ServerError`]).
//!
//! A handful of domain types in `tm-core` (`Lease`, `Decision`, `Milestone`, `Artifact`,
//! `Evidence`) intentionally don't derive `Serialize` — they're pure logic types, not wire
//! types. This module renders them by hand (the `*_json` helpers below) rather than adding
//! `serde` derives to a crate this module doesn't own.
//!
//! `GET /docs` and `POST /docs` are stubs: `tm-docs` isn't wired into [`crate::state::AppState`]
//! (no field for it), so the list is always empty and writes are refused with a clear message
//! rather than silently discarded.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tm_core::artifact::{Artifact, ArtifactStorage, Evidence};
use tm_core::decision::Decision;
use tm_core::lease::Lease;
use tm_core::milestone::Milestone;
use tm_core::store::AuditOutcome;
use tm_core::view::ProjectView;
use tm_core::{
    ArtifactKind, ContextRef, EvidenceKind, ExecutorRequirements, FailureClass, ResourceClaim,
    RetryPolicy, Ticket, TicketKind, TicketState, Trigger, VerificationPolicy,
};
use tm_events::Event;
use tm_harness::HarnessEpoch;
use tm_provider::{CandidateKey, CandidateState};
use tm_types::{
    ArtifactId, Authority, Budget, DecisionId, IdKind, LeaseId, MilestoneId, ParticipantId,
    Predicate, SessionId, TicketId, TmError,
};

use crate::approvals::{ApprovalDecision, ApprovalRequest, ApprovalStatus};
use crate::presence::{path_leases, PathLeaseSummary, PresenceEntry};
use crate::sse::sse_handler;
use crate::state::{AppState, ServerError};
use crate::wiki::{get_wiki_page, list_wiki_pages};

/// Build the full axum router: every handler in `SPEC.md` §14, bound to `state`.
///
/// Two guards wrap every route: [`crate::auth::authenticate`] (bearer token on a non-loopback
/// bind) and [`crate::auth::guard_local_origin`] (foreign `Host`/`Origin` refused on a loopback
/// bind, against DNS rebinding and cross-site requests). Neither affects a plain loopback client
/// such as `curl`, the `tm` CLI or a test's `reqwest`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/events", get(sse_handler))
        .route("/state", get(get_state))
        .route("/schema", get(get_schema))
        .route("/tickets", get(list_tickets).post(create_ticket))
        .route("/tickets/{id}", get(get_ticket).patch(update_ticket))
        .route("/tickets/{id}/events", get(ticket_events))
        .route("/tickets/{id}/transition", post(transition_ticket))
        .route("/tickets/{id}/lease", post(acquire_lease))
        .route("/tickets/{id}/evidence", post(attach_evidence))
        .route("/leases/{id}/heartbeat", post(heartbeat_lease))
        .route("/leases/{id}/release", post(release_lease))
        .route("/graph", get(get_graph))
        .route("/decisions", get(list_decisions).post(create_decision))
        .route("/decisions/{id}/supersede", post(supersede_decision))
        .route("/milestones", get(list_milestones).post(create_milestone))
        .route("/milestones/{id}/close", post(close_milestone))
        .route("/milestones/{id}/reopen", post(reopen_milestone))
        .route("/artifacts", get(list_artifacts).post(create_artifact))
        .route("/artifacts/{id}", get(get_artifact))
        .route("/docs", get(list_docs).post(create_doc))
        .route("/approvals", get(list_approvals).post(create_approval))
        .route("/approvals/{id}", get(get_approval))
        .route("/approvals/{id}/decide", post(decide_approval))
        .route("/sessions", post(create_session))
        .route("/sessions/{id}", delete(delete_session))
        .route("/sessions/{id}/presence", post(update_presence))
        .route("/presence", get(get_presence))
        .route("/providers", get(get_providers))
        .route("/harness", get(get_harness))
        .route("/metrics", get(get_metrics))
        .route("/wiki", get(list_wiki_pages))
        .route("/wiki/{*path}", get(get_wiki_page))
        .route("/agent-tokens", post(mint_agent_token))
        .layer(axum::middleware::from_fn(crate::auth::bind_identity))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::authenticate,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::guard_local_origin,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------------------------
// Rendering helpers for tm-core types that don't derive Serialize.
// ---------------------------------------------------------------------------------------------

fn lease_json(l: &Lease) -> Value {
    json!({
        "id": l.id,
        "ticket": l.ticket,
        "holder": l.holder,
        "authority": l.authority,
        "resources": l.resources,
        "acquired": l.acquired,
        "heartbeat": l.heartbeat,
        "ttl_seconds": l.ttl_seconds,
        "epoch": l.epoch,
    })
}

fn decision_json(d: &Decision) -> Value {
    json!({
        "id": d.id,
        "subject": d.subject,
        "decision": d.decision,
        "reason": d.reason,
        "evidence": d.evidence,
        "affected_tickets": d.affected_tickets,
        "affected_paths": d.affected_paths,
        "author": d.author,
        "ts": d.ts,
        "supersedes": d.supersedes,
        "superseded_by": d.superseded_by,
    })
}

fn milestone_json(m: &Milestone) -> Value {
    json!({
        "id": m.id,
        "title": m.title,
        "tickets": m.tickets,
        "state": m.state,
        "closed_by": m.closed_by,
        "assumptions": m.assumptions,
    })
}

fn artifact_json(a: &Artifact) -> Value {
    let storage = match &a.storage {
        ArtifactStorage::Inline(bytes) => json!({"kind": "inline", "len": bytes.len()}),
        // The on-disk location would leak the user's home directory to any client; the artifact
        // id and hash are what identify it.
        ArtifactStorage::OnDisk(_) => json!({"kind": "disk"}),
    };
    json!({
        "id": a.id,
        "kind": a.kind,
        "media_type": a.media_type,
        "bytes_len": a.bytes_len,
        "hash": a.hash,
        "storage": storage,
        "meta": a.meta,
    })
}

fn evidence_json(e: &Evidence) -> Value {
    json!({
        "ticket": e.ticket,
        "kind": e.kind,
        "artifact": e.artifact,
        "produced_by": e.produced_by,
        "ts": e.ts,
        "summary": e.summary,
    })
}

fn presence_entry_json(e: &PresenceEntry) -> Value {
    json!({
        "participant": e.participant,
        "ticket": e.ticket,
        "file": e.file,
        "action": e.action,
        "last_seen": e.last_seen,
        "ttl_seconds": e.ttl_seconds,
    })
}

fn path_lease_json(p: &PathLeaseSummary) -> Value {
    json!({
        "lease": p.lease,
        "ticket": p.ticket,
        "holder": p.holder,
        "mode": p.mode,
        "paths": p.paths,
    })
}

fn approval_request_json(r: &ApprovalRequest) -> Value {
    json!({
        "id": r.id,
        "ticket": r.ticket,
        "requested_by": r.requested_by,
        "subject": r.subject,
        "detail": r.detail,
        "requested_at": r.requested_at,
    })
}

fn approval_decision_json(d: &ApprovalDecision) -> Value {
    match d {
        ApprovalDecision::Approve { note } => json!({"approve": {"note": note}}),
        ApprovalDecision::Deny { reason } => json!({"deny": {"reason": reason}}),
    }
}

fn approval_status_json(s: &ApprovalStatus) -> Value {
    match s {
        ApprovalStatus::Pending => json!({"status": "pending"}),
        ApprovalStatus::Decided {
            decision,
            decided_by,
            decided_at,
        } => json!({
            "status": "decided",
            "decision": approval_decision_json(decision),
            "decided_by": decided_by,
            "decided_at": decided_at,
        }),
    }
}

fn harness_epoch_json(e: &HarnessEpoch) -> Value {
    json!({
        "number": e.number,
        "config_hash": e.config_hash,
        "config": e.config,
        "promoted_at": e.promoted_at,
        "promoted_by": e.promoted_by,
        "benchmark": e.benchmark,
    })
}

fn graph_json(view: &ProjectView) -> Value {
    let nodes: Vec<&TicketId> = view.graph.nodes().collect();
    let edges: Vec<Value> = view
        .graph
        .edges()
        .iter()
        .map(|e| json!({"from": e.from, "to": e.to, "kind": e.kind}))
        .collect();
    json!({"nodes": nodes, "edges": edges})
}

/// Render a batch of freshly appended events as their wire form: `{seq, ts, kind, subject,
/// actor, session, causation, correlation, payload}`.
///
/// # Errors
/// Whatever [`tm_events::Payload::to_json`] returns for a payload that fails to serialize.
fn events_json(events: &[Event]) -> tm_types::Result<Vec<Value>> {
    events
        .iter()
        .map(|e| {
            Ok(json!({
                "seq": e.seq,
                "ts": e.ts,
                "kind": e.kind,
                "subject": e.subject,
                "actor": e.actor,
                "session": e.session,
                "causation": e.causation,
                "correlation": e.correlation,
                "payload": e.payload.to_json()?,
                "hash": e.hash,
            }))
        })
        .collect()
}

fn ticket_state_label(state: TicketState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{state:?}"))
}

fn default_presence_action() -> String {
    "active".to_string()
}

// ---------------------------------------------------------------------------------------------
// Extractors
//
// Axum's built-in `Path<T>`/`Json<T>` extractors render a rejection (a bad path segment, a
// malformed or type-mismatched request body) as plain text, not this crate's
// `{"error", "message"}` JSON shape — every other error path in this module already goes through
// [`ServerError`]'s `IntoResponse`, so a bad `GET /tickets/NOTFOUND` or a malformed `POST
// /tickets` body was the one place a client saw an un-parseable body instead (`SPEC.md` §14,
// `p1-http-error-json-format`). [`ApiPath`]/[`ApiJson`] wrap the real extractors and convert
// their rejection into [`ServerError::BadRequest`] so they render the same way as everything
// else.
// ---------------------------------------------------------------------------------------------

/// `Path<T>`, but a rejection (e.g. `/tickets/NOTFOUND` failing to parse as a `TicketId`) renders
/// as this crate's JSON error shape instead of axum's plain-text `PathRejection` body.
struct ApiPath<T>(T);

impl<T, S> axum::extract::FromRequestParts<S> for ApiPath<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ServerError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(value)) => Ok(ApiPath(value)),
            Err(rejection) => Err(ServerError::BadRequest(rejection.body_text())),
        }
    }
}

/// `Json<T>`, but a rejection (malformed JSON, a field of the wrong type, a missing field with no
/// default) renders as this crate's JSON error shape instead of axum's plain-text `JsonRejection`
/// body. This still only names the first field serde chokes on for most bodies — serde's own
/// error stops there — but that's still valid `{error, message}` JSON, which is the contract
/// every other handler already promises. [`CreateTicketRequest`]'s own `FromRequest` impl below
/// goes further and names every missing required field together, since that's the shape
/// `p1-http-error-json-format`'s acceptance check exercises.
struct ApiJson<T>(T);

impl<T, S> axum::extract::FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ServerError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(ServerError::BadRequest(rejection.body_text())),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateTicketRequest {
    kind: TicketKind,
    objective: String,
    #[serde(default)]
    parent: Option<TicketId>,
    #[serde(default)]
    milestone: Option<MilestoneId>,
    // Unset fields get what `tm ticket new` gives a ticket, so a client can create workable
    // work from just a kind and an objective.
    #[serde(default = "Authority::worker")]
    authority: Authority,
    #[serde(default)]
    resources: Vec<ResourceClaim>,
    #[serde(default)]
    executor: ExecutorRequirements,
    #[serde(default)]
    context_refs: Vec<ContextRef>,
    #[serde(default)]
    success: Vec<Predicate>,
    #[serde(default)]
    verification: VerificationPolicy,
    #[serde(default = "Budget::unlimited")]
    budget: Budget,
    #[serde(default)]
    retry: RetryPolicy,
    #[serde(default)]
    priority: i32,
    actor: ParticipantId,
}

/// The fields a `POST /tickets` body has no default for; every other field gets what `tm ticket
/// new` gives a ticket (see the field comment above).
const CREATE_TICKET_REQUIRED_FIELDS: &[&str] = &["kind", "objective", "actor"];

/// A dedicated extractor (rather than [`ApiJson`]) so a body missing more than one required field
/// names all of them in one message, not just the first serde would have choked on — the shape
/// `p1-http-error-json-format`'s acceptance check drives (`POST /tickets -d '{}'` must name
/// `objective`, `kind` and `actor` together).
impl<S> axum::extract::FromRequest<S> for CreateTicketRequest
where
    S: Send + Sync,
{
    type Rejection = ServerError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|rejection| ServerError::BadRequest(rejection.body_text()))?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
            ServerError::BadRequest(format!("This request body isn't valid JSON: {e}."))
        })?;
        if let Value::Object(obj) = &value {
            let missing: Vec<&str> = CREATE_TICKET_REQUIRED_FIELDS
                .iter()
                .filter(|field| !obj.contains_key(**field))
                .copied()
                .collect();
            if !missing.is_empty() {
                return Err(ServerError::BadRequest(format!(
                    "This ticket is missing required field(s): {}.",
                    missing.join(", ")
                )));
            }
        }
        serde_json::from_value(value).map_err(|e| {
            ServerError::BadRequest(format!("This ticket's request body is invalid: {e}."))
        })
    }
}

#[derive(Debug, Deserialize)]
struct UpdateTicketRequest {
    fields: Value,
    actor: ParticipantId,
}

/// The audit outcomes a client may submit; converted into [`AuditOutcome`], which is pure logic
/// and carries no serde derive of its own.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AuditOutcomeBody {
    Passed,
    RejectedMinor,
    RejectedStructural,
}

impl From<AuditOutcomeBody> for AuditOutcome {
    fn from(value: AuditOutcomeBody) -> Self {
        match value {
            AuditOutcomeBody::Passed => AuditOutcome::Passed,
            AuditOutcomeBody::RejectedMinor => AuditOutcome::RejectedMinor,
            AuditOutcomeBody::RejectedStructural => AuditOutcome::RejectedStructural,
        }
    }
}

/// `POST /tickets/:id/transition`'s body: externally tagged over every ticket-lifecycle command
/// `tm-core` exposes, since the spec's single "transition" route is this crate's one door onto
/// all of them, not just the generic [`Trigger`] pass-through.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransitionRequest {
    Activate {
        actor: ParticipantId,
    },
    Trigger {
        trigger: Trigger,
        actor: ParticipantId,
    },
    Submit {
        summary: String,
        #[serde(default)]
        evidence: Vec<ArtifactId>,
        actor: ParticipantId,
    },
    Verify {
        verifier: TicketId,
        passed: bool,
        #[serde(default)]
        reason: Option<String>,
        actor: ParticipantId,
    },
    Audit {
        auditor: TicketId,
        outcome: AuditOutcomeBody,
        #[serde(default)]
        reason: Option<String>,
        actor: ParticipantId,
    },
    Close {
        #[serde(default)]
        reason: Option<String>,
        actor: ParticipantId,
    },
    Cancel {
        #[serde(default)]
        reason: Option<String>,
        actor: ParticipantId,
    },
    Reopen {
        #[serde(default)]
        reason: Option<String>,
        actor: ParticipantId,
    },
    /// A human accepts submitted work, closing the ticket (`tm ticket accept`).
    Accept {
        #[serde(default)]
        note: Option<String>,
        actor: ParticipantId,
    },
    /// A human sends submitted work back with a reason (`tm ticket reject`).
    Reject {
        reason: String,
        actor: ParticipantId,
    },
    /// A human sends an escalated ticket back to work (`tm ticket retry`).
    Retry {
        #[serde(default)]
        guidance: Option<String>,
        actor: ParticipantId,
    },
    Fail {
        class: FailureClass,
        detail: String,
        actor: ParticipantId,
    },
}

#[derive(Debug, Deserialize)]
struct AcquireLeaseRequest {
    holder: ParticipantId,
    #[serde(default = "Authority::none")]
    authority: Authority,
    #[serde(default)]
    resources: Vec<ResourceClaim>,
    ttl_seconds: u32,
    actor: ParticipantId,
}

/// Shared body for any endpoint whose only input is who's acting: lease heartbeat/release,
/// milestone close/reopen.
#[derive(Debug, Deserialize)]
struct ActorOnlyRequest {
    actor: ParticipantId,
}

#[derive(Debug, Deserialize)]
struct AttachEvidenceRequest {
    kind: EvidenceKind,
    artifact: ArtifactId,
    summary: String,
    actor: ParticipantId,
}

#[derive(Debug, Deserialize)]
struct DecisionRequest {
    subject: String,
    decision: String,
    reason: String,
    #[serde(default)]
    evidence: Vec<ArtifactId>,
    #[serde(default)]
    affected_tickets: Vec<TicketId>,
    #[serde(default)]
    affected_paths: Vec<String>,
    actor: ParticipantId,
}

#[derive(Debug, Deserialize)]
struct CreateMilestoneRequest {
    title: String,
    #[serde(default)]
    tickets: Vec<TicketId>,
    #[serde(default)]
    assumptions: Vec<DecisionId>,
    actor: ParticipantId,
}

/// `bytes` travels as a JSON array of byte values rather than base64: this server has no
/// established wire-compactness contract yet, and a plain array keeps the client generator
/// (`GET /schema`'s consumer) from needing a bespoke binary codec on day one.
#[derive(Debug, Deserialize)]
struct CreateArtifactRequest {
    kind: ArtifactKind,
    media_type: String,
    #[serde(default)]
    bytes: Vec<u8>,
    #[serde(default)]
    meta: Value,
    #[serde(default)]
    ticket: Option<TicketId>,
    actor: ParticipantId,
}

#[derive(Debug, Deserialize)]
struct CreateApprovalRequest {
    #[serde(default)]
    ticket: Option<TicketId>,
    requested_by: ParticipantId,
    subject: String,
    #[serde(default)]
    detail: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ApprovalDecisionBody {
    Approve {
        #[serde(default)]
        note: Option<String>,
    },
    Deny {
        reason: String,
    },
}

impl From<ApprovalDecisionBody> for ApprovalDecision {
    fn from(value: ApprovalDecisionBody) -> Self {
        match value {
            ApprovalDecisionBody::Approve { note } => ApprovalDecision::Approve { note },
            ApprovalDecisionBody::Deny { reason } => ApprovalDecision::Deny { reason },
        }
    }
}

#[derive(Debug, Deserialize)]
struct DecideApprovalRequest {
    decision: ApprovalDecisionBody,
    decided_by: ParticipantId,
}

#[derive(Debug, Default, Deserialize)]
struct CreateSessionRequest {
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PresenceUpdateRequest {
    participant: ParticipantId,
    #[serde(default)]
    ticket: Option<TicketId>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default = "default_presence_action")]
    action: String,
    #[serde(default)]
    ttl_seconds: Option<u32>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /health`: liveness, plus whether this process is working ready tickets (`workers`, from
/// [`crate::state::ServerConfig::workers`]).
async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"status": "ok", "workers": state.config.workers}))
}

async fn get_state(State(state): State<AppState>) -> Result<Json<Value>, ServerError> {
    let view = state.store.view()?;
    let head = state.events.head()?;
    Ok(Json(json!({
        "head": head,
        "workers": state.config.workers,
        "tickets": view.tickets.values().collect::<Vec<_>>(),
        "leases": view.leases.values().map(lease_json).collect::<Vec<_>>(),
        "decisions": view.decisions.values().map(decision_json).collect::<Vec<_>>(),
        "milestones": view.milestones.values().map(milestone_json).collect::<Vec<_>>(),
        "artifacts": view.artifacts.values().map(artifact_json).collect::<Vec<_>>(),
        "evidence": view.evidence.iter().map(evidence_json).collect::<Vec<_>>(),
        "budgets": view
            .budgets
            .iter()
            .map(|b| json!({"scope": b.scope, "budget": b.budget}))
            .collect::<Vec<_>>(),
    })))
}

/// A hand-written, intentionally partial JSON Schema document covering the wire shapes a client
/// generator needs first: tickets and the transition/lease command bodies. Extend as more
/// clients come online; there is no `schemars`-style derive wired into this crate yet.
async fn get_schema() -> Json<Value> {
    Json(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Ticketmaster server API",
        "definitions": {
            "TicketId": {"type": "string", "pattern": "^(T|V|A)-[0-9]+$"},
            "ParticipantId": {"type": "string"},
            "TicketKind": {
                "type": "string",
                "enum": ["work", "verification", "audit", "investigation", "recovery", "harness"]
            },
            "TicketState": {
                "type": "string",
                "enum": [
                    "draft", "blocked", "ready", "leased", "running", "submitted", "verifying",
                    "auditing", "rework", "replan", "recovery", "escalated", "closed", "cancelled"
                ]
            },
            "CreateTicketRequest": {
                "type": "object",
                "required": ["kind", "objective", "executor", "verification", "retry", "actor"],
                "properties": {
                    "kind": {"$ref": "#/definitions/TicketKind"},
                    "objective": {"type": "string"},
                    "parent": {"$ref": "#/definitions/TicketId"},
                    "priority": {"type": "integer"},
                    "actor": {"$ref": "#/definitions/ParticipantId"}
                }
            },
            "TransitionRequest": {
                "type": "object",
                "description": "externally tagged: exactly one of these keys",
                "minProperties": 1,
                "maxProperties": 1,
                "properties": {
                    "activate": {"type": "object", "required": ["actor"]},
                    "trigger": {"type": "object", "required": ["trigger", "actor"]},
                    "submit": {"type": "object", "required": ["summary", "actor"]},
                    "verify": {"type": "object", "required": ["verifier", "passed", "actor"]},
                    "audit": {"type": "object", "required": ["auditor", "outcome", "actor"]},
                    "close": {"type": "object", "required": ["actor"]},
                    "cancel": {"type": "object", "required": ["actor"]},
                    "reopen": {"type": "object", "required": ["actor"]},
                    "accept": {
                        "type": "object",
                        "description": "a human accepts submitted work, closing the ticket",
                        "required": ["actor"],
                        "properties": {
                            "note": {"type": "string"},
                            "actor": {"$ref": "#/definitions/ParticipantId"}
                        }
                    },
                    "reject": {
                        "type": "object",
                        "description": "a human sends submitted work back; the reason must not be blank",
                        "required": ["reason", "actor"],
                        "properties": {
                            "reason": {"type": "string", "minLength": 1, "pattern": "\\S"},
                            "actor": {"$ref": "#/definitions/ParticipantId"}
                        }
                    },
                    "retry": {
                        "type": "object",
                        "description": "a human sends an escalated ticket back to work",
                        "required": ["actor"],
                        "properties": {
                            "guidance": {"type": "string"},
                            "actor": {"$ref": "#/definitions/ParticipantId"}
                        }
                    },
                    "fail": {"type": "object", "required": ["class", "detail", "actor"]}
                }
            },
            "TicketEventsPage": {
                "type": "object",
                "description": "GET /tickets/{id}/events?after=<seq>&limit=<n>: one page of the ticket's own events, oldest first; pass `next` back as `after` until it is null",
                "required": ["events", "next"],
                "properties": {
                    "events": {"type": "array"},
                    "next": {"type": ["integer", "null"]}
                }
            },
            "Health": {
                "type": "object",
                "required": ["status", "workers"],
                "properties": {
                    "status": {"const": "ok"},
                    "workers": {"type": "boolean", "description": "whether this server works ready tickets itself"}
                }
            }
        }
    }))
}

async fn list_tickets(State(state): State<AppState>) -> Result<Json<Vec<Ticket>>, ServerError> {
    let view = state.store.view()?;
    Ok(Json(view.tickets.into_values().collect()))
}

async fn create_ticket(
    State(state): State<AppState>,
    body: CreateTicketRequest,
) -> Result<(StatusCode, Json<Value>), ServerError> {
    let events = state.store.create_ticket(
        body.kind,
        body.objective,
        body.parent,
        body.milestone,
        body.authority,
        body.resources,
        body.executor,
        body.context_refs,
        body.success,
        body.verification,
        body.budget,
        body.retry,
        body.priority,
        body.actor,
    )?;
    let ticket_id = TicketId::new(events[0].subject.as_str())?;
    let view = state.store.view()?;
    let ticket = view
        .tickets
        .get(&ticket_id)
        .ok_or_else(|| TmError::not_found("ticket", &ticket_id))?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"ticket": ticket, "events": events_json(&events)?})),
    ))
}

async fn get_ticket(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<TicketId>,
) -> Result<Json<Ticket>, ServerError> {
    let view = state.store.view()?;
    let ticket = view
        .tickets
        .get(&id)
        .cloned()
        .ok_or_else(|| TmError::not_found("ticket", &id))?;
    Ok(Json(ticket))
}

async fn update_ticket(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<TicketId>,
    ApiJson(body): ApiJson<UpdateTicketRequest>,
) -> Result<Json<Ticket>, ServerError> {
    state.store.update_ticket(&id, body.fields, body.actor)?;
    let view = state.store.view()?;
    let ticket = view
        .tickets
        .get(&id)
        .cloned()
        .ok_or_else(|| TmError::not_found("ticket", &id))?;
    Ok(Json(ticket))
}

async fn transition_ticket(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<TicketId>,
    ApiJson(body): ApiJson<TransitionRequest>,
) -> Result<Json<Value>, ServerError> {
    let events = match body {
        TransitionRequest::Activate { actor } => state.store.activate(&id, actor)?,
        TransitionRequest::Trigger { trigger, actor } => {
            state.store.transition(&id, trigger, actor)?
        }
        TransitionRequest::Submit {
            summary,
            evidence,
            actor,
        } => state.store.submit(&id, summary, evidence, actor)?,
        TransitionRequest::Verify {
            verifier,
            passed,
            reason,
            actor,
        } => state.store.verify(&id, &verifier, passed, reason, actor)?,
        TransitionRequest::Audit {
            auditor,
            outcome,
            reason,
            actor,
        } => state
            .store
            .audit(&id, &auditor, outcome.into(), reason, actor)?,
        TransitionRequest::Close { reason, actor } => state.store.close(&id, reason, actor)?,
        TransitionRequest::Cancel { reason, actor } => state.store.cancel(&id, reason, actor)?,
        TransitionRequest::Reopen { reason, actor } => state.store.reopen(&id, reason, actor)?,
        TransitionRequest::Accept { note, actor } => state.store.accept(&id, note, actor)?,
        TransitionRequest::Reject { reason, actor } => {
            // `Store::reject` refuses a blank reason itself; checking the same rule here first
            // lets a malformed body answer 400 rather than 409.
            tm_core::store::rejection_reason(&id, &reason)
                .map_err(|e| ServerError::BadRequest(e.to_string()))?;
            state.store.reject(&id, reason, actor)?
        }
        TransitionRequest::Retry { guidance, actor } => state.store.retry(&id, guidance, actor)?,
        TransitionRequest::Fail {
            class,
            detail,
            actor,
        } => state.store.record_failure(&id, class, detail, actor)?,
    };
    Ok(Json(json!({"events": events_json(&events)?})))
}

/// `GET /tickets/{id}/events`'s query: an exclusive `after` cursor (the last `seq` the client
/// has; `from` is accepted as the same thing, matching `GET /events?from=`) and a page size
/// capped at the server's replay page size.
#[derive(Debug, Default, Deserialize)]
struct TicketEventsQuery {
    #[serde(default, alias = "from")]
    after: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /tickets/{id}/events?after=<seq>&limit=<n>`: one page of the ticket's own history (the
/// events whose subject is the ticket, as `tm events` and the per-ticket event backstop count
/// them), oldest first, so a client showing one ticket needn't replay the whole log. `next` is
/// the cursor for the following page, or `null` once this page reached the end.
async fn ticket_events(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<TicketId>,
    axum::extract::Query(query): axum::extract::Query<TicketEventsQuery>,
) -> Result<Json<Value>, ServerError> {
    if !state.store.view()?.tickets.contains_key(&id) {
        return Err(TmError::not_found("ticket", &id).into());
    }
    let max = state.config.sse_replay_page_size.max(1);
    let limit = query.limit.unwrap_or(max).clamp(1, max);
    let events = state.events.read_subject_after(
        &tm_types::Id::from(id),
        query.after.unwrap_or(0),
        limit,
    )?;
    let next = (events.len() == limit)
        .then(|| events.last().map(|e| e.seq))
        .flatten();
    Ok(Json(json!({"events": events_json(&events)?, "next": next})))
}

async fn acquire_lease(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<TicketId>,
    ApiJson(body): ApiJson<AcquireLeaseRequest>,
) -> Result<(StatusCode, Json<Value>), ServerError> {
    let events = state.store.acquire_lease(
        &id,
        body.holder,
        body.authority,
        body.resources,
        body.ttl_seconds,
        body.actor,
    )?;
    let view = state.store.view()?;
    let lease = view
        .leases
        .values()
        .find(|l| l.ticket == id)
        .ok_or_else(|| TmError::invariant("lease not found immediately after acquire"))?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"lease": lease_json(lease), "events": events_json(&events)?})),
    ))
}

async fn heartbeat_lease(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<LeaseId>,
    ApiJson(body): ApiJson<ActorOnlyRequest>,
) -> Result<Json<Value>, ServerError> {
    let events = state.store.heartbeat(&id, body.actor)?;
    Ok(Json(json!({"events": events_json(&events)?})))
}

async fn release_lease(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<LeaseId>,
    ApiJson(body): ApiJson<ActorOnlyRequest>,
) -> Result<Json<Value>, ServerError> {
    let events = state.store.release(&id, body.actor)?;
    Ok(Json(json!({"events": events_json(&events)?})))
}

async fn attach_evidence(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<TicketId>,
    ApiJson(body): ApiJson<AttachEvidenceRequest>,
) -> Result<Json<Value>, ServerError> {
    let events =
        state
            .store
            .attach_evidence(&id, body.kind, &body.artifact, body.summary, body.actor)?;
    Ok(Json(json!({"events": events_json(&events)?})))
}

async fn get_graph(State(state): State<AppState>) -> Result<Json<Value>, ServerError> {
    let view = state.store.view()?;
    Ok(Json(graph_json(&view)))
}

async fn list_decisions(State(state): State<AppState>) -> Result<Json<Value>, ServerError> {
    let view = state.store.view()?;
    let decisions: Vec<Value> = view.decisions.values().map(decision_json).collect();
    Ok(Json(json!(decisions)))
}

async fn create_decision(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<DecisionRequest>,
) -> Result<(StatusCode, Json<Value>), ServerError> {
    let events = state.store.record_decision(
        body.subject,
        body.decision,
        body.reason,
        body.evidence,
        body.affected_tickets,
        body.affected_paths,
        body.actor,
    )?;
    let decision_id = DecisionId::new(events[0].subject.as_str())?;
    let view = state.store.view()?;
    let decision = view
        .decisions
        .get(&decision_id)
        .ok_or_else(|| TmError::not_found("decision", &decision_id))?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"decision": decision_json(decision), "events": events_json(&events)?})),
    ))
}

async fn supersede_decision(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<DecisionId>,
    ApiJson(body): ApiJson<DecisionRequest>,
) -> Result<(StatusCode, Json<Value>), ServerError> {
    let events = state.store.supersede(
        &id,
        body.subject,
        body.decision,
        body.reason,
        body.evidence,
        body.affected_tickets,
        body.affected_paths,
        body.actor,
    )?;
    let decision_id = DecisionId::new(events[0].subject.as_str())?;
    let view = state.store.view()?;
    let decision = view
        .decisions
        .get(&decision_id)
        .ok_or_else(|| TmError::not_found("decision", &decision_id))?;
    // `tm-core`'s materializer records `superseded_by` on the *old* decision row but never
    // back-fills `supersedes` on the *new* one (`decision.created` carries no such field); this
    // handler already knows it from the path, so patch the wire shape rather than under-report
    // truth we have in hand.
    let mut decision_body = decision_json(decision);
    decision_body["supersedes"] = json!(id.as_str());
    Ok((
        StatusCode::CREATED,
        Json(json!({"decision": decision_body, "events": events_json(&events)?})),
    ))
}

async fn list_milestones(State(state): State<AppState>) -> Result<Json<Value>, ServerError> {
    let view = state.store.view()?;
    let milestones: Vec<Value> = view.milestones.values().map(milestone_json).collect();
    Ok(Json(json!(milestones)))
}

async fn create_milestone(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateMilestoneRequest>,
) -> Result<(StatusCode, Json<Value>), ServerError> {
    let events =
        state
            .store
            .create_milestone(body.title, body.tickets, body.assumptions, body.actor)?;
    let milestone_id = MilestoneId::new(events[0].subject.as_str())?;
    let view = state.store.view()?;
    let milestone = view
        .milestones
        .get(&milestone_id)
        .ok_or_else(|| TmError::not_found("milestone", &milestone_id))?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"milestone": milestone_json(milestone), "events": events_json(&events)?})),
    ))
}

async fn close_milestone(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<MilestoneId>,
    ApiJson(body): ApiJson<ActorOnlyRequest>,
) -> Result<Json<Value>, ServerError> {
    let events = state.store.close_milestone(&id, body.actor)?;
    Ok(Json(json!({"events": events_json(&events)?})))
}

async fn reopen_milestone(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<MilestoneId>,
    ApiJson(body): ApiJson<ActorOnlyRequest>,
) -> Result<Json<Value>, ServerError> {
    let events = state.store.reopen_milestone(&id, body.actor)?;
    Ok(Json(json!({"events": events_json(&events)?})))
}

async fn list_artifacts(State(state): State<AppState>) -> Result<Json<Value>, ServerError> {
    let view = state.store.view()?;
    let artifacts: Vec<Value> = view.artifacts.values().map(artifact_json).collect();
    Ok(Json(json!(artifacts)))
}

async fn get_artifact(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<ArtifactId>,
) -> Result<Json<Value>, ServerError> {
    let view = state.store.view()?;
    let artifact = view
        .artifacts
        .get(&id)
        .ok_or_else(|| TmError::not_found("artifact", &id))?;
    Ok(Json(artifact_json(artifact)))
}

async fn create_artifact(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateArtifactRequest>,
) -> Result<(StatusCode, Json<Value>), ServerError> {
    let events = state.store.store_artifact(
        body.kind,
        body.media_type,
        body.bytes,
        body.meta,
        body.ticket,
        body.actor,
    )?;
    let artifact_id = ArtifactId::new(events[0].subject.as_str())?;
    let view = state.store.view()?;
    let artifact = view
        .artifacts
        .get(&artifact_id)
        .ok_or_else(|| TmError::not_found("artifact", &artifact_id))?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"artifact": artifact_json(artifact), "events": events_json(&events)?})),
    ))
}

/// `tm-docs` isn't wired into [`AppState`]; always reports no docs rather than guessing at a
/// storage location.
async fn list_docs() -> Json<Value> {
    Json(json!({"docs": []}))
}

/// See [`list_docs`]: there is nowhere durable to put a doc yet.
async fn create_doc() -> ServerError {
    ServerError::BadRequest("Docs aren't available on this server yet.".to_string())
}

async fn list_approvals(State(state): State<AppState>) -> Json<Value> {
    let pending = state.approvals.pending();
    let rendered: Vec<Value> = pending
        .iter()
        .map(|p| approval_request_json(&p.request))
        .collect();
    Json(json!(rendered))
}

/// Opens an approval request and blocks until [`decide_approval`] (on a different request,
/// possibly a different connection) decides it — the requesting agent's connection *is* the
/// block, per `SPEC.md` §14.
///
/// # Errors
/// [`ServerError::ApprovalFailed`] if the server drops the waiter (e.g. shutdown) before a
/// decision lands.
async fn create_approval(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateApprovalRequest>,
) -> Result<Json<Value>, ServerError> {
    let id = format!("AP-{}", state.ids.random_hex(12));
    let request = ApprovalRequest {
        id: id.clone(),
        ticket: body.ticket,
        requested_by: body.requested_by,
        subject: body.subject,
        detail: body.detail,
        requested_at: state.clock.now(),
    };
    let waiter = state.approvals.open(request);
    let decision = waiter
        .wait()
        .await
        .map_err(|e| ServerError::ApprovalFailed(e.to_string()))?;
    Ok(Json(
        json!({"id": id, "decision": approval_decision_json(&decision)}),
    ))
}

async fn get_approval(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ServerError> {
    let status = state
        .approvals
        .get(&id)
        .ok_or_else(|| ServerError::BadRequest(format!("Approval request {id} not found.")))?;
    Ok(Json(approval_status_json(&status)))
}

async fn decide_approval(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<DecideApprovalRequest>,
) -> Result<Json<Value>, ServerError> {
    // `decided_by` is the authenticated principal (see `auth::bind_identity`), so this is a
    // check on the credential: an agent token can never approve or deny.
    if !body.decided_by.is_human() {
        return Err(ServerError::Domain(TmError::AuthorityDenied(
            "only a human may decide an approval".to_string(),
        )));
    }
    let decided_at = state.clock.now();
    let pending_request = state
        .approvals
        .pending()
        .into_iter()
        .find(|p| p.request.id == id)
        .map(|p| p.request);
    let (subject, verdict, reason) = match &body.decision {
        ApprovalDecisionBody::Approve { note } => (
            pending_request.as_ref().map(|r| r.subject.clone()),
            "approved".to_string(),
            note.clone().unwrap_or_default(),
        ),
        ApprovalDecisionBody::Deny { reason } => (
            pending_request.as_ref().map(|r| r.subject.clone()),
            "denied".to_string(),
            reason.clone(),
        ),
    };
    let decision: ApprovalDecision = body.decision.into();
    state
        .approvals
        .decide(&id, decision, body.decided_by.clone(), decided_at)
        .map_err(|e| ServerError::BadRequest(e.to_string()))?;
    if let (Some(subject), Some(request)) = (subject, pending_request) {
        state.store.record_decision(
            subject,
            verdict,
            reason,
            Vec::new(),
            request.ticket.into_iter().collect(),
            Vec::new(),
            body.decided_by,
        )?;
    }
    Ok(Json(json!({"id": id, "status": "decided"})))
}

/// `tm-server` doesn't hold session/transcript state itself (`SPEC.md` §14: sessions are views);
/// this just mints a fresh id for the caller to tag its own events/presence with.
#[derive(Debug, Deserialize)]
struct MintAgentTokenRequest {
    name: String,
}

/// `POST /agent-tokens {"name": "worker-1"}`: a human operator mints a bearer token that
/// authenticates as `agent:api/worker-1` (and can never act as a human). The token is returned once.
async fn mint_agent_token(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<crate::auth::Principal>,
    ApiJson(body): ApiJson<MintAgentTokenRequest>,
) -> Result<(StatusCode, Json<Value>), ServerError> {
    if !principal.is_human() {
        return Err(ServerError::Domain(TmError::AuthorityDenied(
            "only a human operator may mint agent tokens".to_string(),
        )));
    }
    let name = body.name.trim();
    if name.is_empty()
        || name.len() > 64
        || name.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ServerError::BadRequest(
            "name must be 1-64 characters with no whitespace".to_string(),
        ));
    }
    let participant = ParticipantId::new(format!("agent:api/{name}"))
        .map_err(|e| ServerError::BadRequest(e.to_string()))?;
    let token = state.credentials.mint_agent(participant.clone());
    Ok((
        StatusCode::CREATED,
        Json(json!({"participant": participant, "token": token})),
    ))
}

async fn create_session(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateSessionRequest>,
) -> (StatusCode, Json<Value>) {
    let id = state.ids.next(IdKind::Session);
    (
        StatusCode::CREATED,
        Json(json!({"session": id.as_str(), "label": body.label})),
    )
}

/// No durable session record exists to delete (see [`create_session`]); always succeeds.
async fn delete_session(_id: ApiPath<SessionId>) -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn update_presence(
    State(state): State<AppState>,
    ApiPath(_session): ApiPath<SessionId>,
    ApiJson(body): ApiJson<PresenceUpdateRequest>,
) -> Json<Value> {
    let participant = body.participant.clone();
    let ttl_seconds = body
        .ttl_seconds
        .unwrap_or(state.config.presence_ttl_seconds);
    state.presence.upsert(PresenceEntry {
        participant: body.participant,
        ticket: body.ticket,
        file: body.file,
        action: body.action,
        last_seen: state.clock.now(),
        ttl_seconds,
    });
    Json(json!({"participant": participant}))
}

async fn get_presence(State(state): State<AppState>) -> Result<Json<Value>, ServerError> {
    state.presence.sweep_expired(state.clock.now());
    let participants: Vec<Value> = state
        .presence
        .snapshot()
        .iter()
        .map(presence_entry_json)
        .collect();
    let view = state.store.view()?;
    let leases: Vec<Value> = path_leases(&view).iter().map(path_lease_json).collect();
    Ok(Json(
        json!({"participants": participants, "path_leases": leases}),
    ))
}

async fn get_providers(State(state): State<AppState>) -> Json<Value> {
    let Some(providers) = &state.providers else {
        return Json(json!({"candidates": []}));
    };
    let guard = providers.lock().unwrap_or_else(|p| p.into_inner());
    let candidates: Vec<Value> = guard
        .candidates()
        .map(|(key, candidate): (&CandidateKey, &CandidateState)| {
            json!({"key": key, "state": candidate})
        })
        .collect();
    Json(json!({"candidates": candidates}))
}

async fn get_harness(State(state): State<AppState>) -> Json<Value> {
    match &state.harness {
        Some(registry) => Json(json!({"current": harness_epoch_json(registry.current())})),
        None => Json(json!({"current": Value::Null})),
    }
}

async fn get_metrics(State(state): State<AppState>) -> Result<Json<Value>, ServerError> {
    let view = state.store.view()?;
    let head = state.events.head()?;
    let mut by_state: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for ticket in view.tickets.values() {
        *by_state
            .entry(ticket_state_label(ticket.state))
            .or_insert(0) += 1;
    }
    Ok(Json(json!({
        "event_head": head,
        "ticket_count": view.tickets.len(),
        "tickets_by_state": by_state,
        "lease_count": view.leases.len(),
        "decision_count": view.decisions.len(),
        "milestone_count": view.milestones.len(),
        "artifact_count": view.artifacts.len(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tm_types::{Clock, CounterIds, FixedClock, IdSource, Role, Tolerance};

    use crate::state::ServerConfig;

    fn test_state() -> (TempDir, AppState) {
        let dir = TempDir::new().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let config = ServerConfig {
            project_root: dir.path().to_path_buf(),
            state_dir: dir.path().join(".tm"),
            bind_addr: "127.0.0.1:0".parse().expect("valid loopback addr"),
            token: None,
            presence_ttl_seconds: 60,
            broadcast_poll_interval: Duration::from_millis(10),
            sse_replay_page_size: 100,
            sse_keep_alive: Duration::from_secs(15),
            workers: false,
        };
        let state = AppState::open(config, clock, ids).expect("open app state");
        (dir, state)
    }

    fn executor() -> ExecutorRequirements {
        ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Any,
        }
    }

    fn retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 60,
        }
    }

    fn actor() -> ParticipantId {
        ParticipantId::system()
    }

    fn create_ticket_body() -> CreateTicketRequest {
        CreateTicketRequest {
            kind: TicketKind::Work,
            objective: "do the thing".to_string(),
            parent: None,
            milestone: None,
            authority: Authority::none(),
            resources: Vec::new(),
            executor: executor(),
            context_refs: Vec::new(),
            success: Vec::new(),
            verification: VerificationPolicy::None,
            budget: Budget::none(),
            retry: retry(),
            priority: 0,
            actor: actor(),
        }
    }

    async fn make_ticket(state: &AppState) -> TicketId {
        let (_, Json(body)) = create_ticket(State(state.clone()), create_ticket_body())
            .await
            .expect("create ticket");
        let id = body["ticket"]["id"]
            .as_str()
            .expect("ticket id")
            .to_string();
        TicketId::new(id).expect("valid ticket id")
    }

    fn status_of(err: ServerError) -> StatusCode {
        err.into_response().status()
    }

    #[tokio::test]
    async fn health_reports_ok() {
        let (_dir, state) = test_state();
        let Json(body) = health(State(state)).await;
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn health_and_state_say_whether_workers_are_running() {
        let (_dir, mut state) = test_state();
        for workers in [false, true] {
            state.config.workers = workers;
            let Json(health) = health(State(state.clone())).await;
            assert_eq!(health["workers"], workers);
            let Json(snapshot) = get_state(State(state.clone())).await.expect("state");
            assert_eq!(snapshot["workers"], workers);
        }
    }

    #[tokio::test]
    async fn the_schema_lists_the_human_review_transitions_and_ticket_history() {
        let Json(schema) = get_schema().await;
        let transitions = &schema["definitions"]["TransitionRequest"]["properties"];
        for key in ["accept", "reject", "retry", "activate", "cancel", "fail"] {
            assert!(
                transitions.get(key).is_some(),
                "{key} missing: {transitions}"
            );
        }
        assert_eq!(
            transitions["reject"]["required"],
            json!(["reason", "actor"])
        );
        assert!(schema["definitions"].get("TicketEventsPage").is_some());
        assert!(schema["definitions"]["Health"]["properties"]
            .get("workers")
            .is_some());
    }

    #[tokio::test]
    async fn a_reject_with_a_blank_reason_is_a_bad_request() {
        let (_dir, state) = test_state();
        let id = make_ready_ticket(&state).await;
        for reason in ["", "   ", "\n\t"] {
            let request: TransitionRequest = serde_json::from_value(
                json!({"reject": {"reason": reason, "actor": "human:owner"}}),
            )
            .expect("wire shape");
            let err =
                transition_ticket(State(state.clone()), ApiPath(id.clone()), ApiJson(request))
                    .await
                    .expect_err("a blank reason is refused");
            assert_eq!(status_of(err), StatusCode::BAD_REQUEST, "{reason:?}");
        }
        let view = state.store.view().expect("view");
        assert_eq!(
            view.tickets[&id].state,
            TicketState::Ready,
            "nothing changed"
        );
    }

    #[tokio::test]
    async fn ticket_history_pages_only_that_tickets_events() {
        let (_dir, state) = test_state();
        let first = make_ticket(&state).await;
        let second = make_ticket(&state).await;
        for id in [&first, &second, &first, &second] {
            state
                .store
                .update_ticket(id, json!({"priority": 1}), actor())
                .expect("update");
        }
        let all_first: Vec<u64> = state
            .events
            .read_subject(&tm_types::Id::from(first.clone()))
            .expect("subject")
            .iter()
            .map(|e| e.seq)
            .collect();
        assert!(all_first.len() > 2, "enough to need several pages");

        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let query = TicketEventsQuery {
                after,
                limit: Some(2),
            };
            let Json(page) = ticket_events(
                State(state.clone()),
                ApiPath(first.clone()),
                axum::extract::Query(query),
            )
            .await
            .expect("page");
            let events = page["events"].as_array().expect("events").clone();
            assert!(events.len() <= 2);
            for e in &events {
                assert_eq!(e["subject"], first.as_str(), "only this ticket's events");
                seen.push(e["seq"].as_u64().expect("seq"));
            }
            match page["next"].as_u64() {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, all_first, "every event once, oldest first");

        // `from` is the same exclusive cursor as `after`, as on `GET /events`.
        let page = |q: &str| {
            let query: axum::extract::Query<TicketEventsQuery> =
                axum::extract::Query::try_from_uri(&format!("/x?{q}").parse().expect("uri"))
                    .expect("query parses");
            ticket_events(State(state.clone()), ApiPath(first.clone()), query)
        };
        let Json(by_from) = page(&format!("from={}&limit=2", all_first[0]))
            .await
            .expect("page");
        let Json(by_after) = page(&format!("after={}&limit=2", all_first[0]))
            .await
            .expect("page");
        assert_eq!(by_from, by_after);
        assert_eq!(by_from["events"][0]["seq"].as_u64(), Some(all_first[1]));

        let err = ticket_events(
            State(state.clone()),
            ApiPath(TicketId::new("T-999").expect("id")),
            axum::extract::Query(TicketEventsQuery::default()),
        )
        .await
        .expect_err("unknown ticket");
        assert_eq!(status_of(err), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_ticket_then_fetch_it() {
        let (_dir, state) = test_state();
        let id = make_ticket(&state).await;
        let Json(ticket) = get_ticket(State(state), ApiPath(id.clone()))
            .await
            .expect("get ticket");
        assert_eq!(ticket.id, id);
        assert_eq!(ticket.objective, "do the thing");
        assert_eq!(ticket.state, TicketState::Draft);
    }

    #[tokio::test]
    async fn a_ticket_posted_with_only_an_objective_gets_worker_defaults() {
        let (_dir, state) = test_state();
        let body: CreateTicketRequest = serde_json::from_value(serde_json::json!({
            "kind": "work",
            "objective": "fix the flaky test",
            "actor": actor(),
        }))
        .expect("kind, objective and actor are enough");
        let (_, Json(created)) = create_ticket(State(state.clone()), body)
            .await
            .expect("create ticket");
        let id = TicketId::new(created["ticket"]["id"].as_str().expect("id")).expect("valid id");
        let Json(ticket) = get_ticket(State(state), ApiPath(id))
            .await
            .expect("get ticket");
        assert_eq!(ticket.authority, Authority::worker());
        assert_eq!(ticket.budget, Budget::unlimited());
        assert_eq!(ticket.executor, ExecutorRequirements::default());
        assert_eq!(ticket.verification, VerificationPolicy::Single);
    }

    #[tokio::test]
    async fn get_missing_ticket_is_not_found() {
        let (_dir, state) = test_state();
        let missing = TicketId::new("T-999").expect("valid shape");
        let err = get_ticket(State(state), ApiPath(missing))
            .await
            .unwrap_err();
        assert_eq!(status_of(err), StatusCode::NOT_FOUND);
    }

    /// Spins up the full [`router`] on a loopback port so these two tests exercise the exact
    /// path a real client takes — `Path`/`Json` extractor rejections included — rather than
    /// calling handler functions directly, which would bypass axum's own extraction and
    /// therefore never observe the plain-text-vs-JSON bug `p1-http-error-json-format` fixes.
    async fn spawn_router(state: AppState) -> std::net::SocketAddr {
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    /// `p1-http-error-json-format` acceptance: `GET /tickets/NOTFOUND` (not a valid `T-<n>`/
    /// `V-<n>`/`A-<n>` shape) must return JSON with non-empty `error`/`message` fields, not
    /// axum's default plain-text `PathRejection` body — the message must still name the
    /// expected ID shape.
    #[tokio::test]
    async fn a_malformed_ticket_id_in_the_url_renders_as_json() {
        let (_dir, state) = test_state();
        let token = state.credentials.operator_token().to_string();
        let addr = spawn_router(state).await;
        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/tickets/NOTFOUND"))
            .bearer_auth(&token)
            .send()
            .await
            .expect("request");
        assert!(resp.status().is_client_error(), "{}", resp.status());
        let body: Value = resp.json().await.expect("JSON body, not plain text");
        let error = body["error"].as_str().expect("non-empty error field");
        let message = body["message"].as_str().expect("non-empty message field");
        assert!(!error.is_empty());
        assert!(
            message.contains("T-<n>"),
            "message should name the expected ID shape: {message}"
        );
    }

    /// `p1-http-error-json-format` acceptance: `POST /tickets` with `{}` must name every missing
    /// required field (`objective`, `kind`, `actor`) together in one JSON message, not just the
    /// first one serde would have chosen to choke on.
    #[tokio::test]
    async fn create_ticket_with_no_fields_names_every_missing_one() {
        let (_dir, state) = test_state();
        let token = state.credentials.operator_token().to_string();
        let addr = spawn_router(state).await;
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/tickets"))
            .bearer_auth(&token)
            .json(&json!({}))
            .send()
            .await
            .expect("request");
        assert!(resp.status().is_client_error(), "{}", resp.status());
        let body: Value = resp.json().await.expect("JSON body, not plain text");
        let message = body["message"].as_str().expect("non-empty message field");
        for field in ["objective", "kind", "actor"] {
            assert!(
                message.contains(field),
                "message should name {field}: {message}"
            );
        }
    }

    #[tokio::test]
    async fn list_tickets_reports_every_created_ticket() {
        let (_dir, state) = test_state();
        make_ticket(&state).await;
        make_ticket(&state).await;
        let Json(tickets) = list_tickets(State(state)).await.expect("list tickets");
        assert_eq!(tickets.len(), 2);
    }

    #[tokio::test]
    async fn app_state_open_with_a_state_dir_outside_project_root_serves_tickets_from_it() {
        // D-003: `state_dir` need not be `<project_root>/.tm` (a global-scope project's state
        // lives under `$TM_HOME/projects/<key>/`, unrelated to the workspace it indexes). Assert
        // the server reads/writes tickets from `config.state_dir`, not a `.tm` under
        // `project_root`.
        let workspace = TempDir::new().expect("workspace tempdir");
        let state_home = TempDir::new().expect("state tempdir");
        let state_dir = state_home.path().join("global-state");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let config = ServerConfig {
            project_root: workspace.path().to_path_buf(),
            state_dir: state_dir.clone(),
            bind_addr: "127.0.0.1:0".parse().expect("valid loopback addr"),
            token: None,
            presence_ttl_seconds: 60,
            broadcast_poll_interval: Duration::from_millis(10),
            sse_replay_page_size: 100,
            sse_keep_alive: Duration::from_secs(15),
            workers: false,
        };
        let state = AppState::open(config, clock, ids).expect("open app state");

        make_ticket(&state).await;
        make_ticket(&state).await;
        let Json(tickets) = list_tickets(State(state)).await.expect("list tickets");
        assert_eq!(tickets.len(), 2);

        assert!(
            state_dir.join("project.db").exists(),
            "AppState::open must write project.db under config.state_dir"
        );
        assert!(
            !workspace.path().join(".tm").exists(),
            "AppState::open must not create a .tm directory under project_root when state_dir \
             points elsewhere"
        );
    }

    #[tokio::test]
    async fn update_ticket_changes_objective() {
        let (_dir, state) = test_state();
        let id = make_ticket(&state).await;
        let body = UpdateTicketRequest {
            fields: json!({"priority": 5}),
            actor: actor(),
        };
        let Json(ticket) = update_ticket(State(state), ApiPath(id), ApiJson(body))
            .await
            .expect("update ticket");
        assert_eq!(ticket.priority, 5);
    }

    #[tokio::test]
    async fn update_missing_ticket_is_not_found() {
        let (_dir, state) = test_state();
        let missing = TicketId::new("T-999").expect("valid shape");
        let body = UpdateTicketRequest {
            fields: json!({}),
            actor: actor(),
        };
        let err = update_ticket(State(state), ApiPath(missing), ApiJson(body))
            .await
            .unwrap_err();
        assert_eq!(status_of(err), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn transition_activate_moves_ticket_out_of_draft() {
        let (_dir, state) = test_state();
        let id = make_ticket(&state).await;
        let _ = transition_ticket(
            State(state.clone()),
            ApiPath(id.clone()),
            ApiJson(TransitionRequest::Activate { actor: actor() }),
        )
        .await
        .expect("activate");
        let Json(ticket) = get_ticket(State(state), ApiPath(id))
            .await
            .expect("get ticket");
        assert_ne!(ticket.state, TicketState::Draft);
    }

    #[tokio::test]
    async fn accept_reject_and_retry_are_transitions_only_a_human_can_make() {
        let (_dir, state) = test_state();
        let id = make_ready_ticket(&state).await;
        for body in [
            json!({"accept": {"actor": "system"}}),
            json!({"reject": {"reason": "no tests", "actor": "system"}}),
            json!({"retry": {"guidance": "try again", "actor": "system"}}),
        ] {
            let request: TransitionRequest = serde_json::from_value(body).expect("wire shape");
            let err =
                transition_ticket(State(state.clone()), ApiPath(id.clone()), ApiJson(request))
                    .await
                    .expect_err("the system is not a human");
            assert_ne!(status_of(err), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn transition_on_missing_ticket_is_not_found() {
        let (_dir, state) = test_state();
        let missing = TicketId::new("T-999").expect("valid shape");
        let err = transition_ticket(
            State(state),
            ApiPath(missing),
            ApiJson(TransitionRequest::Activate { actor: actor() }),
        )
        .await
        .unwrap_err();
        assert_eq!(status_of(err), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn transition_generic_trigger_reports_invalid_transition_as_conflict() {
        let (_dir, state) = test_state();
        let id = make_ticket(&state).await;
        // A Draft ticket cannot receive LeaseAcquired directly.
        let err = transition_ticket(
            State(state),
            ApiPath(id),
            ApiJson(TransitionRequest::Trigger {
                trigger: Trigger::LeaseAcquired,
                actor: actor(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(status_of(err), StatusCode::CONFLICT);
    }

    async fn make_ready_ticket(state: &AppState) -> TicketId {
        let id = make_ticket(state).await;
        let _ = transition_ticket(
            State(state.clone()),
            ApiPath(id.clone()),
            ApiJson(TransitionRequest::Activate { actor: actor() }),
        )
        .await
        .expect("activate");
        id
    }

    #[tokio::test]
    async fn acquire_heartbeat_release_lease_roundtrip() {
        let (_dir, state) = test_state();
        let id = make_ready_ticket(&state).await;
        let (status, Json(body)) = acquire_lease(
            State(state.clone()),
            ApiPath(id.clone()),
            ApiJson(AcquireLeaseRequest {
                holder: actor(),
                authority: Authority::none(),
                resources: Vec::new(),
                ttl_seconds: 60,
                actor: actor(),
            }),
        )
        .await
        .expect("acquire lease");
        assert_eq!(status, StatusCode::CREATED);
        let lease_id_str = body["lease"]["id"].as_str().expect("lease id").to_string();
        let lease_id = LeaseId::new(lease_id_str).expect("valid lease id");

        let Json(hb) = heartbeat_lease(
            State(state.clone()),
            ApiPath(lease_id.clone()),
            ApiJson(ActorOnlyRequest { actor: actor() }),
        )
        .await
        .expect("heartbeat");
        assert!(!hb["events"].as_array().expect("events array").is_empty());

        let Json(rel) = release_lease(
            State(state),
            ApiPath(lease_id),
            ApiJson(ActorOnlyRequest { actor: actor() }),
        )
        .await
        .expect("release");
        assert!(!rel["events"].as_array().expect("events array").is_empty());
    }

    #[tokio::test]
    async fn heartbeat_on_unknown_lease_is_not_found() {
        let (_dir, state) = test_state();
        let missing = LeaseId::new("L-000000000000").expect("valid shape");
        let err = heartbeat_lease(
            State(state),
            ApiPath(missing),
            ApiJson(ActorOnlyRequest { actor: actor() }),
        )
        .await
        .unwrap_err();
        assert_eq!(status_of(err), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn attach_evidence_records_an_event() {
        let (_dir, state) = test_state();
        let id = make_ticket(&state).await;
        let (_, Json(artifact_body)) = create_artifact(
            State(state.clone()),
            ApiJson(CreateArtifactRequest {
                kind: ArtifactKind::Report,
                media_type: "text/plain".to_string(),
                bytes: b"ok".to_vec(),
                meta: json!({}),
                ticket: Some(id.clone()),
                actor: actor(),
            }),
        )
        .await
        .expect("create artifact");
        let artifact_id_str = artifact_body["artifact"]["id"]
            .as_str()
            .expect("artifact id")
            .to_string();
        let artifact_id = ArtifactId::new(artifact_id_str).expect("valid artifact id");

        let Json(evidence_body) = attach_evidence(
            State(state),
            ApiPath(id),
            ApiJson(AttachEvidenceRequest {
                kind: EvidenceKind::Review,
                artifact: artifact_id,
                summary: "looks fine".to_string(),
                actor: actor(),
            }),
        )
        .await
        .expect("attach evidence");
        assert!(!evidence_body["events"]
            .as_array()
            .expect("events array")
            .is_empty());
    }

    #[tokio::test]
    async fn graph_reports_dependency_edge() {
        let (_dir, state) = test_state();
        let a = make_ticket(&state).await;
        let b = make_ticket(&state).await;
        state
            .store
            .add_dependency(&b, &a, tm_core::DependencyKind::Hard, actor())
            .expect("add dependency");
        let Json(graph) = get_graph(State(state)).await.expect("get graph");
        let edges = graph["edges"].as_array().expect("edges array");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], b.as_str());
        assert_eq!(edges[0]["to"], a.as_str());
    }

    #[tokio::test]
    async fn create_and_list_decisions() {
        let (_dir, state) = test_state();
        let (status, Json(body)) = create_decision(
            State(state.clone()),
            ApiJson(DecisionRequest {
                subject: "use sqlite".to_string(),
                decision: "yes".to_string(),
                reason: "simplicity".to_string(),
                evidence: Vec::new(),
                affected_tickets: Vec::new(),
                affected_paths: Vec::new(),
                actor: actor(),
            }),
        )
        .await
        .expect("create decision");
        assert_eq!(status, StatusCode::CREATED);
        let decision_id_str = body["decision"]["id"]
            .as_str()
            .expect("decision id")
            .to_string();

        let Json(list) = list_decisions(State(state)).await.expect("list decisions");
        let decisions = list.as_array().expect("array");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["id"], decision_id_str);
    }

    #[tokio::test]
    async fn supersede_decision_links_the_two_records() {
        let (_dir, state) = test_state();
        let (_, Json(first)) = create_decision(
            State(state.clone()),
            ApiJson(DecisionRequest {
                subject: "use sqlite".to_string(),
                decision: "yes".to_string(),
                reason: "simplicity".to_string(),
                evidence: Vec::new(),
                affected_tickets: Vec::new(),
                affected_paths: Vec::new(),
                actor: actor(),
            }),
        )
        .await
        .expect("create decision");
        let first_id = DecisionId::new(first["decision"]["id"].as_str().expect("id").to_string())
            .expect("valid decision id");

        let (_, Json(second)) = supersede_decision(
            State(state),
            ApiPath(first_id.clone()),
            ApiJson(DecisionRequest {
                subject: "use sqlite".to_string(),
                decision: "no, postgres".to_string(),
                reason: "scale".to_string(),
                evidence: Vec::new(),
                affected_tickets: Vec::new(),
                affected_paths: Vec::new(),
                actor: actor(),
            }),
        )
        .await
        .expect("supersede decision");
        assert_eq!(second["decision"]["supersedes"], first_id.as_str());
    }

    #[tokio::test]
    async fn create_close_reopen_milestone() {
        let (_dir, state) = test_state();
        let id = make_ticket(&state).await;
        // Milestone membership requires the ticket to exist; close requires it terminal.
        let (_, Json(created)) = create_milestone(
            State(state.clone()),
            ApiJson(CreateMilestoneRequest {
                title: "v1".to_string(),
                tickets: vec![id.clone()],
                assumptions: Vec::new(),
                actor: actor(),
            }),
        )
        .await
        .expect("create milestone");
        let milestone_id = MilestoneId::new(
            created["milestone"]["id"]
                .as_str()
                .expect("milestone id")
                .to_string(),
        )
        .expect("valid milestone id");

        // Member ticket is still Draft, so close must fail with a conflict/invariant error.
        let close_err = close_milestone(
            State(state.clone()),
            ApiPath(milestone_id.clone()),
            ApiJson(ActorOnlyRequest { actor: actor() }),
        )
        .await
        .unwrap_err();
        assert_ne!(status_of(close_err), StatusCode::OK);

        let Json(list) = list_milestones(State(state))
            .await
            .expect("list milestones");
        assert_eq!(list.as_array().expect("array").len(), 1);
    }

    #[tokio::test]
    async fn create_artifact_then_get_it() {
        let (_dir, state) = test_state();
        let (status, Json(body)) = create_artifact(
            State(state.clone()),
            ApiJson(CreateArtifactRequest {
                kind: ArtifactKind::CommandOutput,
                media_type: "text/plain".to_string(),
                bytes: b"hello".to_vec(),
                meta: json!({"cmd": "echo hello"}),
                ticket: None,
                actor: actor(),
            }),
        )
        .await
        .expect("create artifact");
        assert_eq!(status, StatusCode::CREATED);
        let id = ArtifactId::new(body["artifact"]["id"].as_str().expect("id").to_string())
            .expect("valid artifact id");

        let Json(fetched) = get_artifact(State(state), ApiPath(id))
            .await
            .expect("get artifact");
        assert_eq!(fetched["media_type"], "text/plain");
        assert_eq!(fetched["bytes_len"], 5);
    }

    #[tokio::test]
    async fn get_missing_artifact_is_not_found() {
        let (_dir, state) = test_state();
        let missing = ArtifactId::new("ART-000000000000").expect("valid shape");
        let err = get_artifact(State(state), ApiPath(missing))
            .await
            .unwrap_err();
        assert_eq!(status_of(err), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn docs_list_is_always_empty_and_writes_are_refused() {
        let Json(list) = list_docs().await;
        assert_eq!(list["docs"].as_array().expect("array").len(), 0);
        let err = create_doc().await;
        assert_eq!(status_of(err), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn approval_open_blocks_until_decided_then_records_a_decision() {
        let (_dir, state) = test_state();
        let opener_state = state.clone();
        let handle = tokio::spawn(async move {
            create_approval(
                State(opener_state),
                ApiJson(CreateApprovalRequest {
                    ticket: None,
                    requested_by: actor(),
                    subject: "force-push to main".to_string(),
                    detail: "recovering from a bad rebase".to_string(),
                }),
            )
            .await
        });

        // Give the opener a chance to register before we try to list/decide it.
        tokio::task::yield_now().await;
        let mut pending = Vec::new();
        for _ in 0..50 {
            let Json(list) = list_approvals(State(state.clone())).await;
            pending = list.as_array().expect("array").clone();
            if !pending.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(pending.len(), 1);
        let approval_id = pending[0]["id"].as_str().expect("approval id").to_string();

        let Json(decided) = decide_approval(
            State(state),
            Path(approval_id),
            ApiJson(DecideApprovalRequest {
                decision: ApprovalDecisionBody::Approve {
                    note: Some("looks safe".to_string()),
                },
                decided_by: ParticipantId::new("human:tester").expect("participant"),
            }),
        )
        .await
        .expect("decide approval");
        assert_eq!(decided["status"], "decided");

        let opened = handle.await.expect("task join").expect("create_approval");
        let Json(opened_body) = opened;
        assert_eq!(opened_body["decision"]["approve"]["note"], "looks safe");
    }

    #[tokio::test]
    async fn get_unknown_approval_is_bad_request() {
        let (_dir, state) = test_state();
        let err = get_approval(State(state), Path("AP-unknown".to_string()))
            .await
            .unwrap_err();
        assert_eq!(status_of(err), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn artifact_json_does_not_leak_the_disk_path() {
        let artifact = Artifact {
            id: ArtifactId::new("ART-9f2a1c0b77de").expect("id"),
            kind: ArtifactKind::CommandOutput,
            media_type: "text/plain".to_string(),
            bytes_len: 10,
            hash: "abc".to_string(),
            storage: ArtifactStorage::OnDisk(std::path::PathBuf::from("/home/someone/.tm/a")),
            meta: json!({}),
        };
        let rendered = artifact_json(&artifact).to_string();
        assert!(!rendered.contains("/home/someone"), "{rendered}");
        assert!(rendered.contains("disk"));
    }

    #[tokio::test]
    async fn create_session_then_delete_it() {
        let (_dir, state) = test_state();
        let (status, Json(body)) =
            create_session(State(state), ApiJson(CreateSessionRequest { label: None })).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = SessionId::new(body["session"].as_str().expect("session id").to_string())
            .expect("valid session id");
        let delete_status = delete_session(ApiPath(id)).await;
        assert_eq!(delete_status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn presence_update_then_list() {
        let (_dir, state) = test_state();
        let session = SessionId::new("S-1").expect("valid shape");
        let participant = ParticipantId::new("human:alice").expect("valid participant");
        let _ = update_presence(
            State(state.clone()),
            ApiPath(session),
            ApiJson(PresenceUpdateRequest {
                participant: participant.clone(),
                ticket: None,
                file: Some("src/main.rs".to_string()),
                action: "editing".to_string(),
                ttl_seconds: Some(120),
            }),
        )
        .await;
        let Json(presence) = get_presence(State(state)).await.expect("get presence");
        let participants = presence["participants"].as_array().expect("array");
        assert_eq!(participants.len(), 1);
        assert_eq!(participants[0]["participant"], participant.as_str());
    }

    #[tokio::test]
    async fn providers_reports_empty_when_unwired() {
        let (_dir, state) = test_state();
        let Json(body) = get_providers(State(state)).await;
        assert_eq!(body["candidates"].as_array().expect("array").len(), 0);
    }

    #[tokio::test]
    async fn harness_reports_null_when_unwired() {
        let (_dir, state) = test_state();
        let Json(body) = get_harness(State(state)).await;
        assert!(body["current"].is_null());
    }

    #[tokio::test]
    async fn metrics_counts_tickets_by_state() {
        let (_dir, state) = test_state();
        make_ticket(&state).await;
        make_ready_ticket(&state).await;
        let Json(metrics) = get_metrics(State(state)).await.expect("get metrics");
        assert_eq!(metrics["ticket_count"], 2);
        assert_eq!(metrics["tickets_by_state"]["draft"], 1);
    }

    #[tokio::test]
    async fn schema_endpoint_returns_a_document_with_definitions() {
        let Json(schema) = get_schema().await;
        assert!(schema["definitions"]["TicketId"].is_object());
    }

    #[tokio::test]
    async fn wiki_page_is_served_as_rendered_html_with_a_ticket_link() {
        let (dir, state) = test_state();
        std::fs::create_dir_all(dir.path().join("docs/wiki")).expect("mkdir");
        std::fs::write(
            dir.path().join("docs/wiki/glossary.md"),
            "+++\n[doc]\nid = \"wiki/glossary\"\nmode = \"generated\"\nderived_from = []\n+++\n\n# Glossary\n\nSee T-1 for an example.\n",
        )
        .expect("write page");

        let html = crate::wiki::get_wiki_page(State(state), Path("glossary".to_string()))
            .await
            .expect("page renders")
            .into_response();
        let body = axum::body::to_bytes(html.into_body(), usize::MAX)
            .await
            .expect("collect body");
        let rendered = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(rendered.contains("<h1>Glossary</h1>"));
        assert!(rendered.contains("<a href=\"/tickets/T-1\">T-1</a>"));
        // The front-matter block itself must not leak into the rendered page.
        assert!(!rendered.contains("[doc]"));
    }

    #[tokio::test]
    async fn wiki_page_missing_on_disk_is_not_found() {
        let (_dir, state) = test_state();
        let result =
            crate::wiki::get_wiki_page(State(state), Path("does-not-exist".to_string())).await;
        assert!(matches!(
            result,
            Err(ServerError::Domain(tm_types::TmError::NotFound { .. }))
        ));
    }

    #[tokio::test]
    async fn wiki_index_lists_pages_under_docs_wiki() {
        let (dir, state) = test_state();
        std::fs::create_dir_all(dir.path().join("docs/wiki/architecture")).expect("mkdir");
        std::fs::write(dir.path().join("docs/wiki/glossary.md"), "# Glossary\n").expect("write");
        std::fs::write(
            dir.path().join("docs/wiki/architecture/tm-core.md"),
            "# Architecture\n",
        )
        .expect("write");

        let html = crate::wiki::list_wiki_pages(State(state))
            .await
            .expect("index renders")
            .into_response();
        let body = axum::body::to_bytes(html.into_body(), usize::MAX)
            .await
            .expect("collect body");
        let rendered = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(rendered.contains("href=\"/wiki/glossary\""));
        assert!(rendered.contains("href=\"/wiki/architecture/tm-core\""));
    }
}
