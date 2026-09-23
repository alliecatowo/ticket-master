import type {
  Evidence,
  StateSnapshot,
  Ticket,
  TicketId,
  TicketState,
  WireEvent,
} from "./types";

/** A lease as the client tracks it: who holds a ticket, and until when (epoch millis). */
export interface LiveLease {
  id: string;
  ticket: TicketId;
  holder: string;
  expiresAt: number;
}

/**
 * What the event log says about one ticket beyond its projection. Mirrors `TicketActivity` in
 * `crates/tm-cli/src/tickets/overview.rs`, which the TUI's tickets screen and `tm tickets --json`
 * use, so the web rows say the same thing.
 */
export interface TicketActivity {
  lastCommand?: { text: string; at: string };
  lastStep?: { text: string; at: string };
  submission?: string;
  submittedAt?: string;
  escalation?: { reason: string; at: string };
  cancelReason?: string;
  pendingApproval?: { note: string; at: string };
  retryAt?: string;
}

export interface ProjectStoreSnapshot {
  /** The last event applied to the materialized tickets. */
  head: number;
  tickets: Map<TicketId, Ticket>;
  leases: Map<string, LiveLease>;
  evidence: Evidence[];
  activity: Map<TicketId, TicketActivity>;
  /** Every event about each ticket, oldest first (capped per ticket). */
  timelines: Map<TicketId, WireEvent[]>;
  /** Events seen since the client first connected, most recent last — the Status view's input. */
  recentEvents: WireEvent[];
}

const RECENT_EVENTS_LIMIT = 200;
const TIMELINE_LIMIT = 400;
const TICKET_ID = /^[TVA]-\d+$/;

/** The ticket an event is about: its payload's `ticket` when present, else its subject. */
export function ticketOf(event: WireEvent): TicketId | undefined {
  const payload = event.payload as Record<string, unknown> | null;
  const fromPayload = payload && typeof payload.ticket === "string" ? payload.ticket : undefined;
  const id = fromPayload ?? event.subject;
  return TICKET_ID.test(id) ? id : undefined;
}

/**
 * `ProjectStore` — the client's materialized view (SPEC.md §18.1). Pure and unit-testable: no
 * fetch, no timers, no DOM.
 *
 * Two cursors, because the client reads the log two ways:
 * - `head`: tickets, leases and evidence come from `GET /state` (`seed`) and are then patched by
 *   every event after that snapshot's head. An event at or below `head` is already in the
 *   snapshot and is not applied again.
 * - `historySeq`: activity (submission summaries, escalation reasons, latest command) and the
 *   per-ticket timeline exist only in the event log, so the client replays it from the start
 *   (`GET /events?from=0`) and folds every event once. Re-seeding on a reconnect leaves these
 *   alone; the stream resumes from `historySeq`.
 */
export class ProjectStore {
  private head = 0;
  private historySeq = 0;
  /** The head at the first seed: events after it are "since you connected" for Status. */
  private liveSince: number | null = null;
  private readonly tickets = new Map<TicketId, Ticket>();
  private readonly leases = new Map<string, LiveLease>();
  private evidence: Evidence[] = [];
  private readonly activity = new Map<TicketId, TicketActivity>();
  private readonly timelines = new Map<TicketId, WireEvent[]>();
  private readonly recentEvents: WireEvent[] = [];

  /**
   * Replace the materialized tickets, leases and evidence with a `GET /state` snapshot. Returns
   * `"replay"` when the snapshot is behind what this store has already read — the server's log
   * is not the one it was reading (a different project, or a reset) — in which case history is
   * cleared and the caller must stream again from 0.
   */
  seed(snapshot: StateSnapshot): "resume" | "replay" {
    let outcome: "resume" | "replay" = "resume";
    if (snapshot.head < this.historySeq) {
      this.historySeq = 0;
      this.activity.clear();
      this.timelines.clear();
      this.recentEvents.length = 0;
      this.liveSince = null;
      outcome = "replay";
    }
    this.head = snapshot.head;
    if (this.liveSince === null) this.liveSince = snapshot.head;
    this.tickets.clear();
    for (const ticket of snapshot.tickets) this.tickets.set(ticket.id, ticket);
    this.leases.clear();
    for (const lease of snapshot.leases) {
      const heartbeat = Date.parse(lease.heartbeat);
      this.leases.set(lease.id, {
        id: lease.id,
        ticket: lease.ticket,
        holder: lease.holder,
        expiresAt: heartbeat + lease.ttl_seconds * 1000,
      });
    }
    this.evidence = [...(snapshot.evidence ?? [])];
    return outcome;
  }

  /** Where the event stream should resume: after the last event folded into history. */
  cursor(): number {
    return this.historySeq;
  }

  snapshot(): ProjectStoreSnapshot {
    return {
      head: this.head,
      tickets: new Map(this.tickets),
      leases: new Map(this.leases),
      evidence: [...this.evidence],
      activity: new Map(this.activity),
      timelines: new Map(this.timelines),
      recentEvents: [...this.recentEvents],
    };
  }

  /** Apply one event from the stream. Returns `true` if anything changed. */
  apply(event: WireEvent): boolean {
    let changed = false;
    if (event.seq > this.historySeq) {
      this.historySeq = event.seq;
      this.foldHistory(event);
      changed = true;
    }
    if (event.seq > this.head) {
      this.head = event.seq;
      this.applyToProjection(event);
      changed = true;
    }
    return changed;
  }

