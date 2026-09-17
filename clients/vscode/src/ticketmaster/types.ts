/**
 * Domain types mirroring the Rust core described in SPEC.md sections 2-4 and
 * 14 (tm-types, tm-core, tm-server). `clients/ts/` — the generated shared
 * client package described in SPEC.md 18.1 — does not exist in this
 * repository yet, so these types are hand-written here as a minimal,
 * explicitly-temporary substitute. When `@ticketmaster/client` lands, this
 * file (and ./client.ts) should be deleted and callers should import the
 * generated types instead. See README.md for details.
 */

/** SPEC.md 4.3 */
export type TicketState =
  | "Draft"
  | "Blocked"
  | "Ready"
  | "Leased"
  | "Running"
  | "Submitted"
  | "Verifying"
  | "Auditing"
  | "Rework"
  | "Replan"
  | "Recovery"
  | "Escalated"
  | "Closed"
  | "Cancelled";

export type TicketKind =
  | "Work"
  | "Verification"
  | "Audit"
  | "Investigation"
  | "Recovery"
  | "Harness";

/** SPEC.md 4.2, trimmed to the fields the editor surface needs. */
export interface Ticket {
  id: string; // T-<n>
  kind: TicketKind;
  objective: string;
  state: TicketState;
  parent: string | null;
  children: string[];
  dependencies: string[];
  milestone: string | null; // M-<n>
  priority: number;
  created: string;
  updated: string;
}

/** SPEC.md 4.6 */
export interface Milestone {
  id: string; // M-<n>
  title: string;
  tickets: string[];
  state: "Open" | "Closed";
  closedBy: string | null;
  assumptions: string[]; // Decision ids
}

/** SPEC.md 4.6 */
export interface Decision {
  id: string; // D-<n>
  subject: string;
  decision: string;
  reason: string;
  evidence: string[]; // Artifact ids
  affectedTickets: string[];
  affectedDocs: string[]; // file paths / globs, per SPEC.md 2.4 PathPattern
  author: string;
  ts: string;
  supersedes: string | null;
  supersededBy: string | null;
}

/** SPEC.md 2.4 — a glob-based path pattern. */
export type PathPattern = string;

/** SPEC.md 4.5 (`ResourceClaim` shape is not fully specified; this is the
 * minimal read shape the editor surface consumes). */
export interface ResourceClaim {
  patterns: PathPattern[];
  exclusive: boolean;
}

/** SPEC.md 4.5 */
export interface Lease {
  id: string; // L-<hex12>
  ticket: string; // T-<n>
  holder: string; // participant id
  resources: ResourceClaim[];
  acquired: string;
  heartbeat: string;
  ttlSeconds: number;
  epoch: number;
}

/** GET /state per SPEC.md 14 */
export interface ProjectSnapshot {
  headSeq: number;
  tickets: Ticket[];
  milestones: Milestone[];
  decisions: Decision[];
  leases: Lease[];
}
