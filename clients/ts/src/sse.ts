// A minimal Server-Sent Events reader plus the reconnect-with-Last-Event-ID loop
// `TicketmasterClient.subscribe` is built on (SPEC.md §18.1, `tm-server/src/sse.rs`'s wire
// format: `id: <seq>`, `event: <kind>`, `data: <json>`, blocks separated by a blank line).
//
// This module intentionally does not use the browser `EventSource` API: `EventSource` cannot
// send arbitrary headers (no bearer token) and its reconnect behavior isn't observable/testable
// from outside. Reading `Response.body` by hand gives full control over both, and works
// identically under Node's `fetch` and a browser's.

import type { WireEvent } from "./domain.js";

/** One raw SSE frame, before this module's caller interprets `data` as JSON. */
export interface RawSseFrame {
  id?: string;
  event?: string;
  data: string;
}

/**
 * Split a byte stream into SSE frames. Frames are separated by a blank line; each line within a
 * frame is `field: value` (or `field:value`); unknown fields are ignored, per the SSE spec.
 */
export async function* readSseFrames(
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<RawSseFrame> {
  const reader = body.getReader();
  const decoder = new TextDecoder("utf-8");
  let buffer = "";
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let boundary: number;
      // eslint-disable-next-line no-cond-assign
      while ((boundary = buffer.indexOf("\n\n")) !== -1) {
        const block = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        const frame = parseBlock(block);
        if (frame) yield frame;
      }
    }
    // A trailing partial block with no final blank line is not a complete frame; discard it
    // (mirrors how a client would treat a connection that ended mid-frame).
  } finally {
    reader.releaseLock();
  }
}

function parseBlock(block: string): RawSseFrame | null {
  let id: string | undefined;
  let event: string | undefined;
  const dataLines: string[] = [];
  for (const rawLine of block.split("\n")) {
    const line = rawLine.replace(/\r$/, "");
    if (line === "" || line.startsWith(":")) continue;
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    switch (field) {
      case "id":
        id = value;
        break;
      case "event":
        event = value;
        break;
      case "data":
        dataLines.push(value);
        break;
      default:
        break;
    }
  }
  if (dataLines.length === 0 && id === undefined && event === undefined) return null;
  return { id, event, data: dataLines.join("\n") };
}

/** Parse one frame's `data:` payload as a {@link WireEvent}. Throws on malformed JSON. */
export function frameToEvent(frame: RawSseFrame): WireEvent {
  return JSON.parse(frame.data) as WireEvent;
}

export interface ReconnectOptions {
  /** Base URL of the Ticketmaster server, e.g. `http://127.0.0.1:4173`. */
  baseUrl: string;
  /** Bearer token, if the server requires one (non-loopback bind; SPEC.md §14). */
  token?: string;
  /** Injectable for tests; defaults to the global `fetch`. */
  fetch?: typeof fetch;
  /** Delay before the first reconnect attempt; doubles (capped) on repeated failures. */
  initialBackoffMs?: number;
  /** Upper bound on the backoff delay. */
  maxBackoffMs?: number;
  /** Injectable sleep, so tests don't pay wall-clock backoff delays. */
  sleep?: (ms: number) => Promise<void>;
  /** Aborts the whole subscription (in-flight request included) when triggered. */
  signal?: AbortSignal;
}

const defaultSleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * `GET /events?from=<seq>` as a reconnecting async iterable. Reconnects on any stream error or
 * unexpected close, resuming with `from=<last admitted seq>` (sent as both the query parameter
 * the server actually implements and a `Last-Event-ID` header, for forward compatibility) —
 * never dropping or re-delivering an event across the reconnect, mirroring the server-side
 * dedup contract in `tm-server/src/sse.rs`'s `ResumableStream`.
 */
export async function* subscribeToEvents(
  fromSeq: number,
  opts: ReconnectOptions,
): AsyncGenerator<WireEvent, void, void> {
  const fetchImpl = opts.fetch ?? fetch;
  const sleep = opts.sleep ?? defaultSleep;
  const initialBackoff = opts.initialBackoffMs ?? 250;
  const maxBackoff = opts.maxBackoffMs ?? 5_000;

  // The client-side half of the same admit/dedup guard `tm-server/src/sse.rs::ResumableStream`
  // implements server-side: guards against a reconnect replaying a seq already yielded (the
  // server's own backlog/live handover overlap, or a resumed connection re-sending the frame
  // that was in flight when the previous connection dropped).
  let nextExpectedSeq = fromSeq + 1;
  let backoff = initialBackoff;

  while (!opts.signal?.aborted) {
    const controller = new AbortController();
    const onAbort = () => controller.abort();
    opts.signal?.addEventListener("abort", onAbort, { once: true });

    try {
      const url = new URL("/events", opts.baseUrl);
      url.searchParams.set("from", String(nextExpectedSeq - 1));
      const headers: Record<string, string> = { Accept: "text/event-stream" };
      if (opts.token) headers.Authorization = `Bearer ${opts.token}`;
      if (nextExpectedSeq > 1) headers["Last-Event-ID"] = String(nextExpectedSeq - 1);

      const res = await fetchImpl(url, { headers, signal: controller.signal });
      if (!res.ok || !res.body) {
        throw new Error(`GET /events failed: ${res.status}`);
      }

      for await (const frame of readSseFrames(res.body)) {
        if (frame.data === "") continue;
        const event = frameToEvent(frame);
        if (event.seq < nextExpectedSeq) continue; // already delivered; drop the duplicate
        nextExpectedSeq = event.seq + 1;
        backoff = initialBackoff; // a successful frame resets the reconnect backoff
        yield event;
      }
      // Stream ended (server closed the connection cleanly): reconnect from where we left off,
      // after a short delay so a server that closes immediately doesn't spin us in a tight loop.
    } catch (err) {
      if (opts.signal?.aborted) return;
      if (err instanceof Error && err.name === "AbortError") return;
      // Any other failure (network drop, non-2xx, malformed frame) is treated the same way:
      // back off and reconnect from `nextExpectedSeq - 1`.
    } finally {
      opts.signal?.removeEventListener("abort", onAbort);
    }

    if (opts.signal?.aborted) return;
    await sleep(backoff);
    backoff = Math.min(backoff * 2, maxBackoff);
  }
}
