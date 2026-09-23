import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { TicketmasterProvider } from "./hooks/useTicketmaster";
import type { StateSnapshot } from "./api/types";
import { stateSnapshot, ticket } from "./test/fixtures";

let snapshot: StateSnapshot;
let posts: Array<{ url: string; body: unknown }>;
/** What a transition POST answers; tests override it to exercise error paths. */
let transitionResponse: () => Response;

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

beforeEach(() => {
  snapshot = stateSnapshot({
    head: 0,
    tickets: [
      ticket({ id: "T-1", objective: "render the status view", state: "ready" }),
      ticket({ id: "T-2", objective: "add a login page", state: "submitted", attempts: 1 }),
      ticket({
        id: "T-3",
        objective: "fix the flaky test",
        state: "escalated",
        attempts: 3,
        failures: [{ class: "other", detail: "model refused", at: "2026-01-01T00:00:00Z", attempt: 3 }],
      }),
      ticket({ id: "T-4", objective: "write the docs", state: "closed" }),
    ],
  });
  posts = [];
  transitionResponse = () => json({ events: [] });
  localStorage.clear();
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (init?.method === "POST") {
        posts.push({ url, body: JSON.parse(String(init.body)) });
        if (url === "/tickets") return json({ ticket: ticket({ id: "T-5", state: "draft" }), events: [] }, 201);
        return transitionResponse();
      }
      if (url.startsWith("/state")) return json(snapshot);
      // An open stream with nothing new on it.
      if (url.startsWith("/events")) return new Response(new ReadableStream(), { status: 200 });
      if (url.startsWith("/presence")) return json({ participants: [], path_leases: [] });
      return json({});
    }),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function renderAt(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <TicketmasterProvider>
        <App />
      </TicketmasterProvider>
    </MemoryRouter>,
  );
}

describe("App", () => {
  it("opens on the tickets home, grouped like the TUI's agent view", async () => {
    renderAt("/");
    await waitFor(() => expect(screen.getByTestId("connection-status")).toHaveTextContent("Live"));
    const needs = await screen.findByTestId("group-needs_input");
    expect(within(needs).getByText("T-3")).toBeInTheDocument();
    expect(within(needs).getByText("gave up after 3 attempts: model refused")).toBeInTheDocument();
    expect(within(screen.getByTestId("group-review")).getByText("T-2")).toBeInTheDocument();
    expect(within(screen.getByTestId("group-queued")).getByText("T-1")).toBeInTheDocument();
    expect(within(screen.getByTestId("group-completed")).getByText("T-4")).toBeInTheDocument();
    expect(screen.queryByTestId("group-working")).toBeNull();
    expect(screen.getByTestId("review-count")).toHaveTextContent("1");
  });

  it("dispatches a task: creates it as human:web, then queues it", async () => {
    renderAt("/");
    await screen.findByTestId("group-queued");
    const input = screen.getByTestId("dispatch-input");
    fireEvent.change(input, { target: { value: "Add a health check" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() => expect(posts).toHaveLength(2));
    expect(posts[0]).toEqual({
      url: "/tickets",
      body: { kind: "work", objective: "Add a health check", actor: "human:web" },
    });
    expect(posts[1]).toEqual({ url: "/tickets/T-5/transition", body: { activate: { actor: "human:web" } } });
  });

  it("shows an empty state when the project has no tickets", async () => {
    snapshot = stateSnapshot({ tickets: [] });
    renderAt("/");
    expect(await screen.findByText("Nothing here yet")).toBeInTheDocument();
  });

  it("offers Retry and Retry with guidance on an escalated ticket, as the named human", async () => {
    localStorage.setItem("tm.web.actor", "allie");
    renderAt("/ticket/T-3");
    fireEvent.click(await screen.findByTestId("action-retry_with_guidance"));
    const field = screen.getByLabelText("Guidance for the next attempt");
    const send = screen.getByRole("button", { name: "Retry with guidance" });
    expect(send).toBeDisabled();
    fireEvent.change(field, { target: { value: "use the retry helper" } });
    fireEvent.click(send);
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0]).toEqual({
      url: "/tickets/T-3/transition",
      body: { retry: { guidance: "use the retry helper", actor: "human:allie" } },
    });
  });

  it("asks before cancelling", async () => {
    renderAt("/ticket/T-1");
    fireEvent.click(await screen.findByTestId("action-cancel"));
    expect(posts).toHaveLength(0);
    fireEvent.click(screen.getByRole("button", { name: "Cancel T-1" }));
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].body).toEqual({ cancel: { reason: null, actor: "human:web" } });
  });

  it("offers nothing but the facts on a closed ticket", async () => {
    renderAt("/ticket/T-4");
    await screen.findByText("Objective");
    expect(screen.queryByTestId("action-cancel")).toBeNull();
    expect(screen.queryByTestId("action-accept")).toBeNull();
  });

  it("reviews inline: accept sends accept, reject needs a reason", async () => {
    renderAt("/review");
    const card = await screen.findByTestId("review-T-2");
    fireEvent.click(within(card).getByTestId("action-reject"));
    const send = within(card).getByRole("button", { name: "Send back" });
    expect(send).toBeDisabled();
    fireEvent.change(within(card).getByLabelText("What should the next attempt fix?"), {
      target: { value: "no tests" },
    });
    fireEvent.click(send);
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].body).toEqual({ reject: { reason: "no tests", actor: "human:web" } });

    fireEvent.click(await within(card).findByTestId("action-accept"));
    await waitFor(() => expect(posts).toHaveLength(2));
    expect(posts[1].body).toEqual({ accept: { note: null, actor: "human:web" } });
  });

  it("shows the server's refusal next to the action", async () => {
    transitionResponse = () =>
      json({ error: "authority_denied", message: "authority denied: only a human can accept a submission" }, 403);
    renderAt("/review");
    const card = await screen.findByTestId("review-T-2");
    fireEvent.click(within(card).getByTestId("action-accept"));
    expect(await within(card).findByRole("alert")).toHaveTextContent(
      "Not allowed: only a human can accept a submission",
    );
  });

  it("renders a stub notice for an unbuilt view", async () => {
    renderAt("/graph");
    expect(screen.getByTestId("not-built")).toBeInTheDocument();
    expect(screen.getByText(/not built yet/i)).toBeInTheDocument();
  });

  it("still renders Status and Backlog", async () => {
    renderAt("/backlog");
    await waitFor(() => expect(screen.getByTestId("backlog-view")).toBeInTheDocument());
    await waitFor(() => expect(screen.getByText("T-1")).toBeInTheDocument());
  });
});
