import { describe, expect, it, vi } from "vitest";
import { subscribeToEvents } from "../src/sse.js";
import type { WireEvent } from "../src/domain.js";

/** Render a batch of events as the SSE wire text `tm-server/src/sse.rs::to_sse_event` emits. */
function renderSse(events: WireEvent[]): string {
  return events
    .map((e) => `id: ${e.seq}\nevent: ${e.kind}\ndata: ${JSON.stringify(e)}\n\n`)
    .join("");
}

function streamOf(text: string): ReadableStream<Uint8Array> {
  const bytes = new TextEncoder().encode(text);
  return new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(bytes);
      controller.close();
    },
  });
}

function makeEvent(seq: number): WireEvent {
  return {
    seq,
    ts: `2026-01-01T00:00:${String(seq).padStart(2, "0")}Z`,
    kind: "ticket.state_changed",
    subject: "T-1",
    actor: "P-1",
    session: null,
    causation: null,
    correlation: null,
    payload: { ticket: "T-1", from: "draft", to: "ready" },
  };
}

function fakeResponse(body: ReadableStream<Uint8Array>): Response {
  return { ok: true, status: 200, body } as unknown as Response;
}

/**
 * The core contract this module exists for: a connection that drops mid-stream (the server
 * closes without warning after seq 2) must not cause any event to be delivered twice, or any
 * event to go missing, once the client reconnects and the server replays the overlap window
 * (here, re-sending seq 2 alongside the events seq 2 never actually got flushed for).
 */
describe("subscribeToEvents reconnect", () => {
  it("delivers every event exactly once across a simulated connection drop", async () => {
    const firstConnection = renderSse([makeEvent(1), makeEvent(2)]);
    // Simulates `tm-server`'s backlog/live overlap: the reconnect replays seq 2 (already
    // delivered) before continuing with genuinely new events.
    const secondConnection = renderSse([makeEvent(2), makeEvent(3), makeEvent(4)]);

    const urls: string[] = [];
    const headersSeen: Array<Record<string, string>> = [];
    const fetchMock = vi.fn(async (url: URL, init?: RequestInit) => {
      urls.push(url.toString());
      headersSeen.push((init?.headers as Record<string, string>) ?? {});
      const call = fetchMock.mock.calls.length;
      return fakeResponse(streamOf(call === 1 ? firstConnection : secondConnection));
    });

    const sleepCalls: number[] = [];
    const fakeSleep = async (ms: number) => {
      sleepCalls.push(ms);
    };

    const received: WireEvent[] = [];
    const controller = new AbortController();
    let iterations = 0;
    for await (const event of subscribeToEvents(0, {
      baseUrl: "http://127.0.0.1:4173",
      fetch: fetchMock as unknown as typeof fetch,
      sleep: fakeSleep,
      signal: controller.signal,
    })) {
      received.push(event);
      iterations += 1;
      // Stop once the second connection's genuinely-new tail has been delivered, so the
      // generator doesn't loop forever trying a third connection against the same fixture.
      if (iterations === 4) {
        controller.abort();
        break;
      }
    }

    expect(received.map((e) => e.seq)).toEqual([1, 2, 3, 4]);
    // Exactly once: seq 2 must not appear twice despite the second connection re-sending it.
    expect(received.filter((e) => e.seq === 2)).toHaveLength(1);

    expect(fetchMock).toHaveBeenCalledTimes(2);
    // The reconnect resumes from the last admitted seq (2), both as the `from` query param the
    // server actually implements and as `Last-Event-ID` for forward compatibility.
    expect(urls[1]).toContain("from=2");
    expect(headersSeen[1]?.["Last-Event-ID"]).toBe("2");
  });

  it("backs off and retries when the server is unreachable, then recovers", async () => {
    let call = 0;
    const fetchMock = vi.fn(async () => {
      call += 1;
      if (call === 1) throw new Error("ECONNREFUSED");
      return fakeResponse(streamOf(renderSse([makeEvent(1)])));
    });

    const sleepCalls: number[] = [];
    const fakeSleep = async (ms: number) => {
      sleepCalls.push(ms);
    };

    const controller = new AbortController();
    const received: WireEvent[] = [];
    for await (const event of subscribeToEvents(0, {
      baseUrl: "http://127.0.0.1:4173",
      fetch: fetchMock as unknown as typeof fetch,
      sleep: fakeSleep,
      initialBackoffMs: 10,
      signal: controller.signal,
    })) {
      received.push(event);
      controller.abort();
      break;
    }

    expect(received.map((e) => e.seq)).toEqual([1]);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(sleepCalls).toEqual([10]);
  });

  it("stops cleanly when the caller's AbortSignal fires without yielding a duplicate on restart", async () => {
    const fetchMock = vi.fn(async () => fakeResponse(streamOf(renderSse([makeEvent(1), makeEvent(2)]))));
    const controller = new AbortController();
    const received: WireEvent[] = [];

    for await (const event of subscribeToEvents(0, {
      baseUrl: "http://127.0.0.1:4173",
      fetch: fetchMock as unknown as typeof fetch,
      sleep: async () => {},
      signal: controller.signal,
    })) {
      received.push(event);
      if (event.seq === 2) controller.abort();
    }

    expect(received.map((e) => e.seq)).toEqual([1, 2]);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });
});
