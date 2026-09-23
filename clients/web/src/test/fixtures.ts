// Shared test fixtures: wire-shaped tickets, events and snapshots, as `tm-server` sends them.

import type { StateSnapshot, Ticket, WireEvent } from "../api/types";

export function ticket(overrides: Partial<Ticket> = {}): Ticket {
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

export function event(overrides: Partial<WireEvent>): WireEvent {
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

export function stateSnapshot(overrides: Partial<StateSnapshot> = {}): StateSnapshot {
  return {
    head: 0,
    tickets: [ticket()],
    leases: [],
    decisions: [],
    milestones: [],
    artifacts: [],
    evidence: [],
    budgets: [],
    ...overrides,
  };
}
