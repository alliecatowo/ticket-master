import { describe, expect, it } from "vitest";
import { ProjectStore } from "./store";
import { event, stateSnapshot } from "../test/fixtures";

const snapshot = stateSnapshot();

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

  it("folds an event at or below the snapshot head into history only, never the projection", () => {
    const store = new ProjectStore();
    store.seed({ ...snapshot, head: 5 });
    // The snapshot already includes seq 3; the replay still delivers it for the timeline.
    const applied = store.apply(event({ seq: 3 }));
    expect(applied).toBe(true);
    const view = store.snapshot();
    expect(view.tickets.get("T-1")?.state).toBe("ready");
    expect(view.head).toBe(5);
    expect(view.timelines.get("T-1")?.map((e) => e.seq)).toEqual([3]);
    expect(store.cursor()).toBe(3);
  });

  it("never folds the same event twice (a replayed overlap after a reconnect)", () => {
    const store = new ProjectStore();
    store.seed(snapshot);
    expect(store.apply(event({ seq: 1 }))).toBe(true);
    // Reconnect: a fresh snapshot, then the stream resumes from the cursor and may overlap.
    store.seed({ ...snapshot, head: 1, tickets: [{ ...snapshot.tickets[0], state: "leased" }] });
    expect(store.apply(event({ seq: 1 }))).toBe(false);
    expect(store.snapshot().timelines.get("T-1")).toHaveLength(1);
  });

  it("clears history when the server's log is behind the cursor (a different or reset project)", () => {
    const store = new ProjectStore();
    store.seed(snapshot);
    store.apply(event({ seq: 1 }));
    store.apply(event({ seq: 2, payload: { ticket: "T-1", from: "leased", to: "running" } }));
    expect(store.seed({ ...snapshot, head: 1 })).toBe("replay");
    expect(store.cursor()).toBe(0);
    expect(store.snapshot().timelines.size).toBe(0);
  });

  it("folds submission, escalation and approval activity from the log", () => {
    const store = new ProjectStore();
    store.seed(snapshot);
    store.apply(event({ seq: 1, kind: "ticket.submitted", payload: { ticket: "T-1", summary: "did it" } }));
    store.apply(
      event({ seq: 2, kind: "approval.requested", payload: { ticket: "T-1", requested_of: "agent:w", note: "rm -rf" } }),
    );
    let act = store.snapshot().activity.get("T-1");
    expect(act?.submission).toBe("did it");
    expect(act?.pendingApproval?.note).toBe("rm -rf");
    store.apply(event({ seq: 3, kind: "approval.decided", payload: { ticket: "T-1", approved: true } }));
    store.apply(event({ seq: 4, kind: "ticket.escalated", payload: { ticket: "T-1", reason: "out of attempts" } }));
    act = store.snapshot().activity.get("T-1");
    expect(act?.pendingApproval).toBeUndefined();
    expect(act?.escalation?.reason).toBe("out of attempts");
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
