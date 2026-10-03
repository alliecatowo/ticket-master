// Demo mode: an in-browser stand-in for `tm serve`.
//
// The hosted demo (GitHub Pages) has no backend. `createDemoFetch()` returns a `fetch`
// implementation that answers the same routes the real `tm-server` does (`/health`, `/state`,
// `/tickets`, `/tickets/:id/transition`, `/presence`, and the `/events` SSE stream), backed by a
// small in-memory event log seeded with a believable project. The real client, store and views
// run unmodified on top of it; nothing here talks to the network.
//
// A scripted "worker" keeps the page alive: it heartbeats a lease, narrates what it is doing,
// and walks any ticket you dispatch from ready to submitted, so Accept / Reject / Dispatch all
// have visible consequences.

import type {
  Decision,
  Milestone,
  PresenceSnapshot,
  StateSnapshot,
  Ticket,
  TicketState,
  WireEvent,
} from "../api/types";

const MIN = 60_000;

type Listener = (event: WireEvent) => void;

class DemoProject {
  private seq = 0;
  readonly events: WireEvent[] = [];
  readonly tickets = new Map<string, Ticket>();
  readonly leases = new Map<string, { id: string; ticket: string; holder: string; heartbeat: string }>();
  private readonly listeners = new Set<Listener>();
  private nextTicket = 1;
  private clock: number;

  constructor() {
    this.clock = Date.now() - 5 * 60 * MIN;
    this.seedHistory();
    // Live part of the story runs at wall-clock time from here.
    this.clock = Date.now();
    this.startWorkers();
  }

  // ---- event plumbing -------------------------------------------------------------------

  private tick(minutes: number) {
    this.clock += minutes * MIN;
  }

  private ts(): string {
    return new Date(this.clock).toISOString();
  }

