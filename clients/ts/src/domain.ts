// Hand-written wire types mirroring `tm-core`/`tm-events`/`tm-server`'s JSON rendering.
//
// `GET /schema` (see `src/generated.ts`) only covers a handful of definitions today
// (`TicketId`, `ParticipantId`, `TicketKind`, `TicketState`, `CreateTicketRequest`,
// `TransitionRequest`) — that is the *actual, current* state of the server's schema endpoint,
// not an oversight in this client. Everything below is transcribed by hand from the Rust wire
// shapes (`tm-server/src/routes.rs`'s `*_json` helpers and request-body structs, and
// `tm-events/src/payload.rs`'s payload catalogue) so this client can still offer full typing.
// When the server's `GET /schema` grows to cover these, the corresponding hand-written type
// here should be deleted in favor of the generated one, not kept in parallel.

import type { ParticipantId, TicketId, TicketKind, TicketState } from "./generated.js";

export type { ParticipantId, TicketId, TicketKind, TicketState } from "./generated.js";

export type MilestoneId = string;
export type DecisionId = string;
export type ArtifactId = string;
export type LeaseId = string;
export type SessionId = string;

/** RFC3339 timestamp string, as rendered by `tm_types::Timestamp`'s `Serialize` impl. */
export type Timestamp = string;

export type Trigger =
  | "activate"
  | "dependencies_satisfied"
  | "dependencies_unsatisfied"
  | "lease_acquired"
  | "work_started"
  | "lease_expired"
  | "lease_released"
  | "submit"
  | "verification_started"
  | "verification_passed"
  | "verification_failed"
  | "audit_passed"
  | "audit_rejected_minor"
  | "audit_rejected_structural"
  | "failed"
  | "retry_scheduled"
  | "retry_exhausted"
  | "rework_restarted"
  | "replanned"
  | "escalation_resolved"
  | "abandoned"
  | "reopen"
  | "cancel";

export type DependencyKind = "hard" | "soft" | "loop";
export type ResourceMode = "exclusive" | "shared";

export interface ResourceClaim {
  paths: string[];
  mode: ResourceMode;
}

export type Role =
  | "coder_fast"
  | "coder_careful"
  | "planner"
  | "reviewer"
  | "researcher"
  | "human";

export type Tolerance = "strict" | "any";

export interface ExecutorRequirements {
  role: Role;
  human_required: boolean;
  min_capability: Tolerance;
}

export interface ContextRef {
  locator: string;
  reason: string;
}

export type VerificationPolicy = "none" | "single" | "every_predicate" | "audited";

export interface RetryPolicy {
  max_attempts: number;
  base_delay_seconds: number;
  backoff_multiplier: number;
  max_delay_seconds: number;
}

export interface CycleBudget {
  max_iterations: number;
  iterations: number;
}

export type FailureClass =
  | "executor_crash"
  | "verification_failed"
  | "audit_rejected"
  | "provider_unavailable"
  | "budget_exhausted"
  | "authority_denied"
  | "resource_conflict"
  | "other";

export interface FailureRecord {
  class: FailureClass;
  detail: string;
  at: Timestamp;
  attempt: number;
}

/** Free-form; `tm_types::Authority`'s wire shape is not yet asserted by this client. */
export type Authority = unknown;
/** Free-form; `tm_types::Budget`'s wire shape is not yet asserted by this client. */
export type Budget = unknown;
/** Free-form; `tm_types::Predicate`'s wire shape is not yet asserted by this client. */
export type Predicate = unknown;

export interface Ticket {
  id: TicketId;
  kind: TicketKind;
  objective: string;
  state: TicketState;
  parent: TicketId | null;
  children: TicketId[];
  dependencies: TicketId[];
  milestone: MilestoneId | null;
  authority: Authority;
  resources: ResourceClaim[];
  executor: ExecutorRequirements;
  context_refs: ContextRef[];
  success: Predicate[];
  verification: VerificationPolicy;
  budget: Budget;
  retry: RetryPolicy;
  cycle: CycleBudget | null;
  attempts: number;
  failures: FailureRecord[];
  priority: number;
  created: Timestamp;
  updated: Timestamp;
}

export interface Lease {
  id: LeaseId;
  ticket: TicketId;
  holder: ParticipantId;
  authority: Authority;
  resources: ResourceClaim[];
  acquired: Timestamp;
  heartbeat: Timestamp;
  ttl_seconds: number;
  epoch: number;
}

export interface Decision {
  id: DecisionId;
  subject: string;
  decision: string;
  reason: string;
  evidence: ArtifactId[];
  affected_tickets: TicketId[];
  affected_paths: string[];
  author: ParticipantId;
  ts: Timestamp;
  supersedes: DecisionId | null;
  superseded_by: DecisionId | null;
}

