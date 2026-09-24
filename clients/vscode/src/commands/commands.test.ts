import type { TicketmasterClient } from "@ticketmaster/client";
import { beforeEach, describe, expect, it, vi } from "vitest";

// `commands.ts` imports `vscode`, which is only available inside a real
// extension host (see `../../vitest.config.ts`'s note on host-integration
// files). Mock just enough of the API surface `registerCommands` touches so
// this suite can capture and invoke the registered command callbacks
// directly, and assert on the exact request bodies they send through the
// mocked `TicketmasterClient` — the wire-shape bugs this task fixes.
const mocks = vi.hoisted(() => ({
  registerCommand: vi.fn(),
  showInputBox: vi.fn(),
  showQuickPick: vi.fn(),
  showInformationMessage: vi.fn(),
  showWarningMessage: vi.fn(),
}));

vi.mock("vscode", () => ({
  commands: { registerCommand: mocks.registerCommand },
  window: {
    showInputBox: mocks.showInputBox,
    showQuickPick: mocks.showQuickPick,
    showInformationMessage: mocks.showInformationMessage,
    showWarningMessage: mocks.showWarningMessage,
    activeTextEditor: undefined,
    terminals: [],
    createTerminal: vi.fn(),
  },
  workspace: {
    workspaceFolders: undefined,
    asRelativePath: vi.fn((uri: unknown) => String(uri)),
    openTextDocument: vi.fn(),
  },
}));

const { registerCommands } = await import("./commands");
type CommandDeps = Parameters<typeof registerCommands>[1];

function callbackFor(commandId: string): (...args: unknown[]) => unknown {
  const call = mocks.registerCommand.mock.calls.find(
    (c: unknown[]) => c[0] === commandId,
  );
  if (!call) {
    throw new Error(`command ${commandId} was never registered`);
  }
  return call[1] as (...args: unknown[]) => unknown;
}

function makeClient(): TicketmasterClient {
  return {
    acquireLease: vi.fn().mockResolvedValue({ lease: {}, events: [] }),
    createArtifact: vi
      .fn()
      .mockResolvedValue({ artifact: { id: "A-1" }, events: [] }),
    attachEvidence: vi.fn().mockResolvedValue({ events: [] }),
    transition: vi.fn().mockResolvedValue({ events: [] }),
    createDecision: vi
      .fn()
      .mockResolvedValue({ decision: { id: "D-1" }, events: [] }),
    listTickets: vi.fn().mockResolvedValue([]),
  } as unknown as TicketmasterClient;
}

function makeDeps(client: TicketmasterClient): CommandDeps {
  return {
    client,
    treeProvider: { refresh: vi.fn().mockResolvedValue(undefined) },
    leaseDecorationProvider: { refresh: vi.fn().mockResolvedValue(undefined) },
    codeLensProvider: { refresh: vi.fn().mockResolvedValue(undefined) },
  } as unknown as CommandDeps;
}

describe("registerCommands", () => {
  beforeEach(() => {
    mocks.registerCommand.mockClear();
    mocks.showInputBox.mockReset();
  });

  it("claimTicket acquires a lease with ttl_seconds and actor (no ttlSeconds)", async () => {
    const client = makeClient();
    const deps = makeDeps(client);
    registerCommands({ subscriptions: [] } as never, deps);

    await callbackFor("ticketmaster.claimTicket")("T-1");

    expect(client.acquireLease).toHaveBeenCalledTimes(1);
    const [id, body] = (client.acquireLease as ReturnType<typeof vi.fn>).mock
      .calls[0] as [string, Record<string, unknown>];
    expect(id).toBe("T-1");
    expect(body).toMatchObject({ ttl_seconds: 600 });
    expect(body.actor).toBeTruthy();
    expect(body).not.toHaveProperty("ttlSeconds");
  });

  it("submitWithEvidence attaches evidence then submits with no top-level trigger", async () => {
    const client = makeClient();
    const deps = makeDeps(client);
    mocks.showInputBox.mockResolvedValue("ran the tests, all green");
    registerCommands({ subscriptions: [] } as never, deps);

    await callbackFor("ticketmaster.submitWithEvidence")("T-1");

    expect(client.createArtifact).toHaveBeenCalledTimes(1);
    expect(client.attachEvidence).toHaveBeenCalledTimes(1);
    const [evidenceId, evidenceBody] = (
      client.attachEvidence as ReturnType<typeof vi.fn>
    ).mock.calls[0] as [string, Record<string, unknown>];
    expect(evidenceId).toBe("T-1");
    expect(evidenceBody.kind).toBe("human_attestation");
    expect(evidenceBody.artifact).toBe("A-1");

    expect(client.transition).toHaveBeenCalledTimes(1);
    const [ticketId, command] = (
      client.transition as ReturnType<typeof vi.fn>
    ).mock.calls[0] as [string, Record<string, unknown>];
    expect(ticketId).toBe("T-1");
    expect(command).toEqual({
      submit: {
        summary: "ran the tests, all green",
        evidence: ["A-1"],
        actor: expect.any(String),
      },
    });
    expect(command).not.toHaveProperty("trigger");
  });
});
