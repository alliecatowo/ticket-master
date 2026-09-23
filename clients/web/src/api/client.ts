import { actorFor, DEFAULT_HANDLE } from "./identity";
import type {
  CreateTicketResponse,
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
  /** The human handle mutations act as (`human:<handle>`); defaults to `web`. */
  handle?: string;
}

/**
 * A non-2xx answer from `tm-server`. `message` is the server's own sentence when it sent one:
 * domain errors come back as JSON `{error, message}` (403 authority_denied, 409
 * invalid_transition, 404 not_found), while a body axum could not deserialize comes back as plain
 * text with a 422.
 */
export class HttpError extends Error {
  readonly code: string | null;

  constructor(
    public readonly status: number,
    public readonly body: string,
  ) {
    const parsed = parseErrorBody(body);
    super(parsed.message || `tm-server responded ${status}`);
    this.name = "HttpError";
    this.code = parsed.code;
  }
}

function parseErrorBody(body: string): { code: string | null; message: string } {
  try {
    const json = JSON.parse(body) as { error?: unknown; message?: unknown };
    if (json && typeof json.message === "string") {
      return { code: typeof json.error === "string" ? json.error : null, message: json.message };
    }
  } catch {
    // Plain text (axum's rejection bodies) — use as is.
  }
  return { code: null, message: body.trim() };
}

/** A user-facing sentence for any error a mutation can throw. */
export function describeError(err: unknown): string {
  if (err instanceof HttpError) {
    if (err.status === 403) return `Not allowed: ${stripPrefix(err.message)}`;
    if (err.status === 409) return `Can't do that now: ${stripPrefix(err.message)}`;
    if (err.status === 401) return "The server wants a token (it is bound to a non-loopback address).";
    return err.message;
  }
  if (err instanceof TypeError) return "Couldn't reach tm serve. Is it still running?";
  return err instanceof Error ? err.message : String(err);
}

function stripPrefix(message: string): string {
  return message.replace(/^(authority denied|invalid transition|conflict):\s*/i, "");
}

/** Every ticket action the web client offers, matching the TUI's per-state choices. */
export type TicketAction =
  | { type: "queue" }
  | { type: "accept"; note?: string | null }
  | { type: "reject"; reason: string }
  | { type: "retry"; guidance?: string | null }
  | { type: "cancel"; reason?: string | null };

/**
 * The exact `POST /tickets/:id/transition` body for `action`, acting as `actor`. Pure, so the
 * wire shapes are unit-tested (`client.test.ts`). Optional text is trimmed and sent as `null`
 * when empty; `reject` requires a reason (the server would accept an empty one, the TUI does not).
 */
export function transitionBody(action: TicketAction, actor: string): TransitionRequest {
  const text = (value: string | null | undefined) => {
    const trimmed = (value ?? "").trim();
    return trimmed.length ? trimmed : null;
  };
  switch (action.type) {
    case "queue":
      return { activate: { actor } };
    case "accept":
      return { accept: { note: text(action.note), actor } };
    case "reject": {
      const reason = text(action.reason);
      if (!reason) throw new Error("A rejection needs a reason, so the next attempt knows what to fix.");
      return { reject: { reason, actor } };
    }
    case "retry":
      return { retry: { guidance: text(action.guidance), actor } };
    case "cancel":
      return { cancel: { reason: text(action.reason), actor } };
  }
}

/** The outcome of `dispatch`: the created ticket, and whether it made it into the queue. */
export interface DispatchResult {
  ticket: Ticket;
  queued: boolean;
  /** Why queueing failed, when it did (the ticket then stays a draft). */
  queueError?: string;
}

/**
 * REST + SSE client for `tm-server` (SPEC.md §14 / §18.1).
 *
 * This is the hand-written stand-in for the generated `@ticketmaster/client` package described
 * in SPEC.md §18.1 (`clients/ts/`) — see ../../README.md. Every request the web app makes goes
 * through this class, and so does identity: `handle` is the one place the acting human lives, and
 * every mutation body gets `actor: human:<handle>` from it.
 */
export class TicketmasterClient {
  private readonly baseUrl: string;
  private readonly token?: string;
  private readonly fetchImpl: typeof fetch;
  private currentHandle: string;

  constructor(options: TicketmasterClientOptions = {}) {
    this.baseUrl = options.baseUrl ?? "";
    this.token = options.token;
    this.fetchImpl = options.fetchImpl ?? ((input, init) => globalThis.fetch(input, init));
    this.currentHandle = options.handle ?? DEFAULT_HANDLE;
  }

  /** The handle mutations act as. */
  get handle(): string {
    return this.currentHandle;
  }

  set handle(handle: string) {
    this.currentHandle = handle;
  }

  /** `human:<handle>`: the actor every mutation sends. */
  get actor(): string {
    return actorFor(this.currentHandle);
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

  /** `POST /tickets` with only kind, objective and actor: the server fills `tm ticket new`'s defaults. */
  async createTicket(objective: string): Promise<Ticket> {
    const trimmed = objective.trim();
    if (!trimmed) throw new Error("Describe the task first.");
    const res = await this.request<CreateTicketResponse>("/tickets", {
      method: "POST",
      body: JSON.stringify({ kind: "work", objective: trimmed, actor: this.actor }),
    });
    return res.ticket;
  }

  /**
   * Create a work ticket and queue it (activate), like the TUI's dispatch input
   * (`tickets::create_and_queue`). If the create succeeds and the activate fails, the ticket
   * exists as a draft; the result says so rather than throwing away the created id.
   */
  async dispatch(objective: string): Promise<DispatchResult> {
    const ticket = await this.createTicket(objective);
    try {
      await this.act(ticket.id, { type: "queue" });
      return { ticket, queued: true };
    } catch (err) {
      return { ticket, queued: false, queueError: describeError(err) };
    }
  }

  /** Perform one ticket action as the current human. */
  act(id: TicketId, action: TicketAction): Promise<{ events: WireEvent[] }> {
    return this.transition(id, transitionBody(action, this.actor));
  }

  /** Raw transition; prefer `act`, which builds the body and fills in the actor. */
  transition(id: TicketId, body: TransitionRequest): Promise<{ events: WireEvent[] }> {
    return this.request(`/tickets/${encodeURIComponent(id)}/transition`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /**
   * One connection to `GET /events?from=<seq>`: yields every event after `fromSeq` (the backlog,
   * then live ones) until the stream ends or errors. Reconnecting is the caller's job
   * (`LiveProject` in `live.ts`), because a reconnect also has to re-read `GET /state`.
   *
   * Uses `fetch` + a streaming body reader rather than `EventSource`, because `EventSource`
   * cannot set an `Authorization` header, which this app needs when `tm-server` is bound
   * non-loopback.
   */
  async *streamEvents(fromSeq: number, signal?: AbortSignal): AsyncGenerator<WireEvent> {
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
        buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, "\n");
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
export function parseSseFrame(raw: string): WireEvent | null {
  let data = "";
  for (const line of raw.split("\n")) {
    if (line.startsWith("data:")) data += line.slice(5).trimStart();
  }
  if (!data) return null;
  try {
    return JSON.parse(data) as WireEvent;
  } catch {
    return null;
  }
}