export type MilestoneState = "open" | "closed";

export interface Milestone {
  id: MilestoneId;
  title: string;
  tickets: TicketId[];
  state: MilestoneState;
  closed_by: ParticipantId | null;
  assumptions: DecisionId[];
}

export type ArtifactKind =
  | "command_output"
  | "patch"
  | "file"
  | "report"
  | "index"
  | "benchmark"
  | "transcript";

export type ArtifactStorage =
  | { kind: "inline"; len: number }
  | { kind: "disk"; path: string };

export interface Artifact {
  id: ArtifactId;
  kind: ArtifactKind;
  media_type: string;
  bytes_len: number;
  hash: string;
  storage: ArtifactStorage;
  meta: unknown;
}

export type EvidenceKind =
  | "test_run"
  | "diff"
  | "command_output"
  | "review"
  | "human_attestation";

export interface Evidence {
  ticket: TicketId;
  kind: EvidenceKind;
  artifact: ArtifactId;
  produced_by: ParticipantId;
  ts: Timestamp;
  summary: string;
}

export interface ScopedBudget {
  scope: string;
  budget: Budget;
}

/** `GET /state`'s response body. */
export interface ProjectState {
  head: number;
  tickets: Ticket[];
  leases: Lease[];
  decisions: Decision[];
  milestones: Milestone[];
  artifacts: Artifact[];
  evidence: Evidence[];
  budgets: ScopedBudget[];
}

export interface GraphEdge {
  from: TicketId;
  to: TicketId;
  kind: DependencyKind;
}

export interface Graph {
  nodes: TicketId[];
  edges: GraphEdge[];
}

export type AuditOutcome = "passed" | "rejected_minor" | "rejected_structural";

// ------------------------------------------------------------------------------------------
// Event log wire shapes (`tm-events`'s closed `EventKind` catalogue, `SPEC.md` §3.2).
// ------------------------------------------------------------------------------------------

export type EventKind =
  | "project.created"
  | "project.attached"
  | "ticket.created"
  | "ticket.updated"
  | "ticket.state_changed"
  | "ticket.dependency_added"
  | "ticket.dependency_removed"
  | "ticket.child_added"
  | "ticket.leased"
  | "ticket.heartbeat"
  | "ticket.lease_expired"
  | "ticket.lease_released"
  | "ticket.delegated"
  | "ticket.submitted"
  | "ticket.verified"
  | "ticket.verification_failed"
  | "ticket.audited"
  | "ticket.audit_rejected"
  | "ticket.closed"
  | "ticket.cancelled"
  | "ticket.reopened"
  | "ticket.failed"
  | "ticket.retry_scheduled"
  | "ticket.escalated"
  | "ticket.budget_exhausted"
  | "decision.created"
  | "decision.superseded"
  | "authority.granted"
  | "authority.delegated"
  | "authority.revoked"
  | "authority.reverted"
  | "resource.claimed"
  | "resource.released"
  | "resource.conflict_detected"
  | "artifact.created"
  | "command.started"
  | "command.completed"
  | "milestone.created"
  | "milestone.closed"
  | "milestone.reopened"
  | "session.started"
  | "session.joined"
  | "session.left"
  | "session.ended"
  | "presence.updated"
  | "comment.created"
  | "approval.requested"
  | "approval.decided"
  | "provider.selected"
  | "provider.exhausted"
  | "provider.degraded"
  | "provider.recovered"
  | "executor.failed"
  | "usage.recorded"
  | "doc.registered"
  | "doc.generated"
  | "doc.invalidated"
  | "doc.reconciled"
  | "index.updated"
  | "harness.changed"
  | "harness.benchmarked"
  | "harness.promoted"
  | "genesis.started"
  | "genesis.stage_entered"
  | "genesis.stage_completed"
  | "genesis.assumption_recorded"
  | "genesis.maturity_evaluated"
  | "genesis.completed"
  | "mirror.linked"
  | "mirror.pushed"
  | "mirror.pulled";