  private foldHistory(event: WireEvent): void {
    if (this.liveSince !== null && event.seq > this.liveSince) {
      this.recentEvents.push(event);
      if (this.recentEvents.length > RECENT_EVENTS_LIMIT) this.recentEvents.shift();
    }
    const ticket = ticketOf(event);
    if (!ticket) return;

    const timeline = [...(this.timelines.get(ticket) ?? []), event];
    if (timeline.length > TIMELINE_LIMIT) timeline.splice(0, timeline.length - TIMELINE_LIMIT);
    this.timelines.set(ticket, timeline);

    const text = (field: string) => {
      const payload = event.payload as Record<string, unknown> | null;
      const value = payload?.[field];
      return typeof value === "string" ? value : undefined;
    };
    const prev = this.activity.get(ticket) ?? {};
    const next: TicketActivity = { ...prev };
    switch (event.kind) {
      case "command.started": {
        const command = text("command");
        if (command !== undefined) next.lastCommand = { text: command, at: event.ts };
        break;
      }
      case "goal.step_added": {
        const step = text("text");
        if (step !== undefined) next.lastStep = { text: step, at: event.ts };
        break;
      }
      case "ticket.submitted":
        next.submission = text("summary");
        next.submittedAt = event.ts;
        break;
      case "ticket.escalated":
        next.escalation = { reason: text("reason") ?? "", at: event.ts };
        break;
      case "ticket.cancelled":
        next.cancelReason = text("reason");
        break;
      case "ticket.retry_scheduled":
        next.retryAt = text("not_before");
        break;
      case "approval.requested":
        next.pendingApproval = { note: text("note") ?? "", at: event.ts };
        break;
      // A decision answers the request; a new lease is a new attempt, and an approval the
      // previous one left unanswered is not what this one is waiting on.
      case "approval.decided":
      case "ticket.leased":
        delete next.pendingApproval;
        break;
      default:
        return;
    }
    this.activity.set(ticket, next);
  }

  private applyToProjection(event: WireEvent): void {
    const payload = (event.payload ?? {}) as Record<string, unknown>;
    const ticketId = ticketOf(event);
    switch (event.kind) {
      case "ticket.created": {
        if (!ticketId || this.tickets.has(ticketId)) break;
        // Partial until the `ticket.updated` that always follows fills in the rest.
        this.tickets.set(
          ticketId,
          makePartialTicket(ticketId, String(payload.title ?? ""), event.ts, payload.parent),
        );
        break;
      }
      case "ticket.updated": {
        if (!ticketId) break;
        const fields = { ...((payload.fields ?? {}) as Record<string, unknown>) };
        const attached = fields.evidence_attached as
          | { artifact?: string; kind?: string; summary?: string }
          | undefined;
        delete fields.evidence_attached;
        if (attached && typeof attached.artifact === "string") {
          this.evidence = [
            ...this.evidence,
            {
              ticket: ticketId,
              kind: attached.kind ?? "review",
              artifact: attached.artifact,
              produced_by: event.actor,
              ts: event.ts,
              summary: attached.summary ?? "",
            },
          ];
        }
        if (Object.keys(fields).length > 0) {
          this.patchTicket(ticketId, { ...(fields as Partial<Ticket>), updated: event.ts });
        }
        break;
      }
      case "ticket.state_changed":
        this.patchTicket(ticketId, { state: payload.to as TicketState, updated: event.ts });
        break;
      case "ticket.closed":
        this.patchTicket(ticketId, { state: "closed", updated: event.ts });
        break;
      case "ticket.cancelled":
        this.patchTicket(ticketId, { state: "cancelled", updated: event.ts });
        break;
      case "ticket.escalated":
        this.patchTicket(ticketId, { state: "escalated", updated: event.ts });
        break;
      case "ticket.leased":
      case "ticket.heartbeat": {
        const lease = typeof payload.lease === "string" ? payload.lease : undefined;
        if (!lease || !ticketId) break;
        const existing = this.leases.get(lease);
        this.leases.set(lease, {
          id: lease,
          ticket: ticketId,
          holder:
            typeof payload.holder === "string" ? payload.holder : (existing?.holder ?? event.actor),
          expiresAt: Date.parse(String(payload.expires_at ?? event.ts)),
        });
        break;
      }
      case "ticket.lease_released":
      case "ticket.lease_expired":
        if (typeof payload.lease === "string") this.leases.delete(payload.lease);
        break;
      default:
        break;
    }
  }

  private patchTicket(id: TicketId | undefined, patch: Partial<Ticket>): void {
    if (!id) return;
    const existing = this.tickets.get(id);
    if (!existing) return;
    this.tickets.set(id, { ...existing, ...patch });
  }
}

function makePartialTicket(id: TicketId, title: string, ts: string, parent: unknown): Ticket {
  return {
    id,
    kind: "work",
    objective: title,
    state: "draft",
    parent: typeof parent === "string" ? parent : null,
    children: [],
    dependencies: [],
    milestone: null,
    authority: null,
    resources: [],
    executor: {},
    context_refs: [],
    success: [],
    verification: null,
    budget: null,
    retry: null,
    cycle: null,
    attempts: 0,
    failures: [],
    priority: 0,
    created: ts,
    updated: ts,
  };
}
