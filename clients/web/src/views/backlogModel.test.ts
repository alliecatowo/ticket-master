import { describe, expect, it } from "vitest";
import { applyBacklogFilters, backlogGroupFor, groupBacklog } from "./backlogModel";
import type { Ticket } from "../api/types";

function ticket(overrides: Partial<Ticket>): Ticket {
  return {
    id: "T-1",
    kind: "work",
    objective: "do a thing",
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

describe("backlogGroupFor", () => {
  it("groups ready and blocked directly by state", () => {
    expect(backlogGroupFor(ticket({ state: "ready" }))).toBe("ready");
    expect(backlogGroupFor(ticket({ state: "blocked" }))).toBe("blocked");
  });

  it("groups running/leased/escalated etc. as active", () => {
    expect(backlogGroupFor(ticket({ state: "running" }))).toBe("active");
    expect(backlogGroupFor(ticket({ state: "escalated" }))).toBe("active");
  });

  it("excludes terminal states from any backlog group", () => {
    expect(backlogGroupFor(ticket({ state: "closed" }))).toBeNull();
    expect(backlogGroupFor(ticket({ state: "cancelled" }))).toBeNull();
    expect(backlogGroupFor(ticket({ state: "draft" }))).toBeNull();
  });
});

describe("applyBacklogFilters", () => {
  const tickets = [
    ticket({ id: "T-1", milestone: "M-1", kind: "work", executor: { role: "backend" } }),
    ticket({ id: "T-2", milestone: "M-2", kind: "verification", executor: { role: "qa" } }),
    ticket({ id: "T-3", milestone: "M-1", kind: "work", executor: {} }),
  ];

  it("filters by milestone", () => {
    const result = applyBacklogFilters(tickets, {
      milestone: "M-1",
      kind: null,
      executorRole: null,
    });
    expect(result.map((t) => t.id)).toEqual(["T-1", "T-3"]);
  });

  it("filters by kind", () => {
    const result = applyBacklogFilters(tickets, {
      milestone: null,
      kind: "verification",
      executorRole: null,
    });
    expect(result.map((t) => t.id)).toEqual(["T-2"]);
  });

  it("filters by executor role, excluding tickets with no role set", () => {
    const result = applyBacklogFilters(tickets, {
      milestone: null,
      kind: null,
      executorRole: "backend",
    });
    expect(result.map((t) => t.id)).toEqual(["T-1"]);
  });

  it("combines filters with AND semantics", () => {
    const result = applyBacklogFilters(tickets, {
      milestone: "M-1",
      kind: "work",
      executorRole: "backend",
    });
    expect(result.map((t) => t.id)).toEqual(["T-1"]);
  });

  it("returns everything when no filters are set", () => {
    const result = applyBacklogFilters(tickets, {
      milestone: null,
      kind: null,
      executorRole: null,
    });
    expect(result).toHaveLength(3);
  });
});

describe("groupBacklog", () => {
  it("buckets a mixed ticket list into ready/blocked/active", () => {
    const groups = groupBacklog([
      ticket({ id: "T-1", state: "ready" }),
      ticket({ id: "T-2", state: "blocked" }),
      ticket({ id: "T-3", state: "running" }),
      ticket({ id: "T-4", state: "closed" }),
    ]);
    expect(groups.ready.map((t) => t.id)).toEqual(["T-1"]);
    expect(groups.blocked.map((t) => t.id)).toEqual(["T-2"]);
    expect(groups.active.map((t) => t.id)).toEqual(["T-3"]);
  });
});
