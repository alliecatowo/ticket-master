// `TicketmasterClient`: typed REST bindings for every route `tm-server/src/routes.rs::router`
// exposes, plus `subscribe(fromSeq)` (SPEC.md §18.1). Nothing else in a Ticketmaster surface
// should hand-roll `fetch` against the API — this is the one place that does.

import type {
  AcquireLeaseInput,
  ActorOnlyInput,
  Artifact,
  ArtifactId,
  AttachEvidenceInput,
  CreateApprovalInput,
  CreateArtifactInput,
  CreateMilestoneInput,
  CreateSessionInput,
  CreateTicketInput,
  Decision,
  DecisionId,
  DecisionInput,
  DecideApprovalInput,
  ErrorBody,
  Graph,
  Lease,
  LeaseId,
  MetricsSnapshot,
  Milestone,
  MilestoneId,
  PresenceSnapshot,
  PresenceUpdateInput,
  ProjectState,
  SessionId,
  Ticket,
  TicketId,
  TransitionCommand,
  UpdateTicketInput,
  WireEvent,
} from "./domain.js";
import { TicketmasterApiError } from "./domain.js";
import { subscribeToEvents, type ReconnectOptions } from "./sse.js";

export interface TicketmasterClientOptions {
  /** Base URL of the server, e.g. `http://127.0.0.1:4173`. */
  baseUrl: string;
  /** Bearer token; required when the server is bound non-loopback (SPEC.md §14). */
  token?: string;
  /** Injectable for tests; defaults to the global `fetch`. */
  fetch?: typeof fetch;
}

export class TicketmasterClient {
  private readonly baseUrl: string;
  private readonly token: string | undefined;
  private readonly fetchImpl: typeof fetch;

  constructor(opts: TicketmasterClientOptions) {
    this.baseUrl = opts.baseUrl;
    this.token = opts.token;
    this.fetchImpl = opts.fetch ?? fetch;
  }

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const url = new URL(path, this.baseUrl);
    const headers: Record<string, string> = {};
    if (this.token) headers.Authorization = `Bearer ${this.token}`;
    if (body !== undefined) headers["Content-Type"] = "application/json";

