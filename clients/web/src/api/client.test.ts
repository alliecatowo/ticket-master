import { describe, expect, it, vi } from "vitest";
import { describeError, HttpError, parseSseFrame, TicketmasterClient, transitionBody } from "./client";
import { ticket } from "../test/fixtures";

const ACTOR = "human:allie";

describe("transitionBody", () => {
  it("queues a draft with an externally tagged activate", () => {
    expect(transitionBody({ type: "queue" }, ACTOR)).toEqual({ activate: { actor: ACTOR } });
  });

  it("accepts with a null note unless one is given", () => {
    expect(transitionBody({ type: "accept" }, ACTOR)).toEqual({ accept: { note: null, actor: ACTOR } });
    expect(transitionBody({ type: "accept", note: "  ship it " }, ACTOR)).toEqual({
      accept: { note: "ship it", actor: ACTOR },
    });
  });

  it("rejects with a trimmed reason, and refuses a blank one", () => {
    expect(transitionBody({ type: "reject", reason: "  no tests\n" }, ACTOR)).toEqual({
      reject: { reason: "no tests", actor: ACTOR },
    });
    expect(() => transitionBody({ type: "reject", reason: "   " }, ACTOR)).toThrow(/reason/);
  });

  it("retries with null guidance, or the trimmed guidance", () => {
    expect(transitionBody({ type: "retry" }, ACTOR)).toEqual({ retry: { guidance: null, actor: ACTOR } });
    expect(transitionBody({ type: "retry", guidance: "" }, ACTOR)).toEqual({
      retry: { guidance: null, actor: ACTOR },
    });
    expect(transitionBody({ type: "retry", guidance: " use the helper " }, ACTOR)).toEqual({
      retry: { guidance: "use the helper", actor: ACTOR },
    });
  });

  it("cancels with an optional reason", () => {
    expect(transitionBody({ type: "cancel" }, ACTOR)).toEqual({ cancel: { reason: null, actor: ACTOR } });
    expect(transitionBody({ type: "cancel", reason: "dup" }, ACTOR)).toEqual({
      cancel: { reason: "dup", actor: ACTOR },
    });
  });
});

function okJson(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

describe("TicketmasterClient", () => {
  it("sends every mutation as human:<handle>, following handle changes", async () => {
    const fetchImpl = vi.fn(async () => okJson({ events: [] }));
    const client = new TicketmasterClient({ fetchImpl, handle: "allie" });
    await client.act("T-3", { type: "accept" });
    client.handle = "  human:Sam Jones ";
    await client.act("T-3", { type: "retry", guidance: "again" });

    const calls = fetchImpl.mock.calls as unknown as Array<[string, RequestInit]>;
    const bodies = calls.map(([, init]) => JSON.parse(String(init.body)));
    expect(calls[0][0]).toBe("/tickets/T-3/transition");
    expect(bodies[0]).toEqual({ accept: { note: null, actor: "human:allie" } });
    expect(bodies[1]).toEqual({ retry: { guidance: "again", actor: "human:Sam-Jones" } });
  });

  it("creates with only kind, objective and actor, then activates (dispatch)", async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) =>
      String(url) === "/tickets"
        ? okJson({ ticket: ticket({ id: "T-9", state: "draft" }), events: [] }, 201)
        : okJson({ events: [] }),
    );
    const client = new TicketmasterClient({ fetchImpl });
    const result = await client.dispatch("  fix the flaky test ");
    expect(result).toMatchObject({ queued: true, ticket: { id: "T-9" } });
    const [create, activate] = fetchImpl.mock.calls as unknown as Array<[string, RequestInit]>;
    expect(create[0]).toBe("/tickets");
    expect(JSON.parse(String(create[1].body))).toEqual({
      kind: "work",
      objective: "fix the flaky test",
      actor: "human:web",
    });
    expect(activate[0]).toBe("/tickets/T-9/transition");
    expect(JSON.parse(String(activate[1].body))).toEqual({ activate: { actor: "human:web" } });
  });

  it("reports a created-but-not-queued ticket instead of losing it", async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) =>
      String(url) === "/tickets"
        ? okJson({ ticket: ticket({ id: "T-9", state: "draft" }), events: [] }, 201)
        : okJson({ error: "invalid_transition", message: "invalid transition: nope" }, 409),
    );
    const result = await new TicketmasterClient({ fetchImpl }).dispatch("x");
    expect(result.queued).toBe(false);
    expect(result.ticket.id).toBe("T-9");
    expect(result.queueError).toBe("Can't do that now: nope");
  });

  it("refuses an empty objective without calling the server", async () => {
    const fetchImpl = vi.fn();
    await expect(new TicketmasterClient({ fetchImpl }).dispatch("  ")).rejects.toThrow();
    expect(fetchImpl).not.toHaveBeenCalled();
  });

  it("parses SSE frames split across chunks", async () => {
    const frames = [
      'id: 1\nevent: ticket.created\ndata: {"seq":1,"kind":"ticket.created"}\n\nid: 2\nev',
      'ent: ticket.updated\ndata: {"seq":2,"kind":"ticket.updated"}\r\n\r\n',
    ];
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        for (const f of frames) controller.enqueue(new TextEncoder().encode(f));
        controller.close();
      },
    });
    const fetchImpl = vi.fn(async (_url: RequestInfo | URL) => new Response(body, { status: 200 }));
    const seen: number[] = [];
    for await (const e of new TicketmasterClient({ fetchImpl }).streamEvents(0)) seen.push(e.seq);
    expect(seen).toEqual([1, 2]);
    expect(fetchImpl.mock.calls[0][0]).toBe("/events?from=0");
  });

  it("ignores comment and malformed frames", () => {
    expect(parseSseFrame(": keep-alive")).toBeNull();
    expect(parseSseFrame("data: {not json")).toBeNull();
  });
});

describe("describeError", () => {
  it("explains a 403 from a non-human actor", () => {
    const err = new HttpError(
      403,
      JSON.stringify({ error: "authority_denied", message: "authority denied: only a human can accept T-3" }),
    );
    expect(err.code).toBe("authority_denied");
    expect(describeError(err)).toBe("Not allowed: only a human can accept T-3");
  });

  it("explains a 409 as a state that moved on", () => {
    const err = new HttpError(409, JSON.stringify({ error: "invalid_transition", message: "invalid transition: closed" }));
    expect(describeError(err)).toBe("Can't do that now: closed");
  });

  it("passes a plain-text rejection through", () => {
    const err = new HttpError(422, "Failed to deserialize the JSON body into the target type: missing field `reason`");
    expect(err.code).toBeNull();
    expect(describeError(err)).toMatch(/missing field `reason`/);
  });

  it("says the server is unreachable on a network error", () => {
    expect(describeError(new TypeError("Failed to fetch"))).toMatch(/Couldn't reach tm serve/);
  });
});
