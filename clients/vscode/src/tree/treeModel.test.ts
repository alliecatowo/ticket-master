import type { Milestone, Ticket } from "@ticketmaster/client";
import { describe, expect, it } from "vitest";
import { buildMilestoneTree, stateBadge, type TicketNode } from "./treeModel";

function ticket(overrides: Partial<Ticket> & { id: string }): Ticket {
  return {
    kind: "work",
    objective: "do the thing",
    state: "ready",
    parent: null,
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
    retry: { max_attempts: 3, base_delay_seconds: 1, backoff_multiplier: 2, max_delay_seconds: 60 },
    cycle: null,
    attempts: 0,
    failures: [],
    priority: 0,
    created: "2026-01-01T00:00:00Z",
    updated: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function milestone(overrides: Partial<Milestone> & { id: string }): Milestone {
  return {
    title: overrides.id,
    tickets: [],
    state: "open",
    closed_by: null,
    assumptions: [],
    ...overrides,
  };
}

describe("buildMilestoneTree", () => {
  it("nests child tickets under their parent within a milestone", () => {
    const m = milestone({ id: "M-1", tickets: ["T-1", "T-2"] });
    const t1 = ticket({ id: "T-1", milestone: "M-1", children: ["T-2"] });
    const t2 = ticket({ id: "T-2", milestone: "M-1", parent: "T-1" });

    const tree = buildMilestoneTree([m], [t1, t2]);

    expect(tree).toHaveLength(1);
    const milestoneNode = tree[0];
    expect(milestoneNode.kind).toBe("milestone");
    expect(milestoneNode.children).toHaveLength(1);
    expect(milestoneNode.children[0].id).toBe("T-1");
    expect(milestoneNode.children[0].children).toHaveLength(1);
    expect(milestoneNode.children[0].children[0].id).toBe("T-2");
  });

  it("treats a ticket whose parent is in a different milestone as a root", () => {
    const m1 = milestone({ id: "M-1", tickets: ["T-1"] });
    const m2 = milestone({ id: "M-2", tickets: ["T-2"] });
    const t1 = ticket({ id: "T-1", milestone: "M-1" });
    const t2 = ticket({ id: "T-2", milestone: "M-2", parent: "T-1" });

    const tree = buildMilestoneTree([m1, m2], [t1, t2]);

    const m2Node = tree.find((n) => n.id === "M-2")!;
    expect(m2Node.children.map((c) => (c as TicketNode).id)).toEqual(["T-2"]);
  });

  it("collects milestone-less root tickets into an Unassigned group", () => {
    const t1 = ticket({ id: "T-1", milestone: null });

    const tree = buildMilestoneTree([], [t1]);

    expect(tree).toHaveLength(1);
    expect(tree[0].kind).toBe("unassigned-group");
    expect(tree[0].children.map((c) => c.id)).toEqual(["T-1"]);
  });

  it("omits the Unassigned group when every ticket belongs to a milestone", () => {
    const m = milestone({ id: "M-1", tickets: ["T-1"] });
    const t1 = ticket({ id: "T-1", milestone: "M-1" });

    const tree = buildMilestoneTree([m], [t1]);

    expect(tree).toHaveLength(1);
    expect(tree[0].kind).toBe("milestone");
  });

  it("skips dangling child references instead of throwing", () => {
    const m = milestone({ id: "M-1", tickets: ["T-1"] });
    const t1 = ticket({ id: "T-1", milestone: "M-1", children: ["T-missing"] });

    const tree = buildMilestoneTree([m], [t1]);

    expect(tree[0].children).toHaveLength(1);
    expect(tree[0].children[0].children).toHaveLength(0);
  });

  it("guards against cycles in child links", () => {
    const m = milestone({ id: "M-1", tickets: ["T-1", "T-2"] });
    const t1 = ticket({ id: "T-1", milestone: "M-1", children: ["T-2"] });
    const t2 = ticket({
      id: "T-2",
      milestone: "M-1",
      parent: "T-1",
      children: ["T-1"],
    });

    expect(() => buildMilestoneTree([m], [t1, t2])).not.toThrow();
    const tree = buildMilestoneTree([m], [t1, t2]);
    const root = tree[0].children[0] as TicketNode;
    expect(root.id).toBe("T-1");
    expect(root.children[0].id).toBe("T-2");
    expect(root.children[0].children).toHaveLength(0);
  });
});

describe("stateBadge", () => {
  it("returns a distinct badge per ticket state", () => {
    const ready = stateBadge("ready");
    const blocked = stateBadge("blocked");
    expect(ready.text).toBe("READY");
    expect(blocked.text).toBe("BLOCKED");
    expect(ready.icon).not.toBe(blocked.icon);
  });
});
