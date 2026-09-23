// Seeding and inspecting a project over `tm serve`'s HTTP API, for specs that need a ticket in a
// given state before the browser looks at it.
//
// Wire shapes follow `crates/tm-server/src/routes.rs` (`CreateTicketRequest`,
// `TransitionRequest`, `AcquireLeaseRequest`, `CreateArtifactRequest`). The transition body is
// externally tagged: `{"activate": {"actor": ...}}`, `{"trigger": {"trigger": "work_started",
// "actor": ...}}`, and so on.

export type TicketState =
  | "draft"
  | "blocked"
  | "ready"
  | "leased"
  | "running"
  | "submitted"
  | "verifying"
  | "auditing"
  | "rework"
  | "replan"
  | "recovery"
  | "escalated"
  | "closed"
  | "cancelled";

/** The parts of a ticket specs usually look at; the server sends more. */
export interface Ticket {
  id: string;
  kind: string;
  objective: string;
  state: TicketState;
  attempts: number;
  failures: unknown[];
  priority: number;
  created: string;
  updated: string;
  [field: string]: unknown;
}

/** Human actions (accept, reject, retry, cancel, activate) are sent as this actor. */
export const HUMAN = "human:e2e";
/** The non-human actor seeded worker steps (lease, start, submit) are sent as. */
export const WORKER = "agent:e2e/worker";

export class TmApiError extends Error {
  constructor(
    readonly method: string,
    readonly path: string,
    readonly status: number,
    readonly body: string,
  ) {
    super(`${method} ${path} -> ${status}: ${body}`);
  }
}

export interface SeededSubmission {
  id: string;
  /** The stored artifact submitted as evidence. */
  artifact: string;
  /** The seeded worker's lease. It stays live: the server refuses to release from `submitted`. */
  lease: string;
}

export class TmApi {
  constructor(readonly apiURL: string) {}

