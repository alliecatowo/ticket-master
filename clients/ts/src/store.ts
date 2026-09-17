// `ProjectStore`: a pure, in-memory materialized view fed by the event stream, reduced the same
// way `tm-core::materialize::apply` reduces the log into SQLite (SPEC.md §18.1, §4). A surface
// built on this client renders from the same event-sourced model Core uses, not a second guess
// at it.
//
// Every reducer arm below is a direct transcription of the corresponding arm in
// `crates/tm-core/src/materialize.rs`, including its placeholder defaults (`TicketDefaults`)
// and its documented gaps: a few event kinds carry only a subset of the fields their target
// table has room for (e.g. `artifact.created`'s payload has no `bytes_len`/`hash` — `tm-core`
// itself patches those in with a direct SQL write outside the event log, so a pure event-log
// replay, here or in `tm-core`'s own `replay`, cannot recover them either). Where `tm-core`
// documents a field as unrecoverable from the log, this module leaves it at the same
// placeholder `tm-core` does, rather than inventing a value the log doesn't carry.

import type {
  Artifact,
  ArtifactId,
  Decision,
  DecisionId,
  Evidence,
  Lease,
  LeaseId,
  Milestone,
  MilestoneId,
  ParticipantId,
  Ticket,
  TicketId,
  WireEvent,
} from "./domain.js";

function ticketDefaults(id: TicketId, title: string, parent: TicketId | null, ts: string): Ticket {
  return {
    id,
    kind: "work",
    objective: title,
    state: "draft",
    parent,
    children: [],
    dependencies: [],
    milestone: null,
    authority: null,
    resources: [],
    executor: { role: "coder_fast", human_required: false, min_capability: "any" },
    context_refs: [],
    success: [],
    verification: "none",
    budget: null,
    retry: { max_attempts: 1, base_delay_seconds: 0, backoff_multiplier: 1, max_delay_seconds: 0 },
    cycle: null,
    attempts: 0,
    failures: [],
    priority: 0,
    created: ts,
    updated: ts,
  };
}

function secondsBetween(laterIso: string, earlierIso: string): number {
  const diffMs = Date.parse(laterIso) - Date.parse(earlierIso);
  return Math.max(0, Math.round(diffMs / 1000));
}

/** A snapshot of everything {@link ProjectStore} materializes, mirroring `GET /state`'s shape. */
export interface ProjectStoreState {
  head: number;
  tickets: Ticket[];
  leases: Lease[];
  decisions: Decision[];
  milestones: Milestone[];
  artifacts: Artifact[];
  evidence: Evidence[];
  participants: Record<string, string>;
}

export class ProjectStore {
  private head = 0;
  private readonly tickets = new Map<TicketId, Ticket>();
  private readonly leases = new Map<LeaseId, Lease>();
  private readonly decisions = new Map<DecisionId, Decision>();
  private readonly milestones = new Map<MilestoneId, Milestone>();
  private readonly artifacts = new Map<ArtifactId, Artifact>();
  private readonly evidence: Evidence[] = [];
  private readonly participants = new Map<ParticipantId, string>();

  /** Build a store from a scripted event array in one call (test convenience / cold start). */
  static reduce(events: readonly WireEvent[]): ProjectStoreState {
    const store = new ProjectStore();
    for (const event of events) store.apply(event);
    return store.getState();
  }

