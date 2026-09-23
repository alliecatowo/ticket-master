import { describe, expect, it } from "vitest";
import { ProjectStore } from "../api/store";
import type { TicketState } from "../api/types";
import { event, stateSnapshot, ticket } from "../test/fixtures";
import {
  canCancel,
  choicesFor,
  compactAge,
  countsLine,
  GROUP_ORDER,
  groupFor,
  groupOverviews,
  overviewOf,
  overviewsOf,
  shortTitle,
  type TicketGroup,
} from "./ticketsModel";

const ALL_STATES: TicketState[] = [
  "draft",
  "blocked",
  "ready",
  "leased",
  "running",
  "submitted",
  "verifying",
  "auditing",
  "rework",
  "replan",
  "recovery",
  "escalated",
  "closed",
  "cancelled",
];

describe("groupFor (overview.rs group_for)", () => {
  const expected: Record<TicketState, TicketGroup> = {
    draft: "queued",
    blocked: "queued",
    ready: "queued",
    leased: "working",
    running: "working",
    submitted: "review",
    verifying: "working",
    auditing: "working",
    rework: "queued",
    replan: "queued",
    recovery: "queued",
    escalated: "needs_input",
    closed: "completed",
    cancelled: "completed",
  };

  it.each(ALL_STATES)("puts %s in its group", (state) => {
    expect(groupFor(state, false)).toBe(expected[state]);
  });

  it("moves a leased or running ticket waiting on an approval to Needs input", () => {
    expect(groupFor("leased", true)).toBe("needs_input");
    expect(groupFor("running", true)).toBe("needs_input");
  });

  it("ignores a stale approval on any other state", () => {
    for (const state of ALL_STATES.filter((s) => s !== "leased" && s !== "running")) {
      expect(groupFor(state, true)).toBe(expected[state]);
    }
  });

  it("orders groups as the TUI does", () => {
    expect(GROUP_ORDER).toEqual(["needs_input", "working", "review", "queued", "completed"]);
  });
});

describe("actions each state allows (tickets_view.rs choices)", () => {
  it("offers Accept then Reject on a submission", () => {
    expect(choicesFor("submitted")).toEqual(["accept", "reject"]);
  });
  it("offers Retry then Retry with guidance on an escalation", () => {
    expect(choicesFor("escalated")).toEqual(["retry", "retry_with_guidance"]);
  });
  it("offers Queue it on a draft", () => {
    expect(choicesFor("draft")).toEqual(["queue"]);
  });
  it("offers no numbered choice anywhere else", () => {
    for (const state of ALL_STATES.filter((s) => !["submitted", "escalated", "draft"].includes(s))) {
      expect(choicesFor(state)).toEqual([]);
    }
  });
  it("offers Cancel on every state except closed and cancelled", () => {
    for (const state of ALL_STATES) {
      expect(canCancel(state)).toBe(state !== "closed" && state !== "cancelled");
    }
  });
});

describe("overviews", () => {
  const NOW = Date.parse("2026-01-01T01:00:00Z");

  it("summarizes an escalation with its attempts and last failure", () => {
    const store = new ProjectStore();
    store.seed(
      stateSnapshot({
        tickets: [
          ticket({
            state: "escalated",
            attempts: 3,
            failures: [{ class: "other", detail: "mock provider said no", at: "2026-01-01T00:10:00Z", attempt: 3 }],
          }),
        ],
      }),
    );
    store.apply(
      event({ seq: 1, kind: "ticket.escalated", payload: { ticket: "T-1", reason: "mock provider said no" } }),
    );
    const o = overviewOf(store.snapshot().tickets.get("T-1")!, store.snapshot(), NOW);
    expect(o.group).toBe("needs_input");
    expect(o.waitingFor).toBe("escalation");
    expect(o.summary).toBe("gave up after 3 attempts: mock provider said no");
  });

  it("shows a submission's summary and holds a completed ticket's age at its run length", () => {
    const store = new ProjectStore();
    store.seed(
      stateSnapshot({
        tickets: [
          ticket({ id: "T-1", state: "submitted" }),
          ticket({ id: "T-2", state: "closed", updated: "2026-01-01T00:05:00Z" }),
        ],
      }),
    );
    store.apply(event({ seq: 1, kind: "ticket.submitted", payload: { ticket: "T-1", summary: "added\nthe test" } }));
    const snap = store.snapshot();
    const submitted = overviewOf(snap.tickets.get("T-1")!, snap, NOW);
    expect(submitted.summary).toBe("added the test");
    expect(submitted.group).toBe("review");
    const closed = overviewOf(snap.tickets.get("T-2")!, snap, NOW);
    expect(compactAge(closed.ageMs)).toBe("5m");
  });

  it("shows no worker on a finished ticket whose lease has not lapsed", () => {
    const store = new ProjectStore();
    store.seed(
      stateSnapshot({
        tickets: [ticket({ state: "closed" })],
        leases: [
          {
            id: "L-000000000001",
            ticket: "T-1",
            holder: "agent:mock/w1",
            authority: null,
            resources: [],
            acquired: "2026-01-01T00:59:00Z",
            heartbeat: "2026-01-01T00:59:00Z",
            ttl_seconds: 600,
            epoch: 1,
          },
        ],
      }),
    );
    const o = overviewOf(store.snapshot().tickets.get("T-1")!, store.snapshot(), NOW);
    expect(o.worker).toBeNull();
    expect(o.working).toBe(false);
  });

  it("groups newest first, most recently finished first in Completed, and counts them", () => {
    const store = new ProjectStore();
    store.seed(
      stateSnapshot({
        tickets: [
          ticket({ id: "T-2", state: "ready" }),
          ticket({ id: "T-10", state: "ready" }),
          ticket({ id: "T-3", state: "closed", updated: "2026-01-01T00:01:00Z" }),
          ticket({ id: "T-4", state: "cancelled", updated: "2026-01-01T00:09:00Z" }),
          ticket({ id: "T-5", state: "submitted" }),
        ],
      }),
    );
    const groups = groupOverviews(overviewsOf(store.snapshot(), NOW));
    expect(groups.queued.map((o) => o.id)).toEqual(["T-10", "T-2"]);
    expect(groups.completed.map((o) => o.id)).toEqual(["T-4", "T-3"]);
    expect(countsLine(groups)).toBe("1 ready for review · 2 queued · 2 completed");
  });
});

describe("shortTitle", () => {
  it("keeps the first clause and fits 32 columns", () => {
    expect(shortTitle("Fix the flaky test, then tidy up")).toBe("Fix the flaky test");
    expect(shortTitle("Rewrite the whole ticket grouping model for the web client")).toBe(
      "Rewrite the whole ticket…",
    );
  });
});
