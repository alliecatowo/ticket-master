import * as vscode from "vscode";
import type { TicketmasterClient } from "../ticketmaster/client";
import type { DecisionCodeLensProvider } from "../codelens/decisionCodeLensProvider";
import type { LeaseDecorationProvider } from "../leases/leaseDecorationProvider";
import type { TicketTreeProvider } from "../tree/ticketTreeProvider";

export interface CommandDeps {
  client: TicketmasterClient;
  treeProvider: TicketTreeProvider;
  leaseDecorationProvider: LeaseDecorationProvider;
  codeLensProvider: DecisionCodeLensProvider;
}

async function refreshAll(deps: CommandDeps): Promise<void> {
  await Promise.all([
    deps.treeProvider.refresh(),
    deps.leaseDecorationProvider.refresh(),
    deps.codeLensProvider.refresh(),
  ]);
}

async function pickTicketId(deps: CommandDeps): Promise<string | undefined> {
  const tickets = await deps.client.listTickets();
  const picked = await vscode.window.showQuickPick(
    tickets.map((t) => ({
      label: t.id,
      description: t.state,
      detail: t.objective,
    })),
    { placeHolder: "Select a ticket" },
  );
  return picked?.label;
}

export function registerCommands(
  context: vscode.ExtensionContext,
  deps: CommandDeps,
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("ticketmaster.refresh", () =>
      refreshAll(deps),
    ),

    vscode.commands.registerCommand(
      "ticketmaster.openTicket",
      async (ticketId?: string) => {
        const id = ticketId ?? (await pickTicketId(deps));
        if (!id) {
          return;
        }
        const ticket = await deps.client.getTicket(id);
        const doc = await vscode.workspace.openTextDocument({
          language: "json",
          content: JSON.stringify(ticket, null, 2),
        });
        await vscode.window.showTextDocument(doc, { preview: true });
      },
    ),

    vscode.commands.registerCommand(
      "ticketmaster.claimTicket",
      async (ticketId?: string) => {
        const id = ticketId ?? (await pickTicketId(deps));
        if (!id) {
          return;
        }
        const holder = `human:${process.env.USER ?? "unknown"}`;
        await deps.client.leaseTicket(id, { holder, ttlSeconds: 600 });
        await refreshAll(deps);
        void vscode.window.showInformationMessage(`Claimed ${id} as ${holder}`);
      },
    ),

    vscode.commands.registerCommand(
      "ticketmaster.submitWithEvidence",
      async (ticketId?: string) => {
        const id = ticketId ?? (await pickTicketId(deps));
        if (!id) {
          return;
        }
        const summary = await vscode.window.showInputBox({
          prompt: "Evidence summary (e.g. test output, diff description)",
        });
        if (!summary) {
          return;
        }
        await deps.client.submit(id, [
          { kind: "HumanAttestation", summary },
        ]);
        await refreshAll(deps);
        void vscode.window.showInformationMessage(`Submitted ${id}`);
      },
    ),

    vscode.commands.registerCommand(
      "ticketmaster.recordDecision",
      async (decisionId?: string) => {
        if (decisionId) {
          const decisions = await deps.client.listDecisions();
          const decision = decisions.find((d) => d.id === decisionId);
          if (decision) {
            void vscode.window.showInformationMessage(
              `${decision.id}: ${decision.decision} — ${decision.reason}`,
            );
            return;
          }
        }
        const subject = await vscode.window.showInputBox({ prompt: "Subject" });
        if (!subject) {
          return;
        }
        const decision = await vscode.window.showInputBox({ prompt: "Decision" });
        if (!decision) {
          return;
        }
        const reason = await vscode.window.showInputBox({ prompt: "Reason" });
        if (!reason) {
          return;
        }
        const editor = vscode.window.activeTextEditor;
        const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
        const affectedDocs =
          editor && workspaceRoot
            ? [
                vscode.workspace.asRelativePath(editor.document.uri, false),
              ]
            : [];
        await deps.client.recordDecision({
          subject,
          decision,
          reason,
          affectedDocs,
        });
        await refreshAll(deps);
      },
    ),

    vscode.commands.registerCommand("ticketmaster.runOnSelection", async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) {
        return;
      }
      const selection = editor.document.getText(editor.selection);
      if (!selection.trim()) {
        void vscode.window.showWarningMessage(
          "Select some text before running tm on it.",
        );
        return;
      }
      const terminal =
        vscode.window.terminals.find((t) => t.name === "tm") ??
        vscode.window.createTerminal("tm");
      terminal.show();
      // `tm` (SPEC.md 15) reads free-form objectives from argv; quoting keeps
      // the selection as a single shell argument.
      terminal.sendText(`tm run -- ${JSON.stringify(selection)}`, true);
    }),
  );
}