/** Every payload shape in `tm-events`' closed catalogue (`payload.rs`), keyed by `EventKind`. */
export interface EventPayloadMap {
  "project.created": { name: string; root: string };
  "project.attached": { name: string; root: string };
  "ticket.created": { ticket: TicketId; title: string; parent: TicketId | null };
  "ticket.updated": { ticket: TicketId; fields: Record<string, unknown> };
  "ticket.state_changed": { ticket: TicketId; from: string; to: string };
  "ticket.dependency_added": { ticket: TicketId; depends_on: TicketId };
  "ticket.dependency_removed": { ticket: TicketId; depends_on: TicketId };
  "ticket.child_added": { parent: TicketId; child: TicketId };
  "ticket.leased": { ticket: TicketId; lease: LeaseId; holder: ParticipantId; expires_at: Timestamp };
  "ticket.heartbeat": { ticket: TicketId; lease: LeaseId; expires_at: Timestamp };
  "ticket.lease_expired": { ticket: TicketId; lease: LeaseId };
  "ticket.lease_released": { ticket: TicketId; lease: LeaseId };
  "ticket.delegated": { parent: TicketId; child: TicketId; delegate: ParticipantId };
  "ticket.submitted": { ticket: TicketId; summary: string };
  "ticket.verified": { ticket: TicketId; verifier: TicketId };
  "ticket.verification_failed": { ticket: TicketId; verifier: TicketId; reason: string };
  "ticket.audited": { ticket: TicketId; auditor: TicketId };
  "ticket.audit_rejected": { ticket: TicketId; auditor: TicketId; reason: string };
  "ticket.closed": { ticket: TicketId; reason: string | null };
  "ticket.cancelled": { ticket: TicketId; reason: string | null };
  "ticket.reopened": { ticket: TicketId; reason: string | null };
  "ticket.failed": { ticket: TicketId; reason: string };
  "ticket.retry_scheduled": { ticket: TicketId; attempt: number; not_before: Timestamp };
  "ticket.escalated": { ticket: TicketId; reason: string };
  "ticket.budget_exhausted": { ticket: TicketId; dimension: string; limit: number; spent: number };
  "decision.created": { decision: DecisionId; ticket: TicketId | null; summary: string };
  "decision.superseded": { decision: DecisionId; superseded_by: DecisionId };
  "authority.granted": { subject: ParticipantId; ticket: TicketId | null; grant: Authority };
  "authority.delegated": { from: ParticipantId; to: ParticipantId; ticket: TicketId | null; grant: Authority };
  "authority.revoked": { subject: ParticipantId; ticket: TicketId | null };
  "authority.reverted": { subject: ParticipantId; ticket: TicketId | null; to_seq: number };
  "resource.claimed": { resource: string; holder: ParticipantId };
  "resource.released": { resource: string; holder: ParticipantId };
  "resource.conflict_detected": { resource: string; holders: ParticipantId[] };
  "artifact.created": { artifact: ArtifactId; ticket: TicketId | null; path: string; media_type: string };
  "command.started": { command: string; ticket: TicketId | null; session: SessionId | null };
  "command.completed": {
    command: string;
    ticket: TicketId | null;
    session: SessionId | null;
    exit_code: number;
    duration_ms: number;
  };
  "milestone.created": { milestone: MilestoneId; title: string };
  "milestone.closed": { milestone: MilestoneId };
  "milestone.reopened": { milestone: MilestoneId };
  "session.started": { session: SessionId; participant: ParticipantId };
  "session.joined": { session: SessionId; participant: ParticipantId };
  "session.left": { session: SessionId; participant: ParticipantId };
  "session.ended": { session: SessionId };
  "presence.updated": { participant: ParticipantId; status: string };
  "comment.created": { ticket: TicketId | null; author: ParticipantId; body: string };
  "approval.requested": { ticket: TicketId | null; requested_of: ParticipantId; note: string };
  "approval.decided": { ticket: TicketId | null; decided_by: ParticipantId; approved: boolean; note: string | null };
  "provider.selected": { role: string; provider: string; model: string };
  "provider.exhausted": { role: string; provider: string; reason: string };
  "provider.degraded": { provider: string; reason: string };
  "provider.recovered": { provider: string };
  "executor.failed": { ticket: TicketId | null; reason: string };
  "usage.recorded": {
    ticket: TicketId | null;
    session: SessionId | null;
    tokens: number;
    dollars_micros: number;
    wall_seconds: number;
  };
  "doc.registered": { path: string; ticket: TicketId | null };
  "doc.generated": { path: string; ticket: TicketId | null };
  "doc.invalidated": { path: string; reason: string };
  "doc.reconciled": { path: string };
  "index.updated": { path: string; entries: number };
  "harness.changed": { field: string; from: unknown; to: unknown };
  "harness.benchmarked": { suite: string; score: number };
  "harness.promoted": { candidate: string };
  "genesis.started": { project: string };
  "genesis.stage_entered": { stage: string };
  "genesis.stage_completed": { stage: string };
  "genesis.assumption_recorded": { assumption: string };
  "genesis.maturity_evaluated": { stage: string; score: number };
  "genesis.completed": Record<string, never>;
  "mirror.linked": { remote: string };
  "mirror.pushed": { remote: string; reference: string };
  "mirror.pulled": { remote: string; reference: string };
}

/**
 * One event exactly as it travels over `GET /events`'s SSE `data:` payload
 * (`tm-server/src/sse.rs`'s `WireEvent`).
 */
