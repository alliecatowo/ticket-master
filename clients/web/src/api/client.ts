import type {
  PresenceSnapshot,
  StateSnapshot,
  Ticket,
  TicketId,
  TransitionRequest,
  WireEvent,
} from "./types";

export interface TicketmasterClientOptions {
  /** Base URL for the API. Empty string means same-origin (the `tm serve` deployment story). */
  baseUrl?: string;
  /** Bearer token, required only when `tm-server` is bound to a non-loopback address. */
  token?: string;
  /** Injectable for tests; defaults to `window.fetch`. */
  fetchImpl?: typeof fetch;
}

class HttpError extends Error {
  constructor(
    public readonly status: number,
    public readonly body: string,
  ) {
    super(`tm-server responded ${status}: ${body}`);
  }
}

/**
 * REST + SSE client for `tm-server` (SPEC.md §14 / §18.1).
 *
 * This is the hand-written stand-in for the generated `@ticketmaster/client` package described
 * in SPEC.md §18.1 (`clients/ts/`), which does not exist in this repository yet — see
 * ../../README.md. Every mutation the web app performs goes through this class; nothing else in
 * `clients/web` calls `fetch` directly.
 */
export class TicketmasterClient {
  private readonly baseUrl: string;
  private readonly token?: string;
  private readonly fetchImpl: typeof fetch;

  constructor(options: TicketmasterClientOptions = {}) {
    this.baseUrl = options.baseUrl ?? "";
    this.token = options.token;
    this.fetchImpl = options.fetchImpl ?? fetch.bind(globalThis);
  }

  private async request<T>(path: string, init?: RequestInit): Promise<T> {
    const headers = new Headers(init?.headers);
    if (init?.body) headers.set("content-type", "application/json");
    if (this.token) headers.set("authorization", `Bearer ${this.token}`);
    const res = await this.fetchImpl(`${this.baseUrl}${path}`, { ...init, headers });
    const text = await res.text();
    if (!res.ok) throw new HttpError(res.status, text);
    return text.length ? (JSON.parse(text) as T) : (undefined as T);
  }

  health(): Promise<{ status: string }> {
    return this.request("/health");
  }

  getState(): Promise<StateSnapshot> {
    return this.request("/state");
  }

  listTickets(): Promise<Ticket[]> {
    return this.request("/tickets");
  }

  getTicket(id: TicketId): Promise<Ticket> {
    return this.request(`/tickets/${encodeURIComponent(id)}`);
  }

  getPresence(): Promise<PresenceSnapshot> {
    return this.request("/presence");
  }

  transition(id: TicketId, body: TransitionRequest): Promise<{ events: unknown[] }> {
    return this.request(`/tickets/${encodeURIComponent(id)}/transition`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /**
   * `subscribe(fromSeq)`: an async iterable over `GET /events?from=<seq>`, per SPEC.md §18.1.
   *
   * Reconnects with the last-seen `seq` on any stream error, so a caller that keeps consuming
   * this iterator sees a gap-free (if occasionally re-requested) sequence across reconnects.
   * Uses `fetch` + a streaming body reader rather than `EventSource`, because `EventSource`
   * cannot set an `Authorization` header, which this app needs when `tm-server` is bound
   * non-loopback.
   */
  async *subscribe(fromSeq = 0, signal?: AbortSignal): AsyncGenerator<WireEvent> {
    let cursor = fromSeq;
    while (!signal?.aborted) {
      try {
        for await (const event of this.streamOnce(cursor, signal)) {
          cursor = event.seq;
          yield event;
        }
        // Server closed the stream cleanly; nothing left to read right now — stop.
        return;
      } catch (err) {
        if (signal?.aborted) return;
        // Reconnect from the last admitted seq. A brief backoff avoids a hot loop against a
        // server that is down.
        await new Promise((resolve) => setTimeout(resolve, 1000));
        continue;
      }
    }
  }

  private async *streamOnce(fromSeq: number, signal?: AbortSignal): AsyncGenerator<WireEvent> {
    const headers = new Headers();
    if (this.token) headers.set("authorization", `Bearer ${this.token}`);
    const res = await this.fetchImpl(`${this.baseUrl}/events?from=${fromSeq}`, {
      headers,
      signal,
    });
    if (!res.ok || !res.body) {
      throw new HttpError(res.status, await res.text().catch(() => ""));
    }
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return;
        buffer += decoder.decode(value, { stream: true });
        let boundary = buffer.indexOf("\n\n");
        while (boundary !== -1) {
          const raw = buffer.slice(0, boundary);
          buffer = buffer.slice(boundary + 2);
          const event = parseSseFrame(raw);
          if (event) yield event;
          boundary = buffer.indexOf("\n\n");
        }
      }
    } finally {
      reader.releaseLock();
    }
  }
}

/** Parse one `\n\n`-delimited SSE frame (as written by `to_sse_event` in `sse.rs`). */
function parseSseFrame(raw: string): WireEvent | null {
  let data = "";
  for (const line of raw.split("\n")) {
    if (line.startsWith("data:")) data += line.slice(5).trimStart();
  }
  if (!data) return null;
  return JSON.parse(data) as WireEvent;
}
