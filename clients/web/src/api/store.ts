import type { StateSnapshot, Ticket, TicketId, TicketState, WireEvent } from "./types";

export interface ProjectStoreSnapshot {
  head: number;
  tickets: Map<TicketId, Ticket>;
  /** Event kinds seen since the store was seeded, most recent last — used by the Status view. */
  recentEvents: WireEvent[];
}

const RECENT_EVENTS_LIMIT = 200;

/**
 * `ProjectStore` — an in-memory materialized view fed by the event stream (SPEC.md §18.1).
 *
 * Pure and unit-testable with a scripted event array: no fetch, no timers, no DOM. Seed it from
 * `GET /state` (a full snapshot) and then feed it every event from `subscribe()` in order;
 * `apply` is idempotent-by-`seq` (an event with `seq <= head` is ignored) so a reconnect that
 * re-delivers the tail of the backlog does not double-apply.
 *
 * Honest limitation (documented in ../../README.md): `ticket.created` events carry only
 * `{ticket, title, parent}`, not the full `Ticket` shape `tm-core` materializes server-side, so
 * a ticket created *after* this store's initial seed appears with a minimal partial record until
 * the next full re-fetch of `GET /state`. Every other tracked transition (state changes, leases,
 * closes) updates the existing record's known fields in place.
 */
export class ProjectStore {
  private head = 0;
  private readonly tickets = new Map<TicketId, Ticket>();
  private readonly recentEvents: WireEvent[] = [];

  seed(snapshot: StateSnapshot): void {
    this.head = snapshot.head;
    this.tickets.clear();
    for (const ticket of snapshot.tickets) this.tickets.set(ticket.id, ticket);
    this.recentEvents.length = 0;
  }

  snapshot(): ProjectStoreSnapshot {
    return {
      head: this.head,
      tickets: new Map(this.tickets),
      recentEvents: [...this.recentEvents],
    };
  }

  /** Apply one event from `subscribe()`. Returns `true` if it advanced the store's state. */
  apply(event: WireEvent): boolean {
    if (event.seq <= this.head) return false;
    this.head = event.seq;

    this.recentEvents.push(event);
    if (this.recentEvents.length > RECENT_EVENTS_LIMIT) this.recentEvents.shift();

    const payload = event.payload as Record<string, unknown> | null;
    const ticketId = payload && typeof payload.ticket === "string" ? payload.ticket : undefined;

    switch (event.kind) {
      case "ticket.created": {
        if (!ticketId || this.tickets.has(ticketId)) break;
        // Partial record — see the class doc. Filled in fully on the next `seed()`.
        this.tickets.set(ticketId, makePartialTicket(ticketId, String(payload?.title ?? "")));
        break;
      }
      case "ticket.state_changed": {
        if (!ticketId) break;
        const to = payload?.to;
        this.patchTicket(ticketId, { state: to as TicketState, updated: event.ts });
        break;
      }
      case "ticket.closed":
        this.patchTicket(ticketId, { state: "closed", updated: event.ts });
        break;
      case "ticket.cancelled":
        this.patchTicket(ticketId, { state: "cancelled", updated: event.ts });
        break;
      case "ticket.escalated":
        this.patchTicket(ticketId, { state: "escalated", updated: event.ts });
        break;
      default:
        break;
    }
    return true;
  }

  private patchTicket(id: TicketId | undefined, patch: Partial<Ticket>): void {
    if (!id) return;
    const existing = this.tickets.get(id);
    if (!existing) return;
    this.tickets.set(id, { ...existing, ...patch });
  }
}

function makePartialTicket(id: TicketId, title: string): Ticket {
  return {
    id,
    kind: "work",
    objective: title,
    state: "draft",
    parent: null,
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
    created: "",
    updated: "",
  };
}
