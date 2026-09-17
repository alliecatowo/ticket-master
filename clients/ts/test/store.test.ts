import { describe, expect, it } from "vitest";
import { ProjectStore } from "../src/store.js";
import type { WireEvent } from "../src/domain.js";

function event<K extends WireEvent["kind"]>(
  seq: number,
  kind: K,
  payload: unknown,
  overrides: Partial<WireEvent> = {},
): WireEvent {
  return {
    seq,
    ts: `2026-01-01T00:00:${String(seq).padStart(2, "0")}Z`,
    kind,
    subject: "S",
    actor: "P-1",
    session: null,
    causation: null,
    correlation: null,
    payload,
    ...overrides,
  } as WireEvent;
}

describe("ProjectStore", () => {
  it("materializes a ticket from ticket.created with tm-core's defaults", () => {
    const events: WireEvent[] = [
      event(1, "ticket.created", { ticket: "T-1", title: "Do the thing", parent: null }),
    ];
    const state = ProjectStore.reduce(events);

    expect(state.head).toBe(1);
    expect(state.tickets).toHaveLength(1);
    const ticket = state.tickets[0]!;
    expect(ticket.id).toBe("T-1");
    expect(ticket.objective).toBe("Do the thing");
    expect(ticket.state).toBe("draft");
    expect(ticket.kind).toBe("work");
    expect(ticket.children).toEqual([]);
  });

  it("links a child ticket to its parent's children list", () => {
    const events: WireEvent[] = [
      event(1, "ticket.created", { ticket: "T-1", title: "Parent", parent: null }),
      event(2, "ticket.created", { ticket: "T-2", title: "Child", parent: "T-1" }),
    ];
    const state = ProjectStore.reduce(events);

    const parent = state.tickets.find((t) => t.id === "T-1")!;
    expect(parent.children).toEqual(["T-2"]);
    const child = state.tickets.find((t) => t.id === "T-2")!;
    expect(child.parent).toBe("T-1");
  });

  it("applies ticket.updated field-by-field, matching materialize::apply_ticket_updated", () => {
    const events: WireEvent[] = [
      event(1, "ticket.created", { ticket: "T-1", title: "Original", parent: null }),
      event(2, "ticket.updated", {
        ticket: "T-1",
        fields: { objective: "Revised", priority: 5, kind: "investigation" },
      }),
    ];
    const state = ProjectStore.reduce(events);
    const ticket = state.tickets[0]!;
    expect(ticket.objective).toBe("Revised");
    expect(ticket.priority).toBe(5);
    expect(ticket.kind).toBe("investigation");
  });

  it("moves ticket.state_changed's `to` into the materialized state", () => {
    const events: WireEvent[] = [
      event(1, "ticket.created", { ticket: "T-1", title: "T", parent: null }),
      event(2, "ticket.state_changed", { ticket: "T-1", from: "draft", to: "ready" }),
    ];
    const state = ProjectStore.reduce(events);
    expect(state.tickets[0]!.state).toBe("ready");
  });

  it("tracks dependency add/remove on the ticket's dependencies list", () => {
    const events: WireEvent[] = [
      event(1, "ticket.created", { ticket: "T-1", title: "A", parent: null }),
      event(2, "ticket.created", { ticket: "T-2", title: "B", parent: null }),
      event(3, "ticket.dependency_added", { ticket: "T-1", depends_on: "T-2" }),
    ];
    let state = ProjectStore.reduce(events);
    expect(state.tickets.find((t) => t.id === "T-1")!.dependencies).toEqual(["T-2"]);

    const removed = [...events, event(4, "ticket.dependency_removed", { ticket: "T-1", depends_on: "T-2" })];
    state = ProjectStore.reduce(removed);
    expect(state.tickets.find((t) => t.id === "T-1")!.dependencies).toEqual([]);
  });

  it("acquires, heartbeats, and releases a lease with the expected ttl bookkeeping", () => {
    const events: WireEvent[] = [
      event(1, "ticket.created", { ticket: "T-1", title: "A", parent: null }),
      event(2, "ticket.leased", {
        ticket: "T-1",
        lease: "L-1",
        holder: "P-1",
        expires_at: "2026-01-01T00:01:00Z",
      }),
    ];
    let state = ProjectStore.reduce(events);
    expect(state.leases).toHaveLength(1);
    expect(state.leases[0]!.ttl_seconds).toBe(58); // expires_at - ts(event 2, seq=2 -> :02)

    const heartbeat = [
      ...events,
      event(3, "ticket.heartbeat", { lease: "L-1", expires_at: "2026-01-01T00:02:00Z" }),
    ];
    state = ProjectStore.reduce(heartbeat);
    expect(state.leases[0]!.heartbeat).toBe("2026-01-01T00:00:03Z");

    const released = [...heartbeat, event(4, "ticket.lease_released", { ticket: "T-1", lease: "L-1" })];
    state = ProjectStore.reduce(released);
    expect(state.leases).toHaveLength(0);
  });

  it("records a decision.created payload's JSON-blob summary into structured fields", () => {
    const summary = JSON.stringify({
      subject: "Use SQLite",
      decision: "Yes",
      reason: "Simplicity",
      evidence: ["A-1"],
      affected_tickets: ["T-1"],
      affected_paths: ["src/**"],
    });
    const events: WireEvent[] = [event(1, "decision.created", { decision: "D-1", ticket: null, summary })];
    const state = ProjectStore.reduce(events);
    expect(state.decisions).toHaveLength(1);
    expect(state.decisions[0]).toMatchObject({
      id: "D-1",
      subject: "Use SQLite",
      decision: "Yes",
      reason: "Simplicity",
      evidence: ["A-1"],
      affected_tickets: ["T-1"],
      affected_paths: ["src/**"],
      author: "P-1",
      superseded_by: null,
    });
  });

  it("sets superseded_by on the old decision without touching its own fields", () => {
    const summary = JSON.stringify({ subject: "S", decision: "D", reason: "R" });
    const events: WireEvent[] = [
      event(1, "decision.created", { decision: "D-1", ticket: null, summary }),
      event(2, "decision.created", { decision: "D-2", ticket: null, summary }),
      event(3, "decision.superseded", { decision: "D-1", superseded_by: "D-2" }),
    ];
    const state = ProjectStore.reduce(events);
    const old = state.decisions.find((d) => d.id === "D-1")!;
    expect(old.superseded_by).toBe("D-2");
    expect(old.subject).toBe("S");
  });

  it("opens, closes, and reopens a milestone", () => {
    const events: WireEvent[] = [
      event(1, "milestone.created", { milestone: "M-1", title: "Beta" }),
      event(2, "milestone.closed", { milestone: "M-1" }),
    ];
    let state = ProjectStore.reduce(events);
    expect(state.milestones[0]!.state).toBe("closed");

    const reopened = [...events, event(3, "milestone.reopened", { milestone: "M-1" })];
    state = ProjectStore.reduce(reopened);
    expect(state.milestones[0]!.state).toBe("open");
  });

  it("attaches evidence via a ticket.updated `evidence_attached` field", () => {
    const events: WireEvent[] = [
      event(1, "ticket.created", { ticket: "T-1", title: "A", parent: null }),
      event(2, "ticket.updated", {
        ticket: "T-1",
        fields: { evidence_attached: { kind: "test_run", artifact: "A-1", summary: "all green" } },
      }),
    ];
    const state = ProjectStore.reduce(events);
    expect(state.evidence).toEqual([
      { ticket: "T-1", kind: "test_run", artifact: "A-1", produced_by: "P-1", ts: events[1]!.ts, summary: "all green" },
    ]);
  });

  it("tracks participant status through session and presence events", () => {
    const events: WireEvent[] = [
      event(1, "session.started", { session: "SE-1", participant: "P-1" }),
      event(2, "presence.updated", { participant: "P-1", status: "editing" }),
      event(3, "session.left", { session: "SE-1", participant: "P-1" }),
    ];
    const state = ProjectStore.reduce(events);
    expect(state.participants["P-1"]).toBe("idle");
  });

  it("ignores unmaterialized event kinds (e.g. usage.recorded) as a no-op, matching tm-core", () => {
    const events: WireEvent[] = [
      event(1, "ticket.created", { ticket: "T-1", title: "A", parent: null }),
      event(2, "usage.recorded", { ticket: "T-1", session: null, tokens: 10, dollars_micros: 1, wall_seconds: 1 }),
    ];
    const state = ProjectStore.reduce(events);
    expect(state.head).toBe(2);
    expect(state.tickets).toHaveLength(1);
  });

  it("apply() mutates the same store incrementally, matching reduce()'s batch result", () => {
    const events: WireEvent[] = [
      event(1, "ticket.created", { ticket: "T-1", title: "A", parent: null }),
      event(2, "ticket.state_changed", { ticket: "T-1", from: "draft", to: "ready" }),
    ];
    const store = new ProjectStore();
    for (const e of events) store.apply(e);
    expect(store.getState()).toEqual(ProjectStore.reduce(events));
  });
});