    const res = await this.fetchImpl(url, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });

    if (res.status === 204) return undefined as T;

    const text = await res.text();
    const parsed: unknown = text.length > 0 ? JSON.parse(text) : undefined;

    if (!res.ok) {
      throw new TicketmasterApiError(res.status, parsed as ErrorBody);
    }
    return parsed as T;
  }

  // -- misc --------------------------------------------------------------------------------

  health(): Promise<{ status: string }> {
    return this.request("GET", "/health");
  }

  getState(): Promise<ProjectState> {
    return this.request("GET", "/state");
  }

  getSchema(): Promise<Record<string, unknown>> {
    return this.request("GET", "/schema");
  }

  getGraph(): Promise<Graph> {
    return this.request("GET", "/graph");
  }

  getMetrics(): Promise<MetricsSnapshot> {
    return this.request("GET", "/metrics");
  }

  getProviders(): Promise<{ candidates: unknown[] }> {
    return this.request("GET", "/providers");
  }

  getHarness(): Promise<{ current: unknown }> {
    return this.request("GET", "/harness");
  }

  // -- tickets -------------------------------------------------------------------------------

  listTickets(): Promise<Ticket[]> {
    return this.request("GET", "/tickets");
  }

  createTicket(input: CreateTicketInput): Promise<{ ticket: Ticket; events: WireEvent[] }> {
    return this.request("POST", "/tickets", input);
  }

  getTicket(id: TicketId): Promise<Ticket> {
    return this.request("GET", `/tickets/${encodeURIComponent(id)}`);
  }

  updateTicket(id: TicketId, input: UpdateTicketInput): Promise<Ticket> {
    return this.request("PATCH", `/tickets/${encodeURIComponent(id)}`, input);
  }

  transition(id: TicketId, command: TransitionCommand): Promise<{ events: WireEvent[] }> {
    return this.request("POST", `/tickets/${encodeURIComponent(id)}/transition`, command);
  }

  acquireLease(id: TicketId, input: AcquireLeaseInput): Promise<{ lease: Lease; events: WireEvent[] }> {
    return this.request("POST", `/tickets/${encodeURIComponent(id)}/lease`, input);
  }

  attachEvidence(id: TicketId, input: AttachEvidenceInput): Promise<{ events: WireEvent[] }> {
    return this.request("POST", `/tickets/${encodeURIComponent(id)}/evidence`, input);
  }

  // -- leases --------------------------------------------------------------------------------

  heartbeatLease(id: LeaseId, actor: ActorOnlyInput): Promise<{ events: WireEvent[] }> {
    return this.request("POST", `/leases/${encodeURIComponent(id)}/heartbeat`, actor);
  }

  releaseLease(id: LeaseId, actor: ActorOnlyInput): Promise<{ events: WireEvent[] }> {
    return this.request("POST", `/leases/${encodeURIComponent(id)}/release`, actor);
  }

  // -- decisions -----------------------------------------------------------------------------

  listDecisions(): Promise<Decision[]> {
    return this.request("GET", "/decisions");
  }

  createDecision(input: DecisionInput): Promise<{ decision: Decision; events: WireEvent[] }> {
    return this.request("POST", "/decisions", input);
  }

  supersedeDecision(
    id: DecisionId,
    input: DecisionInput,
  ): Promise<{ decision: Decision; events: WireEvent[] }> {
    return this.request("POST", `/decisions/${encodeURIComponent(id)}/supersede`, input);
  }

  // -- milestones ----------------------------------------------------------------------------

  listMilestones(): Promise<Milestone[]> {
    return this.request("GET", "/milestones");
  }

  createMilestone(
    input: CreateMilestoneInput,
  ): Promise<{ milestone: Milestone; events: WireEvent[] }> {
    return this.request("POST", "/milestones", input);
  }

  closeMilestone(id: MilestoneId, actor: ActorOnlyInput): Promise<{ events: WireEvent[] }> {
    return this.request("POST", `/milestones/${encodeURIComponent(id)}/close`, actor);
  }

  reopenMilestone(id: MilestoneId, actor: ActorOnlyInput): Promise<{ events: WireEvent[] }> {
    return this.request("POST", `/milestones/${encodeURIComponent(id)}/reopen`, actor);
  }

  // -- artifacts -----------------------------------------------------------------------------

  listArtifacts(): Promise<Artifact[]> {
    return this.request("GET", "/artifacts");
  }

  getArtifact(id: ArtifactId): Promise<Artifact> {
    return this.request("GET", `/artifacts/${encodeURIComponent(id)}`);
  }

  createArtifact(input: CreateArtifactInput): Promise<{ artifact: Artifact; events: WireEvent[] }> {
    return this.request("POST", "/artifacts", input);
  }

  // -- approvals -------------------------------------------------------------------------------

  listApprovals(): Promise<unknown[]> {
    return this.request("GET", "/approvals");
  }

  /**
   * Opens an approval request. Per SPEC.md §14 this call blocks (the request itself is the
   * rendezvous) until a decision is made via {@link decideApproval} on another connection.
   */
  createApproval(input: CreateApprovalInput): Promise<{ id: string; decision: unknown }> {
    return this.request("POST", "/approvals", input);
  }

  getApproval(id: string): Promise<unknown> {
    return this.request("GET", `/approvals/${encodeURIComponent(id)}`);
  }

  decideApproval(id: string, input: DecideApprovalInput): Promise<{ id: string; status: string }> {
    return this.request("POST", `/approvals/${encodeURIComponent(id)}/decide`, input);
  }

  // -- sessions / presence ---------------------------------------------------------------------

  createSession(input: CreateSessionInput = {}): Promise<{ session: string; label: string | null }> {
    return this.request("POST", "/sessions", input);
  }

  deleteSession(id: SessionId): Promise<void> {
    return this.request("DELETE", `/sessions/${encodeURIComponent(id)}`);
  }

  updatePresence(id: SessionId, input: PresenceUpdateInput): Promise<{ participant: string }> {
    return this.request("POST", `/sessions/${encodeURIComponent(id)}/presence`, input);
  }

  getPresence(): Promise<PresenceSnapshot> {
    return this.request("GET", "/presence");
  }

  // -- events --------------------------------------------------------------------------------

  /**
   * `GET /events?from=<seq>` as a resumable async iterable: every event with `seq > fromSeq`,
   * exactly once, in order — reconnecting with `Last-Event-ID`/`from=<lastSeq>` across a
   * dropped connection, never dropping or duplicating (SPEC.md §18.1).
   */
  subscribe(fromSeq: number, opts: Partial<ReconnectOptions> = {}): AsyncIterable<WireEvent> {
    return {
      [Symbol.asyncIterator]: () =>
        subscribeToEvents(fromSeq, {
          baseUrl: this.baseUrl,
          token: this.token,
          fetch: this.fetchImpl,
          ...opts,
        })[Symbol.asyncIterator](),
    };
  }
}
