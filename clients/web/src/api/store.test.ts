import { describe, expect, it } from "vitest";
import { ProjectStore } from "./store";
import type { StateSnapshot, Ticket, WireEvent } from "./types";

function ticket(overrides: Partial<Ticket> = {}): Ticket {
  return {
    id: "T-1",
    kind: "work",
    objective: "seed a store",
    state: "ready",
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
    created: "2026-01-01T00:00:00Z",
    updated: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function event(overrides: Partial<WireEvent>): WireEvent {
  return {
    seq: 1,
    ts: "2026-01-01T00:00:01Z",
    kind: "ticket.state_changed",
    subject: "T-1",
    actor: "human:allie",
    session: null,
    causation: null,
    correlation: null,
    payload: { ticket: "T-1", from: "ready", to: "leased" },
    ...overrides,
  };
}

const snapshot: StateSnapshot = {
  head: 0,
  tickets: [ticket()],
  leases: [],
  decisions: [],
  milestones: [],
  artifacts: [],
  evidence: [],
  budgets: [],
};

describe("ProjectStore", () => {
  it("seeds tickets from a GET /state snapshot", () => {
    const store = new ProjectStore();
    store.seed(snapshot);
    const view = store.snapshot();
    expect(view.head).toBe(0);
    expect(view.tickets.get("T-1")?.state).toBe("ready");
  });

  it("applies a ticket.state_changed event to an existing ticket", () => {
    const store = new ProjectStore();
    store.seed(snapshot);
    const applied = store.apply(event({ seq: 1 }));
    expect(applied).toBe(true);
    expect(store.snapshot().tickets.get("T-1")?.state).toBe("leased");
    expect(store.snapshot().head).toBe(1);
  });

  it("ignores an event whose seq is not newer than head (reconnect overlap)", () => {
    const store = new ProjectStore();
    store.seed({ ...snapshot, head: 5 });
    const applied = store.apply(event({ seq: 3 }));
    expect(applied).toBe(false);
    expect(store.snapshot().tickets.get("T-1")?.state).toBe("ready");
  });

  it("applies events strictly in increasing seq order and keeps head monotonic", () => {
    const store = new ProjectStore();
    store.seed(snapshot);
    store.apply(event({ seq: 1, payload: { ticket: "T-1", from: "ready", to: "leased" } }));
    store.apply(
      event({
        seq: 2,
        kind: "ticket.closed",
        payload: { ticket: "T-1", reason: "done" },
      }),
    );
    const view = store.snapshot();
    expect(view.head).toBe(2);
    expect(view.tickets.get("T-1")?.state).toBe("closed");
    expect(view.recentEvents).toHaveLength(2);
  });

  it("adds a minimal partial ticket on ticket.created for an unseen id", () => {
    const store = new ProjectStore();
    store.seed({ ...snapshot, tickets: [] });
    store.apply(
      event({
        seq: 1,
        kind: "ticket.created",
        subject: "T-2",
        payload: { ticket: "T-2", title: "new work", parent: null },
      }),
    );
    const created = store.snapshot().tickets.get("T-2");
    expect(created?.objective).toBe("new work");
    expect(created?.state).toBe("draft");
  });

  it("marks a ticket escalated on ticket.escalated", () => {
    const store = new ProjectStore();
    store.seed(snapshot);
    store.apply(
      event({ seq: 1, kind: "ticket.escalated", payload: { ticket: "T-1", reason: "stuck" } }),
    );
    expect(store.snapshot().tickets.get("T-1")?.state).toBe("escalated");
  });
});