export interface WireEvent<K extends EventKind = EventKind> {
  seq: number;
  ts: Timestamp;
  kind: K;
  subject: string;
  actor: ParticipantId;
  session: SessionId | null;
  causation: number | null;
  correlation: string | null;
  payload: K extends keyof EventPayloadMap ? EventPayloadMap[K] : Record<string, unknown>;
}

/** The `{"error": "<code>", "message": "<detail>"}` body every non-2xx response carries. */
export interface ErrorBody {
  error: string;
  message: string;
}

// ------------------------------------------------------------------------------------------
// Request bodies (`tm-server/src/routes.rs`'s `Deserialize` structs).
// ------------------------------------------------------------------------------------------

export interface CreateTicketInput {
  kind: TicketKind;
  objective: string;
  parent?: TicketId;
  milestone?: MilestoneId;
  authority?: Authority;
  resources?: ResourceClaim[];
  executor: ExecutorRequirements;
  context_refs?: ContextRef[];
  success?: Predicate[];
  verification: VerificationPolicy;
  budget?: Budget;
  retry: RetryPolicy;
  priority?: number;
  actor: ParticipantId;
}

export interface UpdateTicketInput {
  fields: Record<string, unknown>;
  actor: ParticipantId;
}

export type TransitionCommand =
  | { activate: { actor: ParticipantId } }
  | { trigger: { trigger: Trigger; actor: ParticipantId } }
  | { submit: { summary: string; evidence?: ArtifactId[]; actor: ParticipantId } }
  | { verify: { verifier: TicketId; passed: boolean; reason?: string | null; actor: ParticipantId } }
  | { audit: { auditor: TicketId; outcome: AuditOutcome; reason?: string | null; actor: ParticipantId } }
  | { close: { reason?: string | null; actor: ParticipantId } }
  | { cancel: { reason?: string | null; actor: ParticipantId } }
  | { reopen: { reason?: string | null; actor: ParticipantId } }
  | { fail: { class: FailureClass; detail: string; actor: ParticipantId } };

export interface AcquireLeaseInput {
  holder: ParticipantId;
  authority?: Authority;
  resources?: ResourceClaim[];
  ttl_seconds: number;
  actor: ParticipantId;
}

export interface ActorOnlyInput {
  actor: ParticipantId;
}

export interface AttachEvidenceInput {
  kind: EvidenceKind;
  artifact: ArtifactId;
  summary: string;
  actor: ParticipantId;
}

export interface DecisionInput {
  subject: string;
  decision: string;
  reason: string;
  evidence?: ArtifactId[];
  affected_tickets?: TicketId[];
  affected_paths?: string[];
  actor: ParticipantId;
}

export interface CreateMilestoneInput {
  title: string;
  tickets?: TicketId[];
  assumptions?: DecisionId[];
  actor: ParticipantId;
}

export interface CreateArtifactInput {
  kind: ArtifactKind;
  media_type: string;
  bytes?: number[];
  meta?: unknown;
  ticket?: TicketId;
  actor: ParticipantId;
}

export interface CreateApprovalInput {
  ticket?: TicketId;
  requested_by: ParticipantId;
  subject: string;
  detail?: string;
}

export type ApprovalDecisionInput =
  | { approve: { note?: string | null } }
  | { deny: { reason: string } };

export interface DecideApprovalInput {
  decision: ApprovalDecisionInput;
  decided_by: ParticipantId;
}

export interface CreateSessionInput {
  label?: string | null;
}

export interface PresenceUpdateInput {
  participant: ParticipantId;
  ticket?: TicketId;
  file?: string;
  action?: string;
  ttl_seconds?: number;
}

export interface ApprovalRequestView {
  id: string;
  ticket: TicketId | null;
  requested_by: ParticipantId;
  subject: string;
  detail: string;
  requested_at: Timestamp;
}

export interface PresenceEntryView {
  participant: ParticipantId;
  ticket: TicketId | null;
  file: string | null;
  action: string;
  last_seen: Timestamp;
  ttl_seconds: number;
}

export interface PathLeaseView {
  lease: LeaseId;
  ticket: TicketId;
  holder: ParticipantId;
  mode: ResourceMode;
  paths: string[];
}

export interface PresenceSnapshot {
  participants: PresenceEntryView[];
  path_leases: PathLeaseView[];
}

export interface MetricsSnapshot {
  event_head: number;
  ticket_count: number;
  tickets_by_state: Record<string, number>;
  lease_count: number;
  decision_count: number;
  milestone_count: number;
  artifact_count: number;
}

export class TicketmasterApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, body: ErrorBody) {
    super(`${body.error}: ${body.message}`);
    this.name = "TicketmasterApiError";
    this.status = status;
    this.code = body.error;
  }
}
