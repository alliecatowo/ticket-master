import * as path from "path";
import * as vscode from "vscode";
import type { TicketmasterClient } from "../ticketmaster/client";
import type { Decision } from "../ticketmaster/types";
import { computeDecisionCodeLenses } from "./decisionCodeLens";

/** Adapts `computeDecisionCodeLenses` onto `vscode.CodeLensProvider`. */
export class DecisionCodeLensProvider implements vscode.CodeLensProvider {
  private readonly onDidChangeCodeLensesEmitter = new vscode.EventEmitter<void>();
  readonly onDidChangeCodeLenses = this.onDidChangeCodeLensesEmitter.event;

  private decisions: Decision[] = [];

  constructor(
    private readonly client: TicketmasterClient,
    private readonly workspaceRoot: string | undefined,
  ) {}

  async refresh(): Promise<void> {
    const snapshot = await this.client.getState();
    this.decisions = snapshot.decisions;
    this.onDidChangeCodeLensesEmitter.fire();
  }

  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    const relativePath = this.workspaceRoot
      ? path.relative(this.workspaceRoot, document.uri.fsPath)
      : document.uri.fsPath;
    return computeDecisionCodeLenses(this.decisions, relativePath).map(
      (item) =>
        new vscode.CodeLens(new vscode.Range(item.line, 0, item.line, 0), {
          title: item.title,
          tooltip: item.tooltip,
          command: "ticketmaster.recordDecision",
          arguments: [item.decisionId],
        }),
    );
  }
}
