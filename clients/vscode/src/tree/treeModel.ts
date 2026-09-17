/**
 * Pure logic for building the milestones -> tickets -> children tree shown
 * in the Ticketmaster view (SPEC.md 18.4). No `vscode` import here so this
 * is unit-testable without a host; `./ticketTreeProvider.ts` adapts this
 * into `vscode.TreeDataProvider`.
 */
import type { Milestone, Ticket, TicketState } from "../ticketmaster/types";

export type TmTreeNode = MilestoneNode | TicketNode | UnassignedGroupNode;

export interface MilestoneNode {
  kind: "milestone";
  id: string;
  label: string;
  state: Milestone["state"];
  children: TicketNode[];
}

export interface TicketNode {
  kind: "ticket";
  id: string;
  label: string;
  state: TicketState;
  badge: StateBadge;
  ticket: Ticket;
  children: TicketNode[];
}

/** Synthetic bucket for tickets that reference no milestone, so no ticket is
 * silently dropped from the tree. */
export interface UnassignedGroupNode {
  kind: "unassigned-group";
  id: "__unassigned__";
  label: string;
  children: TicketNode[];
}

export interface StateBadge {
  /** Single-character/emoji-free short label suitable for a TreeItem
   * description, e.g. "READY", "BLOCKED". */
  text: string;
  /** VS Code ThemeIcon id (without the `$()` wrapper) the host layer can use
   * for the tree item icon. */
  icon: string;
}

const BADGES: Record<TicketState, StateBadge> = {
  Draft: { text: "DRAFT", icon: "circle-outline" },
  Blocked: { text: "BLOCKED", icon: "circle-slash" },
  Ready: { text: "READY", icon: "play-circle" },
  Leased: { text: "LEASED", icon: "lock" },
  Running: { text: "RUNNING", icon: "sync~spin" },
  Submitted: { text: "SUBMITTED", icon: "cloud-upload" },
  Verifying: { text: "VERIFYING", icon: "beaker" },
  Auditing: { text: "AUDITING", icon: "eye" },
  Rework: { text: "REWORK", icon: "history" },
  Replan: { text: "REPLAN", icon: "issue-reopened" },
  Recovery: { text: "RECOVERY", icon: "debug-restart" },
  Escalated: { text: "ESCALATED", icon: "warning" },
  Closed: { text: "CLOSED", icon: "check" },
  Cancelled: { text: "CANCELLED", icon: "circle-slash" },
};

export function stateBadge(state: TicketState): StateBadge {
  return BADGES[state];
}

function buildTicketNode(
  ticket: Ticket,
  byId: Map<string, Ticket>,
  seen: Set<string>,
): TicketNode {
  seen.add(ticket.id);
  const children: TicketNode[] = [];
  for (const childId of ticket.children) {
    const child = byId.get(childId);
    if (!child) {
      // Dangling reference (e.g. stale snapshot); skip rather than throw so
      // one bad edge doesn't blank the whole tree.
      continue;
    }
    if (seen.has(childId)) {
      // Cycle guard: SPEC.md 4.3 allows dependency cycles under
      // DependencyKind::Loop, but the parent/child tree must stay a tree.
      continue;
    }
    children.push(buildTicketNode(child, byId, seen));
  }
  return {
    kind: "ticket",
    id: ticket.id,
    label: `${ticket.id} ${ticket.objective}`,
    state: ticket.state,
    badge: stateBadge(ticket.state),
    ticket,
    children,
  };
}

/**
 * Build the milestone -> ticket -> child-ticket tree.
 *
 * - A ticket is a root of its milestone's subtree if it has no parent, or
 *   its parent belongs to a different milestone.
 * - Tickets with no milestone are collected into a single trailing
 *   "Unassigned" group (omitted from the result when empty).
 */
export function buildMilestoneTree(
  milestones: Milestone[],
  tickets: Ticket[],
): TmTreeNode[] {
  const byId = new Map(tickets.map((t) => [t.id, t] as const));
  const seen = new Set<string>();
  const nodes: TmTreeNode[] = [];

  for (const milestone of milestones) {
    const members = milestone.tickets
      .map((id) => byId.get(id))
      .filter((t): t is Ticket => t !== undefined);
    const roots = members.filter(
      (t) => !t.parent || byId.get(t.parent)?.milestone !== milestone.id,
    );
    const children = roots.map((t) => buildTicketNode(t, byId, seen));
    nodes.push({
      kind: "milestone",
      id: milestone.id,
      label: milestone.title,
      state: milestone.state,
      children,
    });
  }

  const unassigned = tickets.filter(
    (t) => t.milestone === null && !seen.has(t.id) && !t.parent,
  );
  if (unassigned.length > 0) {
    nodes.push({
      kind: "unassigned-group",
      id: "__unassigned__",
      label: "Unassigned",
      children: unassigned.map((t) => buildTicketNode(t, byId, seen)),
    });
  }

  return nodes;
}