  emit(kind: string, subject: string, payload: Record<string, unknown>, actor: string): WireEvent {
    const event: WireEvent = {
      seq: ++this.seq,
      ts: this.ts(),
      kind,
      subject,
      actor,
      session: null,
      causation: null,
      correlation: null,
      payload,
    };
    this.events.push(event);
    for (const listener of this.listeners) listener(event);
    return event;
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  head(): number {
    return this.seq;
  }

  // ---- ticket helpers (each mutates the projection and emits matching events) -----------

  create(
    title: string,
    actor: string,
    extra: Partial<Ticket> = {},
    id = `T-${this.nextTicket++}`,
  ): Ticket {
    const ts = this.ts();
    const ticket: Ticket = {
      id,
      kind: "work",
      objective: title,
      state: "draft",
      parent: null,
      children: [],
      dependencies: [],
      milestone: null,
      authority: null,
      resources: [],
      executor: { role: "worker" },
      context_refs: [],
      success: [],
      verification: null,
      budget: null,
      retry: { max_attempts: 3, base_delay_seconds: 30, backoff_multiplier: 2, max_delay_seconds: 600 },
      cycle: null,
      attempts: 0,
      failures: [],
      priority: 0,
      created: ts,
      updated: ts,
      ...extra,
    };
    this.tickets.set(id, ticket);
    this.emit("ticket.created", id, { ticket: id, title }, actor);
    this.emit("ticket.updated", id, { ticket: id, fields: { objective: title } }, actor);
    return ticket;
  }

  move(id: string, to: TicketState, actor: string): void {
    const t = this.tickets.get(id);
    if (!t) return;
    const from = t.state;
    t.state = to;
    t.updated = this.ts();
    this.emit("ticket.state_changed", id, { ticket: id, from, to }, actor);
  }

  lease(id: string, holder: string): string {
    const lease = `L-${id.slice(2)}-${this.attempts(id) + 1}`;
    this.move(id, "leased", "system:scheduler");
    this.leases.set(lease, { id: lease, ticket: id, holder, heartbeat: this.ts() });
    this.emit(
      "ticket.leased",
      id,
      { ticket: id, lease, holder, expires_at: new Date(this.clock + 10 * MIN).toISOString() },
      "system:scheduler",
    );
    const t = this.tickets.get(id)!;
    t.attempts += 1;
    this.emit("ticket.updated", id, { ticket: id, fields: { attempts: t.attempts } }, holder);
    this.move(id, "running", holder);
    return lease;
  }

  private attempts(id: string): number {
    return this.tickets.get(id)?.attempts ?? 0;
  }

  heartbeat(lease: string): void {
    const l = this.leases.get(lease);
    if (!l) return;
    l.heartbeat = this.ts();
    this.emit(
      "ticket.heartbeat",
      l.ticket,
      { ticket: l.ticket, lease, holder: l.holder, expires_at: new Date(this.clock + 10 * MIN).toISOString() },
      l.holder,
    );
  }

  step(id: string, text: string, actor: string): void {
    this.emit("goal.step_added", id, { ticket: id, text }, actor);
  }

  run(id: string, command: string, actor: string): void {
    this.emit("command.started", id, { ticket: id, command }, actor);
  }

  submit(id: string, lease: string, summary: string, evidence: { artifact: string; kind: string; summary: string }, actor: string) {
    this.emit("ticket.updated", id, { ticket: id, fields: { evidence_attached: evidence } }, actor);
    this.emit("ticket.submitted", id, { ticket: id, summary }, actor);
    this.move(id, "submitted", actor);
    this.leases.delete(lease);
    this.emit("ticket.lease_released", id, { ticket: id, lease }, actor);
  }

  close(id: string, reason: string, actor: string): void {
    const t = this.tickets.get(id);
    if (!t) return;
    t.state = "closed";
    t.updated = this.ts();
    this.emit("ticket.closed", id, { ticket: id, reason }, actor);
  }

  // ---- the seeded history ---------------------------------------------------------------

  private seedHistory() {
    const human = "human:allie";
    const w1 = "agent:worker-1";
    const w2 = "agent:worker-2";
    const w3 = "agent:worker-3";
    const ms = "M-1";

    // T-1, T-2: finished earlier, accepted by a human.
    for (const [title, summary] of [
      ["Add per-key rate limiting to POST /v1/export", "Token bucket in middleware/ratelimit.ts; 429 + Retry-After; 14 new tests, all green."],
      ["Drop the unused legacy_invoices table and its migration", "Migration 0042 drops the table; no references remain (grep clean); schema snapshot regenerated."],
    ] as const) {
      const t = this.create(title, human, { milestone: ms, priority: 2 });
      this.tick(2);
      this.move(t.id, "ready", human);
      this.tick(3);
      const lease = this.lease(t.id, w1);
      this.run(t.id, "pnpm test --run", w1);
      this.tick(6);
      this.submit(t.id, lease, summary, { artifact: `A-${t.id.slice(2)}`, kind: "test-report", summary: "all checks passed" }, w1);
      this.tick(4);
      this.move(t.id, "verifying", "system:verifier");
      this.tick(2);
      this.emit("ticket.verified", t.id, { ticket: t.id }, "system:verifier");
      this.tick(20);
      this.close(t.id, "accepted", human);
      this.tick(5);
    }

    // T-3: submitted, waiting for review.
    {
      const t = this.create("Fix off-by-one in the pagination cursor (last page repeats its final row)", human, {
        milestone: ms, priority: 3,
      });
      this.move(t.id, "ready", human);
      this.tick(2);
      const lease = this.lease(t.id, w2);
      this.step(t.id, "Reproduce with a 21-row fixture and page size 10", w2);
      this.run(t.id, "pnpm test paginate -t cursor", w2);
      this.tick(7);
      this.step(t.id, "Cursor encoded the last-seen id inclusively; make the comparison strict", w2);
      this.run(t.id, "pnpm test --run", w2);
      this.tick(4);
      this.submit(
        t.id, lease,
        "The cursor compared ids with >= so the boundary row came back twice. Now strict (>), with a regression test for page boundaries. Full suite green (212 tests).",
        { artifact: "A-3", kind: "test-report", summary: "212 passed, 0 failed" }, w2,
      );
      this.tick(6);
    }

    // T-4: submitted, waiting for review.
    {
      const t = this.create("Retry failed webhook deliveries with jittered exponential backoff", human, {
        milestone: ms, priority: 2,
      });
      this.move(t.id, "ready", human);
      this.tick(1);
      const lease = this.lease(t.id, w3);
      this.run(t.id, "pnpm test webhooks", w3);
      this.tick(9);
      this.step(t.id, "Cap at 6 attempts, full jitter, dead-letter after the cap", w3);
      this.tick(5);
      this.submit(
        t.id, lease,
        "Webhook sender retries 5xx and timeouts with full-jitter backoff (1s base, 6 attempts) then dead-letters to the failed_deliveries table. Added a fake-clock test for the schedule.",
        { artifact: "A-4", kind: "test-report", summary: "38 webhook tests passed" }, w3,
      );
      this.tick(4);
    }

    // T-7: escalated, needs a human decision.
    {
      const t = this.create("Upgrade the OpenSSL binding to 0.10.80", human, { priority: 1 }, "T-7");
      this.move(t.id, "ready", human);
      this.tick(1);
      const lease = this.lease(t.id, w1);
      this.run(t.id, "cargo build --release", w1);
      this.tick(5);
      this.leases.delete(lease);
      this.emit("ticket.escalated", t.id, {
        ticket: t.id,
        reason: "The new binding drops the FIPS feature flag we still ship in the enterprise build. Upgrade and lose FIPS, or pin and carry the CVE?",
      }, w1);
      this.tickets.get(t.id)!.state = "escalated";
      this.tick(3);
    }

    // Reserve ids so the numbering reads naturally: T-5 and T-6 are created below.
    // T-5: running now (worker narrates live).
    this.nextTicket = 5;
    this.create("Move the session store from in-process memory to Redis", human, { milestone: ms, priority: 3 });
    this.move("T-5", "ready", human);
    this.tick(1);
    this.create("Document the /v1/export rate limits in the API reference", human, { milestone: ms, priority: 1, dependencies: ["T-5"] });
    this.move("T-6", "blocked", human);
    this.nextTicket = 8;
    this.create("Add a dashboard tile for failed webhook deliveries", human, { priority: 1 });
    this.move("T-8", "ready", human);
    this.create("Audit logging for admin actions", human, { priority: 0 });
    this.create("Speed up the CI cache key for the monorepo", human, { priority: 0, kind: "investigation", executor: { role: "researcher" } });
    this.move("T-10", "ready", human);
    this.nextTicket = 11;
  }

  // ---- scripted workers -----------------------------------------------------------------

  private t5Lease: string | null = null;

  private startWorkers() {
    // T-5 is mid-run when you arrive.
    this.clock = Date.now();
    this.t5Lease = this.lease("T-5", "agent:worker-2");
    this.step("T-5", "Introduce a SessionStore interface; keep MemoryStore as the test double", "agent:worker-2");
    this.run("T-5", "pnpm tsc --noEmit", "agent:worker-2");

    const script: Array<() => void> = [
      () => this.run("T-5", "pnpm test session -t redis", "agent:worker-2"),
      () => this.step("T-5", "Sliding TTL: refresh expiry on read, not just on write", "agent:worker-2"),
      () => this.run("T-5", "pnpm test --run", "agent:worker-2"),
      () => {
        if (this.t5Lease) {
          this.submit(
            "T-5", this.t5Lease,
            "Sessions now live in Redis behind a SessionStore interface (sliding 30 min TTL). MemoryStore remains for tests. Added an integration test against redis-mock; full suite green (219 tests).",
            { artifact: "A-5", kind: "test-report", summary: "219 passed, 0 failed" }, "agent:worker-2",
          );
          this.t5Lease = null;
        }
      },
    ];
    let i = 0;
    setInterval(() => {
      this.clock = Date.now();
      for (const lease of this.leases.keys()) this.heartbeat(lease);
    }, 20_000);
    const next = () => {
      if (i >= script.length) return;
      setTimeout(() => {
        this.clock = Date.now();
        script[i++]();
        next();
      }, 9_000 + i * 2_000);
    };
    next();
  }

  /** Walk a freshly queued ticket through a believable worker run. */
  runWorkerOn(id: string) {
    const holder = "agent:worker-1";
    const steps: Array<() => void> = [
      () => {
        this.clock = Date.now();
        const lease = this.lease(id, holder);
        this.step(id, "Read the objective and locate the relevant modules", holder);
        (this as unknown as { _l: Map<string, string> })._l ??= new Map();
        (this as unknown as { _l: Map<string, string> })._l.set(id, lease);
      },
      () => this.run(id, "rg --files-with-matches . | head -40", holder),
      () => this.step(id, "Sketch the change, then write a failing test first", holder),
      () => this.run(id, "pnpm test --run", holder),
      () => {
        const lease = (this as unknown as { _l: Map<string, string> })._l.get(id)!;
        const t = this.tickets.get(id);
        if (!t || t.state === "cancelled") return;
        this.submit(
          id, lease,
          `Implemented: ${t.objective}. Added a focused test and ran the full suite; everything passes.`,
          { artifact: `A-${id.slice(2)}`, kind: "test-report", summary: "all checks passed" }, holder,
        );
      },
    ];
    steps.forEach((fn, n) => setTimeout(() => {
      const t = this.tickets.get(id);
      if (!t || t.state === "cancelled") return;
      this.clock = Date.now();
      fn();
    }, 2_500 + n * 3_500));
  }

  // ---- read side ------------------------------------------------------------------------

  state(): StateSnapshot {
    const milestones: Milestone[] = [
      {
        id: "M-1",
        title: "Billing API hardening",
        tickets: [...this.tickets.values()].filter((t) => t.milestone === "M-1").map((t) => t.id),
        state: "open",
        closed_by: null,
        assumptions: [],
      },
    ];
    const decisions: Decision[] = [];
    return {
      head: this.seq,
      tickets: [...this.tickets.values()].map((t) => ({ ...t })),
      leases: [...this.leases.values()].map((l) => ({
        id: l.id,
        ticket: l.ticket,
        holder: l.holder,
        authority: null,
        resources: [],
        acquired: l.heartbeat,
        heartbeat: l.heartbeat,
        ttl_seconds: 600,
        epoch: 1,
      })),
      decisions,
      milestones,
      artifacts: [],
      evidence: [],
      budgets: [],
    };
  }

  presence(): PresenceSnapshot {
    const now = new Date().toISOString();
    return {
      participants: [
        { participant: "agent:worker-2", ticket: this.t5Lease ? "T-5" : null, file: this.t5Lease ? "src/session/redisStore.ts" : null, action: this.t5Lease ? "editing" : null, last_seen: now, ttl_seconds: 60 },
        { participant: "human:allie", ticket: null, file: null, action: null, last_seen: now, ttl_seconds: 60 },
      ],
      path_leases: this.t5Lease
        ? [{ lease: this.t5Lease, ticket: "T-5", holder: "agent:worker-2", mode: "write", paths: ["src/session/**"] }]
        : [],
    };
  }

  // ---- write side -----------------------------------------------------------------------

  transition(id: string, body: Record<string, { actor: string; note?: string | null; reason?: string | null; guidance?: string | null }>): WireEvent[] {
    const t = this.tickets.get(id);
    if (!t) throw new DemoError(404, "not_found", `no ticket ${id}`);
    const before = this.events.length;
    const [verb] = Object.keys(body);
    const args = body[verb];
    const actor = args.actor;
    const need = (...states: TicketState[]) => {
      if (!states.includes(t.state)) {
        throw new DemoError(409, "invalid_transition", `invalid transition: ${verb} from ${t.state}`);
      }
    };
    this.clock = Date.now();
    switch (verb) {
      case "activate":
        need("draft");
        this.move(id, "ready", actor);
        this.runWorkerOn(id);
        break;
      case "accept":
        need("submitted");
        this.emit("ticket.verified", id, { ticket: id }, "system:verifier");
        this.close(id, args.note || "accepted", actor);
        // The docs ticket was blocked on the Redis migration.
        if (id === "T-5" && this.tickets.get("T-6")?.state === "blocked") this.move("T-6", "ready", "system:scheduler");
        break;
      case "reject":
        need("submitted");
        this.emit("ticket.verification_failed", id, { ticket: id, reason: args.reason ?? "" }, actor);
        this.move(id, "rework", actor);
        break;
      case "retry":
        need("escalated", "rework");
        this.move(id, "ready", actor);
        this.runWorkerOn(id);
        break;
      case "cancel":
        if (t.state === "closed" || t.state === "cancelled") need("draft");
        t.state = "cancelled";
        t.updated = this.ts();
        this.emit("ticket.cancelled", id, { ticket: id, reason: args.reason ?? "" }, actor);
        break;
      default:
        throw new DemoError(422, "unprocessable", `unknown transition ${verb}`);
    }
    return this.events.slice(before);
  }
}

class DemoError extends Error {
  constructor(readonly status: number, readonly code: string, message: string) {
    super(message);
  }
}

const json = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

/** A `fetch` that serves the `tm-server` routes the web client uses, entirely in memory. */
export function createDemoFetch(): typeof fetch {
  const project = new DemoProject();

  return async (input, init) => {
    const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.href : input.url, "http://demo.local");
    const method = (init?.method ?? "GET").toUpperCase();
    const path = url.pathname;
    try {
      if (path === "/health") return json(200, { status: "ok" });
      if (path === "/state") return json(200, project.state());
      if (path === "/presence") return json(200, project.presence());
      if (path === "/tickets" && method === "GET") return json(200, project.state().tickets);
      if (path === "/tickets" && method === "POST") {
        const req = JSON.parse(String(init?.body ?? "{}")) as { objective: string; actor: string };
        const before = project.events.length;
        const ticket = project.create(req.objective, req.actor);
        return json(201, { ticket: { ...ticket }, events: project.events.slice(before) });
      }
      const transition = path.match(/^\/tickets\/([^/]+)\/transition$/);
      if (transition && method === "POST") {
        const events = project.transition(decodeURIComponent(transition[1]), JSON.parse(String(init?.body ?? "{}")));
        return json(200, { events });
      }
      const one = path.match(/^\/tickets\/([^/]+)$/);
      if (one) {
        const t = project.tickets.get(decodeURIComponent(one[1]));
        return t ? json(200, t) : json(404, { error: "not_found", message: "not found" });
      }
      if (path === "/events") return sse(project, Number(url.searchParams.get("from") ?? 0), init?.signal ?? undefined);
      return json(404, { error: "not_found", message: `no route ${path}` });
    } catch (err) {
      if (err instanceof DemoError) return json(err.status, { error: err.code, message: err.message });
      throw err;
    }
  };
}

function sse(project: DemoProject, from: number, signal?: AbortSignal): Response {
  const encoder = new TextEncoder();
  let unsubscribe = () => {};
  const frame = (e: WireEvent) => encoder.encode(`id: ${e.seq}\nevent: ${e.kind}\ndata: ${JSON.stringify(e)}\n\n`);
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const e of project.events) if (e.seq > from) controller.enqueue(frame(e));
      unsubscribe = project.subscribe((e) => {
        try {
          controller.enqueue(frame(e));
        } catch {
          unsubscribe();
        }
      });
      signal?.addEventListener("abort", () => {
        unsubscribe();
        try {
          controller.close();
        } catch {
          // already closed
        }
      });
    },
    cancel() {
      unsubscribe();
    },
  });
  return new Response(stream, { status: 200, headers: { "content-type": "text/event-stream" } });
}
