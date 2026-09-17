import * as vscode from "vscode";
import { DecisionCodeLensProvider } from "./codelens/decisionCodeLensProvider";
import { registerCommands } from "./commands/commands";
import { LeaseDecorationProvider } from "./leases/leaseDecorationProvider";
import { TicketmasterClient } from "./ticketmaster/client";
import { TicketTreeProvider } from "./tree/ticketTreeProvider";

export function activate(context: vscode.ExtensionContext): void {
  const config = vscode.workspace.getConfiguration("ticketmaster");
  const baseUrl = config.get<string>("serverUrl", "http://127.0.0.1:4180");
  const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

  const client = new TicketmasterClient({ baseUrl });

  const treeProvider = new TicketTreeProvider(client);
  const treeView = vscode.window.createTreeView("ticketmaster.milestonesView", {
    treeDataProvider: treeProvider,
  });

  const leaseDecorationProvider = new LeaseDecorationProvider(
    client,
    workspaceRoot,
  );

  const codeLensProvider = new DecisionCodeLensProvider(client, workspaceRoot);
  const codeLensRegistration = vscode.languages.registerCodeLensProvider(
    { scheme: "file" },
    codeLensProvider,
  );

  registerCommands(context, {
    client,
    treeProvider,
    leaseDecorationProvider,
    codeLensProvider,
  });

  context.subscriptions.push(treeView, leaseDecorationProvider, codeLensRegistration);

  void treeProvider.refresh().catch((err) => {
    void vscode.window.showWarningMessage(
      `Ticketmaster: could not reach tm-server at ${baseUrl}: ${String(err)}`,
    );
  });
  void leaseDecorationProvider.refresh().catch(() => undefined);
  void codeLensProvider.refresh().catch(() => undefined);
}

export function deactivate(): void {
  // Providers are disposed via context.subscriptions; nothing else to tear down.
}
