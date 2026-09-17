import { describe, expect, it, vi } from "vitest";
import { TicketmasterClient } from "../src/client.js";
import { TicketmasterApiError } from "../src/domain.js";

function jsonResponse(status: number, body: unknown): Response {
  return {
    status,
    ok: status >= 200 && status < 300,
    text: async () => JSON.stringify(body),
  } as unknown as Response;
}

describe("TicketmasterClient", () => {
  it("sends a JSON POST with a bearer token and decodes the response", async () => {
    const fetchMock = vi.fn(async (url: URL, init?: RequestInit) => {
      expect(url.toString()).toBe("http://127.0.0.1:4173/tickets");
      expect(init?.method).toBe("POST");
      expect((init?.headers as Record<string, string>).Authorization).toBe("Bearer secret");
      expect(JSON.parse(init?.body as string)).toMatchObject({ objective: "Ship it" });
      return jsonResponse(201, {
        ticket: { id: "T-1", objective: "Ship it" },
        events: [],
      });
    });

    const client = new TicketmasterClient({
      baseUrl: "http://127.0.0.1:4173",
      token: "secret",
      fetch: fetchMock as unknown as typeof fetch,
    });

    const result = await client.createTicket({
      kind: "work",
      objective: "Ship it",
      executor: { role: "coder_fast", human_required: false, min_capability: "any" },
      verification: "none",
      retry: { max_attempts: 1, base_delay_seconds: 0, backoff_multiplier: 1, max_delay_seconds: 0 },
      actor: "P-1",
    });

    expect(result.ticket.id).toBe("T-1");
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("throws TicketmasterApiError with the server's error code/message on a non-2xx response", async () => {
    const fetchMock = vi.fn(async () =>
      jsonResponse(404, { error: "not_found", message: "ticket T-9 not found" }),
    );
    const client = new TicketmasterClient({
      baseUrl: "http://127.0.0.1:4173",
      fetch: fetchMock as unknown as typeof fetch,
    });

    await expect(client.getTicket("T-9")).rejects.toMatchObject(
      new TicketmasterApiError(404, { error: "not_found", message: "ticket T-9 not found" }),
    );
  });

  it("builds every REST route relative to baseUrl without a network call", async () => {
    const seen: string[] = [];
    const fetchMock = vi.fn(async (url: URL) => {
      seen.push(url.pathname + url.search);
      return jsonResponse(200, {});
    });
    const client = new TicketmasterClient({
      baseUrl: "http://127.0.0.1:4173",
      fetch: fetchMock as unknown as typeof fetch,
    });

    await client.health();
    await client.getState();
    await client.listTickets();
    await client.getGraph();
    await client.listDecisions();
    await client.listMilestones();
    await client.listArtifacts();
    await client.getPresence();
    await client.getMetrics();

    expect(seen).toEqual([
      "/health",
      "/state",
      "/tickets",
      "/graph",
      "/decisions",
      "/milestones",
      "/artifacts",
      "/presence",
      "/metrics",
    ]);
  });
});