  /** Any API call. Throws [`TmApiError`] on a non-2xx answer. */
  async request<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await fetch(`${this.apiURL}${path}`, {
      method,
      headers: body === undefined ? undefined : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.text();
    if (!res.ok) throw new TmApiError(method, path, res.status, text);
    return (text ? JSON.parse(text) : undefined) as T;
  }

  getTicket(id: string): Promise<Ticket> {
    return this.request<Ticket>("GET", `/tickets/${encodeURIComponent(id)}`);
  }

  listTickets(): Promise<Ticket[]> {
    return this.request<Ticket[]>("GET", "/tickets");
  }

  /** `GET /state`: every ticket, lease, artifact and evidence record, plus the log head. */
  getState(): Promise<Record<string, unknown>> {
    return this.request("GET", "/state");
  }

  /** `POST /tickets` with the same worker defaults `tm ticket new` gives. Returns the new id. */
  async createTicket(
    objective: string,
    options: { kind?: string; priority?: number; parent?: string; actor?: string } = {},
  ): Promise<string> {
    const created = await this.request<{ ticket: Ticket }>("POST", "/tickets", {
      kind: options.kind ?? "work",
      objective,
      priority: options.priority ?? 0,
      ...(options.parent ? { parent: options.parent } : {}),
      actor: options.actor ?? HUMAN,
    });
    return created.ticket.id;
  }

  /** `POST /tickets/:id/transition` with an externally tagged body. */
  transition(id: string, body: Record<string, unknown>): Promise<{ events: unknown[] }> {
    return this.request("POST", `/tickets/${encodeURIComponent(id)}/transition`, body);
  }

  /** Draft -> ready. In-process workers pick a ready ticket up within about 2 seconds. */
  async activate(id: string, actor = HUMAN): Promise<void> {
    await this.transition(id, { activate: { actor } });
  }

  async cancel(id: string, reason?: string, actor = HUMAN): Promise<void> {
    await this.transition(id, { cancel: { reason: reason ?? null, actor } });
  }

  async accept(id: string, note?: string, actor = HUMAN): Promise<void> {
    await this.transition(id, { accept: { note: note ?? null, actor } });
  }

  async reject(id: string, reason: string, actor = HUMAN): Promise<void> {
    await this.transition(id, { reject: { reason, actor } });
  }

  async retry(id: string, guidance?: string, actor = HUMAN): Promise<void> {
    await this.transition(id, { retry: { guidance: guidance ?? null, actor } });
  }

  /** Store a small inline artifact (bytes travel as a JSON array). Returns its id. */
  async createArtifact(
    content: string,
    options: { kind?: string; mediaType?: string; ticket?: string; actor?: string } = {},
  ): Promise<string> {
    const created = await this.request<{ artifact: { id: string } }>("POST", "/artifacts", {
      kind: options.kind ?? "patch",
      media_type: options.mediaType ?? "text/plain",
      bytes: Array.from(new TextEncoder().encode(content)),
      ...(options.ticket ? { ticket: options.ticket } : {}),
      actor: options.actor ?? WORKER,
    });
    return created.artifact.id;
  }

  /**
   * Poll `GET /tickets/:id` until its state is `state` (or one of `state`). Returns the ticket.
   * Throws with the last state seen on timeout.
   */
  async waitForState(
    id: string,
    state: TicketState | TicketState[],
    options: { timeoutMs?: number; intervalMs?: number } = {},
  ): Promise<Ticket> {
    const wanted = Array.isArray(state) ? state : [state];
    const deadline = Date.now() + (options.timeoutMs ?? 30_000);
    let last: Ticket | undefined;
    for (;;) {
      last = await this.getTicket(id);
      if (wanted.includes(last.state)) return last;
      if (Date.now() >= deadline) {
        throw new Error(`${id} is ${last.state}, still not ${wanted.join(" or ")}`);
      }
      await new Promise((r) => setTimeout(r, options.intervalMs ?? 200));
    }
  }

  /**
   * Create and activate a ticket, then wait while the in-process workers fail it. The mock model
   * fails every attempt, so it escalates after 3 attempts (about 6 to 10 seconds). Needs a server
   * started with workers (the default).
   */
  async seedEscalated(objective: string, options: { timeoutMs?: number } = {}): Promise<string> {
    const id = await this.createTicket(objective);
    await this.activate(id);
    await this.waitForState(id, "escalated", { timeoutMs: options.timeoutMs ?? 45_000 });
    return id;
  }

  /**
   * Drive a fresh ticket draft -> ready -> leased -> running -> submitted by hand, as the
   * non-human [`WORKER`], with a stored artifact as its evidence.
   *
   * The lease is taken right after activation, before the in-process scheduler's next tick
   * (every 2 s). If the scheduler wins that race anyway (the lease answers 409), the stolen
   * ticket is cancelled and a new one is seeded, so a rare run leaves an extra cancelled row.
   */
  async seedSubmitted(
    objective: string,
    options: { summary?: string; evidence?: string } = {},
  ): Promise<SeededSubmission> {
    for (let attempt = 1; ; attempt += 1) {
      const id = await this.createTicket(objective);
      await this.activate(id);
      let lease: string;
      try {
        const leased = await this.request<{ lease: { id: string } }>(
          "POST",
          `/tickets/${encodeURIComponent(id)}/lease`,
          { holder: WORKER, ttl_seconds: 3600, actor: WORKER },
        );
        lease = leased.lease.id;
      } catch (error) {
        if (error instanceof TmApiError && error.status === 409 && attempt < 3) {
          await this.cancel(id, "e2e: the scheduler took the seed ticket").catch(() => undefined);
          continue;
        }
        throw error;
      }
      await this.transition(id, { trigger: { trigger: "work_started", actor: WORKER } });
      const artifact = await this.createArtifact(
        options.evidence ?? `diff --git a/e2e b/e2e\n+${objective}\n`,
        { ticket: id },
      );
      await this.transition(id, {
        submit: {
          summary: options.summary ?? `Done: ${objective}`,
          evidence: [artifact],
          actor: WORKER,
        },
      });
      await this.waitForState(id, "submitted", { timeoutMs: 5_000 });
      return { id, artifact, lease };
    }
  }
}
