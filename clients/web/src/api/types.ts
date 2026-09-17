// Hand-written wire types mirroring the JSON `tm-server` actually emits (see
// `crates/tm-server/src/routes.rs` and `crates/tm-events/src/kind.rs`).
//
// SPEC.md §18.1 describes a generated `@ticketmaster/client` package (`pnpm gen` against
// `GET /schema`) shared by the web app and the VS Code extension. That package does not exist
// yet in this repository (there is no `clients/ts/`), and this task is scoped to
// `clients/web/` only, so these types and the client below are hand-written and live locally
// instead of being imported from a workspace dependency. See ../../README.md for the exact
// scope of this deviation.

export type TicketId = string;
export type MilestoneId = string;
export type ParticipantId = string;
export type LeaseId = string;
export type DecisionId = string;
export type ArtifactId = string;
export type SessionId = string;

export type TicketKind =
  | "work"
  | "verification"
  | "audit"
  | "investigation"
  | "recovery"
  | "harness";

export type TicketState =
  | "draft"
  | "blocked"
  | "ready"
  | "leased"
  | "running"
  | "submitted"
  | "verifying"
  | "auditing"
  | "rework"
  | "replan"
  | "recovery"
  | "escalated"
  | "closed"
  | "cancelled";

export interface ExecutorRequirements {
  role?: string | null;
  [key: string]: unknown;
}

export interface Ticket {
  id: TicketId;
  kind: TicketKind;
  objective: string;
  state: TicketState;
  parent: TicketId | null;
  children: TicketId[];
  dependencies: TicketId[];
  milestone: MilestoneId | null;
  authority: unknown;
  resources: unknown[];
  executor: ExecutorRequirements;
  context_refs: unknown[];
  success: unknown[];
  verification: unknown;
  budget: unknown;
  retry: unknown;
  cycle: unknown | null;
  attempts: number;
  failures: unknown[];
  priority: number;
  created: string;
  updated: string;
}

export interface Lease {
  id: LeaseId;
  ticket: TicketId;
  holder: ParticipantId;
  authority: unknown;
  resources: unknown[];
  acquired: string;
  heartbeat: string;
  ttl_seconds: number;
  epoch: number;
}

export interface Decision {
  id: DecisionId;
  subject: string;
  decision: string;
  reason: string | null;
  evidence: ArtifactId[];
  affected_tickets: TicketId[];
  affected_paths: string[];
  author: ParticipantId;
  ts: string;
  supersedes: DecisionId | null;
  superseded_by: DecisionId | null;
}

export interface Milestone {
  id: MilestoneId;
  title: string;
  tickets: TicketId[];
  state: string;
  closed_by: ParticipantId | null;
  assumptions: unknown[];
}

export interface Artifact {
  id: ArtifactId;
  [key: string]: unknown;
}

export interface Budget {
  scope: string;
  budget: unknown;
}

export interface StateSnapshot {
  head: number;
  tickets: Ticket[];
  leases: Lease[];
  decisions: Decision[];
  milestones: Milestone[];
  artifacts: Artifact[];
  evidence: unknown[];
  budgets: Budget[];
}

export interface PresenceEntry {
  participant: ParticipantId;
  ticket: TicketId | null;
  file: string | null;
  action: string | null;
  last_seen: string;
  ttl_seconds: number;
}

export interface PathLeaseSummary {
  lease: LeaseId;
  ticket: TicketId;
  holder: ParticipantId;
  mode: string;
  paths: string[];
}

export interface PresenceSnapshot {
  participants: PresenceEntry[];
  path_leases: PathLeaseSummary[];
}

/// The dotted event kinds `tm-events` emits; kept as a plain string union rather than
/// re-declaring every one, since the app only branches on a handful for the Status view.
export type EventKind = string;

/** The exact shape `to_sse_event` in `crates/tm-server/src/sse.rs` writes as `data:`. */
export interface WireEvent {
  seq: number;
  ts: string;
  kind: EventKind;
  subject: string;
  actor: ParticipantId;
  session: SessionId | null;
  causation: number | null;
  correlation: string | null;
  payload: unknown;
}

/** Body for `POST /tickets/:id/transition`; externally tagged, snake_case (see routes.rs). */
export type TransitionRequest =
  | { activate: { actor: ParticipantId } }
  | { close: { reason?: string | null; actor: ParticipantId } }
  | { cancel: { reason?: string | null; actor: ParticipantId } }
  | { reopen: { reason?: string | null; actor: ParticipantId } };
