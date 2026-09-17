import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { TicketmasterProvider } from "./hooks/useTicketmaster";
import type { StateSnapshot } from "./api/types";

const snapshot: StateSnapshot = {
  head: 3,
  tickets: [
    {
      id: "T-1",
      kind: "work",
      objective: "render the status view",
      state: "ready",
      parent: null,
      children: [],
      dependencies: [],
      milestone: null,
      authority: null,
      resources: [],
      executor: {},
      context_refs: [],
      success: [],
      verification: null,
      budget: null,
      retry: null,
      cycle: null,
      attempts: 0,
      failures: [],
      priority: 0,
      created: "2026-01-01T00:00:00Z",
      updated: "2026-01-01T00:00:00Z",
    },
  ],
  leases: [],
  decisions: [],
  milestones: [],
  artifacts: [],
  evidence: [],
  budgets: [],
};

function emptyBodyReader() {
  return {
    getReader: () => ({
      read: async () => ({ done: true, value: undefined }),
      releaseLock: () => {},
    }),
  };
}

function jsonResponse(body: unknown) {
  return { ok: true, status: 200, text: async () => JSON.stringify(body) };
}

beforeEach(() => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/state")) return jsonResponse(snapshot);
      if (url.includes("/events")) return { ...jsonResponse(""), body: emptyBodyReader() };
      if (url.includes("/presence")) return jsonResponse({ participants: [], path_leases: [] });
      return jsonResponse({});
    }),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("App", () => {
  it("renders the nav and the Status view by default, seeded from GET /state", async () => {
    render(
      <MemoryRouter initialEntries={["/status"]}>
        <TicketmasterProvider>
          <App />
        </TicketmasterProvider>
      </MemoryRouter>,
    );

    expect(screen.getByText("Ticketmaster")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByTestId("connection-status")).toHaveTextContent("live"));
    expect(screen.getByTestId("status-view")).toBeInTheDocument();
  });

  it("renders the Backlog view with the seeded ticket", async () => {
    render(
      <MemoryRouter initialEntries={["/backlog"]}>
        <TicketmasterProvider>
          <App />
        </TicketmasterProvider>
      </MemoryRouter>,
    );

    await waitFor(() => expect(screen.getByTestId("backlog-view")).toBeInTheDocument());
    await waitFor(() => expect(screen.getByText("T-1")).toBeInTheDocument());
  });

  it("renders a stub notice for an unbuilt view", async () => {
    render(
      <MemoryRouter initialEntries={["/graph"]}>
        <TicketmasterProvider>
          <App />
        </TicketmasterProvider>
      </MemoryRouter>,
    );

    expect(screen.getByTestId("not-built")).toBeInTheDocument();
    expect(screen.getByText(/not built yet/i)).toBeInTheDocument();
  });
});