  /** Apply one event, mutating this store's materialized tables in place. */
  apply(event: WireEvent): void {
    this.head = Math.max(this.head, event.seq);
    const p = event.payload as Record<string, unknown>;

    switch (event.kind) {
      case "ticket.created": {
        const ticket = p as { ticket: TicketId; title: string; parent: TicketId | null };
        const existing = this.tickets.get(ticket.ticket);
        if (existing) {
          existing.objective = ticket.title;
          existing.parent = ticket.parent;
        } else {
          this.tickets.set(
            ticket.ticket,
            ticketDefaults(ticket.ticket, ticket.title, ticket.parent, event.ts),
          );
        }
        if (ticket.parent) this.addChild(ticket.parent, ticket.ticket);
        break;
      }
      case "ticket.updated": {
        const body = p as { ticket: TicketId; fields: Record<string, unknown> };
        this.applyTicketUpdated(event, body.ticket, body.fields);
        break;
      }
      case "ticket.state_changed": {
        const body = p as { ticket: TicketId; to: string };
        const ticket = this.tickets.get(body.ticket);
        if (ticket) ticket.state = body.to as Ticket["state"];
        break;
      }
      case "ticket.dependency_added": {
        const body = p as { ticket: TicketId; depends_on: TicketId };
        const ticket = this.tickets.get(body.ticket);
        if (ticket && !ticket.dependencies.includes(body.depends_on)) {
          ticket.dependencies.push(body.depends_on);
        }
        break;
      }
      case "ticket.dependency_removed": {
        const body = p as { ticket: TicketId; depends_on: TicketId };
        const ticket = this.tickets.get(body.ticket);
        if (ticket) {
          ticket.dependencies = ticket.dependencies.filter((d) => d !== body.depends_on);
        }
        break;
      }
      case "ticket.child_added": {
        const body = p as { parent: TicketId; child: TicketId };
        this.addChild(body.parent, body.child);
        break;
      }
      case "ticket.delegated": {
        const body = p as { parent: TicketId; child: TicketId };
        this.addChild(body.parent, body.child);
        break;
      }
      case "ticket.leased": {
        const body = p as { ticket: TicketId; lease: LeaseId; holder: ParticipantId; expires_at: string };
        this.leases.set(body.lease, {
          id: body.lease,
          ticket: body.ticket,
          holder: body.holder,
          authority: null,
          resources: [],
          acquired: event.ts,
          heartbeat: event.ts,
          ttl_seconds: secondsBetween(body.expires_at, event.ts),
          epoch: 0,
        });
        break;
      }
      case "ticket.heartbeat": {
        const body = p as { lease: LeaseId; expires_at: string };
        const lease = this.leases.get(body.lease);
        if (lease) {
          lease.heartbeat = event.ts;
          lease.ttl_seconds = secondsBetween(body.expires_at, event.ts);
        }
        break;
      }
      case "ticket.lease_expired":
      case "ticket.lease_released": {
        const body = p as { lease: LeaseId };
        this.leases.delete(body.lease);
        break;
      }
      case "decision.created": {
        const body = p as { decision: DecisionId; summary: string };
        const parsed = JSON.parse(body.summary) as {
          subject?: string;
          decision?: string;
          reason?: string;
          evidence?: ArtifactId[];
          affected_tickets?: TicketId[];
          affected_paths?: string[];
        };
        this.decisions.set(body.decision, {
          id: body.decision,
          subject: parsed.subject ?? "",
          decision: parsed.decision ?? "",
          reason: parsed.reason ?? "",
          evidence: parsed.evidence ?? [],
          affected_tickets: parsed.affected_tickets ?? [],
          affected_paths: parsed.affected_paths ?? [],
          author: event.actor,
          ts: event.ts,
          supersedes: null,
          superseded_by: null,
        });
        break;
      }
      case "decision.superseded": {
        const body = p as { decision: DecisionId; superseded_by: DecisionId };
        const decision = this.decisions.get(body.decision);
        if (decision) decision.superseded_by = body.superseded_by;
        break;
      }
      case "artifact.created": {
        // Placeholder fields only: `bytes_len`/`hash`/real `kind`/`meta` are not carried by this
        // event's payload and (per `tm-core::materialize`'s doc comment) are only ever written
        // by a direct SQL call alongside it, not recoverable from the log alone.
        const body = p as { artifact: ArtifactId; path: string; media_type: string };
        this.artifacts.set(body.artifact, {
          id: body.artifact,
          kind: "file",
          media_type: body.media_type,
          bytes_len: 0,
          hash: "",
          storage: { kind: "disk", path: body.path },
          meta: {},
        });
        break;
      }
      case "milestone.created": {
        const body = p as { milestone: MilestoneId; title: string };
        this.milestones.set(body.milestone, {
          id: body.milestone,
          title: body.title,
          tickets: [],
          state: "open",
          closed_by: null,
          assumptions: [],
        });
        break;
      }
      case "milestone.closed": {
        const body = p as { milestone: MilestoneId };
        const milestone = this.milestones.get(body.milestone);
        if (milestone) milestone.state = "closed";
        break;
      }
      case "milestone.reopened": {
        const body = p as { milestone: MilestoneId };
        const milestone = this.milestones.get(body.milestone);
        if (milestone) milestone.state = "open";
        break;
      }
      case "session.started": {
        const body = p as { participant: ParticipantId };
        this.participants.set(body.participant, "active");
        break;
      }
      case "session.joined": {
        const body = p as { participant: ParticipantId };
        this.participants.set(body.participant, "active");
        break;
      }
      case "session.left": {
        const body = p as { participant: ParticipantId };
        this.participants.set(body.participant, "idle");
        break;
      }
      case "presence.updated": {
        const body = p as { participant: ParticipantId; status: string };
        this.participants.set(body.participant, body.status);
        break;
      }
      default:
        // Every other catalogued kind either carries no materialized-table effect in `tm-core`
        // (ticket.submitted/verified/audited/closed/cancelled/reopened/failed/... — the
        // accompanying `ticket.state_changed` in the same transaction already moved `state`) or
        // names fields with no home in this store's tables yet, matching
        // `crates/tm-core/src/materialize.rs`'s own closing `_ => {}` arm exactly.
        break;
    }
  }

