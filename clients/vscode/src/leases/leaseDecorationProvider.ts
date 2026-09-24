import type { Lease, TicketmasterClient } from "@ticketmaster/client";
import * as path from "path";
import * as vscode from "vscode";
import { computeLeaseDecoration } from "./leaseDecorations";

/** Adapts `computeLeaseDecoration` onto a live `TextEditorDecorationType`,
 * refreshed whenever the active editor changes or a new snapshot arrives. */
export class LeaseDecorationProvider implements vscode.Disposable {
  private readonly decorationType = vscode.window.createTextEditorDecorationType({
    isWholeLine: true,
    overviewRulerColor: new vscode.ThemeColor("editorWarning.foreground"),
    overviewRulerLane: vscode.OverviewRulerLane.Left,
    after: {
      color: new vscode.ThemeColor("descriptionForeground"),
    },
  });
  private leases: Lease[] = [];
  private readonly disposables: vscode.Disposable[] = [];

  constructor(
    private readonly client: TicketmasterClient,
    private readonly workspaceRoot: string | undefined,
  ) {
    this.disposables.push(
      vscode.window.onDidChangeActiveTextEditor(() => this.applyToActiveEditor()),
    );
  }

  async refresh(): Promise<void> {
    const snapshot = await this.client.getState();
    this.leases = snapshot.leases;
    this.applyToActiveEditor();
  }

  private applyToActiveEditor(): void {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      return;
    }
    const relativePath = this.workspaceRoot
      ? path.relative(this.workspaceRoot, editor.document.uri.fsPath)
      : editor.document.uri.fsPath;
    const decoration = computeLeaseDecoration(
      this.leases,
      relativePath,
      editor.document.lineCount,
    );
    if (!decoration) {
      editor.setDecorations(this.decorationType, []);
      return;
    }
    const range = new vscode.Range(
      decoration.range.startLine,
      decoration.range.startCharacter,
      decoration.range.endLine,
      decoration.range.endCharacter,
    );
    editor.setDecorations(this.decorationType, [
      {
        range,
        hoverMessage: decoration.message,
        renderOptions: {
          after: { contentText: `  ${decoration.message}` },
        },
      },
    ]);
  }

  dispose(): void {
    this.decorationType.dispose();
    for (const d of this.disposables) {
      d.dispose();
    }
  }
}
