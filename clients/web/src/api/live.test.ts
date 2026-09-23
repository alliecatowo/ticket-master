import { describe, expect, it } from "vitest";
import type { TicketmasterClient } from "./client";
import { LiveProject, type LiveState } from "./live";
import type { StateSnapshot, WireEvent } from "./types";
import { event, stateSnapshot, ticket } from "../test/fixtures";

/**
 * A stand-in for `TicketmasterClient`: each `streamEvents` call plays the next scripted
 * connection (yield these events, then fail or hang), and records the `from` it was asked for.
 */
function fakeClient(snapshots: StateSnapshot[], connections: Array<{ events: WireEvent[]; then: "fail" | "hang" }>) {
  const froms: number[] = [];
  let states = 0;
  let conns = 0;
  const client = {
    async getState() {
      return snapshots[Math.min(states++, snapshots.length - 1)];
    },
    async *streamEvents(from: number, signal?: AbortSignal) {
      froms.push(from);
      const conn = connections[Math.min(conns++, connections.length - 1)];
      for (const e of conn.events) if (e.seq > from) yield e;
      if (conn.then === "fail") throw new TypeError("network error");
      await new Promise<void>((resolve) => signal?.addEventListener("abort", () => resolve()));
    },
  };
  return { client: client as unknown as TicketmasterClient, froms };
}

async function until(check: () => boolean, ms = 1000) {
  const start = Date.now();
  while (!check()) {
    if (Date.now() - start > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 2));
  }
}

describe("LiveProject", () => {
  it("goes live, reconnects after a dropped stream, and resumes without duplicating history", async () => {
    const e1 = event({ seq: 1 });
    const e2 = event({ seq: 2, payload: { ticket: "T-1", from: "leased", to: "running" } });
    const e3 = event({ seq: 3, kind: "ticket.submitted", payload: { ticket: "T-1", summary: "done" } });
    const { client, froms } = fakeClient(
      [
        stateSnapshot({ head: 0 }),
        stateSnapshot({ head: 3, tickets: [ticket({ state: "submitted" })] }),
      ],
      [
        { events: [e1, e2], then: "fail" },
        // The server replays from the requested seq; an overlapping e2 must not be folded twice.
        { events: [e2, e3], then: "hang" },
      ],
    );
    const live = new LiveProject(client, { backoffMs: [5], flushMs: 1 });
    const states: LiveState[] = [];
    const stop = live.start((s) => states.push(s));

    await until(() => states.some((s) => s.status === "reconnecting"));
    const dropped = states.find((s) => s.status === "reconnecting")!;
    expect(dropped.error).toMatch(/Couldn't reach tm serve/);
    expect(dropped.loaded).toBe(true);

    await until(() => froms.length === 2 && states[states.length - 1].store.timelines.get("T-1")?.length === 3);
    expect(froms).toEqual([0, 2]);
    const last = states[states.length - 1];
    expect(last.status).toBe("live");
    expect(last.store.timelines.get("T-1")!.map((e) => e.seq)).toEqual([1, 2, 3]);
    expect(last.store.tickets.get("T-1")?.state).toBe("submitted");
    expect(last.store.activity.get("T-1")?.submission).toBe("done");
    stop();
  });

  it("retryNow skips the rest of the backoff", async () => {
    let calls = 0;
    const client = {
      async getState(): Promise<StateSnapshot> {
        calls++;
        throw new TypeError("Failed to fetch");
      },
    } as unknown as TicketmasterClient;
    const live = new LiveProject(client, { backoffMs: [60_000] });
    const stop = live.start(() => {});
    await until(() => calls === 1);
    live.retryNow();
    await until(() => calls === 2);
    stop();
  });

  it("says connecting, not reconnecting, until the first snapshot loads", async () => {
    const client = {
      async getState(): Promise<StateSnapshot> {
        throw new TypeError("Failed to fetch");
      },
    } as unknown as TicketmasterClient;
    const states: LiveState[] = [];
    const stop = new LiveProject(client, { backoffMs: [1000] }).start((s) => states.push(s));
    await until(() => states.length > 0);
    expect(states[0]).toMatchObject({ status: "connecting", loaded: false });
    expect(states[0].retryAt).not.toBeNull();
    stop();
  });
});
