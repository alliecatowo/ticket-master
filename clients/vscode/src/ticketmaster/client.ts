/**
 * Minimal REST client for `tm-server` (SPEC.md section 14).
 *
 * IMPORTANT: `clients/ts/` (the shared `@ticketmaster/client` package
 * described in SPEC.md 18.1) does not exist in this repository yet. SPEC.md
 * 18.4 says the VS Code extension must consume that package and never
 * hand-roll fetch. Since the package it should consume hasn't been built,
 * this file is a deliberately narrow, single-purpose stand-in: every network
 * call the extension makes goes through this one module (never scattered
 * `fetch` calls elsewhere), using only the REST surface documented in
 * SPEC.md 14. It does not include the generated types, `subscribe`
 * reconnect-with-`Last-Event-ID` exactly-once guarantee, or the
 * `ProjectStore` materialized view that SPEC.md 18.1 specifies for the real
 * package.
 *
 * When `clients/ts/` lands, delete this file and `./types.ts` and import
 * `TicketmasterClient` / `ProjectStore` from `@ticketmaster/client` instead.
 * This file is intentionally not unit-tested here: it is thin, host/network
 * dependent glue, not the pure logic this package's tests target (see
 * README.md).
 */
import type { Decision, Milestone, ProjectSnapshot, Ticket } from "./types";

export interface TicketmasterClientOptions {
  baseUrl: string;
  token?: string;
}

export interface SubmitEvidence {
  kind: "TestRun" | "Diff" | "CommandOutput" | "Review" | "HumanAttestation";
  summary: string;
  artifact?: string; // Artifact id, if already uploaded
}

export interface DecisionDraft {
  subject: string;
  decision: string;
  reason: string;
  affectedTickets?: string[];
  affectedDocs?: string[];
}

export class TicketmasterApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly body: string,
  ) {
    super(`tm-server responded ${status}: ${body}`);
  }
}

export class TicketmasterClient {
  constructor(private readonly opts: TicketmasterClientOptions) {}

  private async request<T>(path: string, init?: RequestInit): Promise<T> {
    const headers: Record<string, string> = {
      "content-type": "application/json",
      ...(init?.headers as Record<string, string> | undefined),
    };
    if (this.opts.token) {
      headers.authorization = `Bearer ${this.opts.token}`;
    }
    const res = await fetch(`${this.opts.baseUrl}${path}`, {
      ...init,
      headers,
    });
    if (!res.ok) {
      throw new TicketmasterApiError(res.status, await res.text());
    }
    if (res.status === 204) {
      return undefined as T;
    }
    return (await res.json()) as T;
  }

  getState(): Promise<ProjectSnapshot> {
    return this.request<ProjectSnapshot>("/state");
  }

  listTickets(): Promise<Ticket[]> {
    return this.request<Ticket[]>("/tickets");
  }

  getTicket(id: string): Promise<Ticket> {
    return this.request<Ticket>(`/tickets/${encodeURIComponent(id)}`);
  }

  listMilestones(): Promise<Milestone[]> {
    return this.request<Milestone[]>("/milestones");
  }

  listDecisions(): Promise<Decision[]> {
    return this.request<Decision[]>("/decisions");
  }

  leaseTicket(
    id: string,
    body: { holder: string; ttlSeconds: number },
  ): Promise<void> {
    return this.request(`/tickets/${encodeURIComponent(id)}/lease`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  submit(id: string, evidence: SubmitEvidence[]): Promise<void> {
    return this.request(`/tickets/${encodeURIComponent(id)}/transition`, {
      method: "POST",
      body: JSON.stringify({ trigger: "Submit", evidence }),
    });
  }

  recordDecision(draft: DecisionDraft): Promise<Decision> {
    return this.request<Decision>("/decisions", {
      method: "POST",
      body: JSON.stringify(draft),
    });
  }
}