  private addChild(parent: TicketId, child: TicketId): void {
    const parentTicket = this.tickets.get(parent);
    if (parentTicket && !parentTicket.children.includes(child)) {
      parentTicket.children.push(child);
    }
  }

  private applyTicketUpdated(event: WireEvent, ticketId: TicketId, fields: Record<string, unknown>): void {
    const ticket = this.tickets.get(ticketId);
    for (const [key, value] of Object.entries(fields)) {
      switch (key) {
        case "evidence_attached": {
          const ev = value as { kind?: Evidence["kind"]; artifact?: ArtifactId; summary?: string };
          if (ev && ev.artifact) {
            this.evidence.push({
              ticket: ticketId,
              kind: (ev.kind ?? "review") as Evidence["kind"],
              artifact: ev.artifact,
              produced_by: event.actor,
              ts: event.ts,
              summary: ev.summary ?? "",
            });
          }
          break;
        }
        case "objective":
          if (ticket && typeof value === "string") ticket.objective = value;
          break;
        case "milestone":
          if (ticket) ticket.milestone = (value as MilestoneId | null) ?? null;
          break;
        case "created":
          if (ticket && typeof value === "string") ticket.created = value;
          break;
        case "updated":
          if (ticket && typeof value === "string") ticket.updated = value;
          break;
        case "priority":
          if (ticket && typeof value === "number") ticket.priority = value;
          break;
        case "attempts":
          if (ticket && typeof value === "number") ticket.attempts = value;
          break;
        case "kind":
          if (ticket) ticket.kind = value as Ticket["kind"];
          break;
        case "authority":
          if (ticket) ticket.authority = value;
          break;
        case "resources":
          if (ticket) ticket.resources = value as Ticket["resources"];
          break;
        case "executor":
          if (ticket) ticket.executor = value as Ticket["executor"];
          break;
        case "context_refs":
          if (ticket) ticket.context_refs = value as Ticket["context_refs"];
          break;
        case "success":
          if (ticket) ticket.success = value as Ticket["success"];
          break;
        case "verification":
          if (ticket) ticket.verification = value as Ticket["verification"];
          break;
        case "budget":
          if (ticket) ticket.budget = value;
          break;
        case "retry":
          if (ticket) ticket.retry = value as Ticket["retry"];
          break;
        case "cycle":
          if (ticket) ticket.cycle = value as Ticket["cycle"];
          break;
        case "failures":
          if (ticket) ticket.failures = value as Ticket["failures"];
          break;
        default:
          break;
      }
    }
  }

  /** An immutable-ish snapshot; callers get plain arrays/objects, not live references. */
  getState(): ProjectStoreState {
    return {
      head: this.head,
      tickets: [...this.tickets.values()].map((t) => ({ ...t })),
      leases: [...this.leases.values()].map((l) => ({ ...l })),
      decisions: [...this.decisions.values()].map((d) => ({ ...d })),
      milestones: [...this.milestones.values()].map((m) => ({ ...m })),
      artifacts: [...this.artifacts.values()].map((a) => ({ ...a })),
      evidence: this.evidence.map((e) => ({ ...e })),
      participants: Object.fromEntries(this.participants),
    };
  }

  getTicket(id: TicketId): Ticket | undefined {
    const ticket = this.tickets.get(id);
    return ticket ? { ...ticket } : undefined;
  }
}
