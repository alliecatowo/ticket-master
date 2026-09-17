import * as vscode from "vscode";
import type { TicketmasterClient } from "../ticketmaster/client";
import type { Milestone, Ticket } from "../ticketmaster/types";
import { buildMilestoneTree, type TmTreeNode } from "./treeModel";

export class TmTreeItem extends vscode.TreeItem {
  constructor(public readonly node: TmTreeNode) {
    super(node.label, itemCollapsibleState(node));
    this.id = node.id;
    if (node.kind === "ticket") {
      this.description = node.badge.text;
      this.iconPath = new vscode.ThemeIcon(node.badge.icon);
      this.contextValue = "ticket";
      this.command = {
        command: "ticketmaster.openTicket",
        title: "Open Ticket",
        arguments: [node.ticket.id],
      };
    } else if (node.kind === "milestone") {
      this.description = node.state;
      this.iconPath = new vscode.ThemeIcon(
        node.state === "Closed" ? "check-all" : "milestone",
      );
      this.contextValue = "milestone";
    } else {
      this.iconPath = new vscode.ThemeIcon("question");
      this.contextValue = "unassigned-group";
    }
  }
}

function itemCollapsibleState(
  node: TmTreeNode,
): vscode.TreeItemCollapsibleState {
  return node.children.length > 0
    ? vscode.TreeItemCollapsibleState.Expanded
    : vscode.TreeItemCollapsibleState.None;
}

/** Adapts the pure `buildMilestoneTree` model onto `vscode.TreeDataProvider`. */
export class TicketTreeProvider implements vscode.TreeDataProvider<TmTreeNode> {
  private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<
    TmTreeNode | undefined | void
  >();
  readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

  private milestones: Milestone[] = [];
  private tickets: Ticket[] = [];

  constructor(private readonly client: TicketmasterClient) {}

  async refresh(): Promise<void> {
    const snapshot = await this.client.getState();
    this.milestones = snapshot.milestones;
    this.tickets = snapshot.tickets;
    this.onDidChangeTreeDataEmitter.fire();
  }

  getTreeItem(element: TmTreeNode): vscode.TreeItem {
    return new TmTreeItem(element);
  }

  getChildren(element?: TmTreeNode): TmTreeNode[] {
    if (!element) {
      return buildMilestoneTree(this.milestones, this.tickets);
    }
    return element.children;
  }
}
