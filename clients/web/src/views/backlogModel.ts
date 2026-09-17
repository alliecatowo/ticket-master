import type { Ticket, TicketState } from "../api/types";

export interface BacklogFilters {
  milestone: string | null;
  kind: string | null;
  executorRole: string | null;
}

export const BACKLOG_GROUPS = ["ready", "blocked", "active"] as const;
export type BacklogGroup = (typeof BACKLOG_GROUPS)[number];

const ACTIVE_STATES: TicketState[] = [
  "leased",
  "running",
  "submitted",
  "verifying",
  "auditing",
  "rework",
  "replan",
  "recovery",
  "escalated",
];

/** Which backlog group (SPEC.md §18.2: "Ready / blocked / active") a ticket belongs in, if any. */
export function backlogGroupFor(ticket: Ticket): BacklogGroup | null {
  if (ticket.state === "ready") return "ready";
  if (ticket.state === "blocked") return "blocked";
  if (ACTIVE_STATES.includes(ticket.state)) return "active";
  return null;
}

export function executorRoleOf(ticket: Ticket): string | null {
  const role = ticket.executor?.role;
  return typeof role === "string" ? role : null;
}

/** Pure filter step, unit-testable without a store: milestone / kind / executor role. */
export function applyBacklogFilters(tickets: Ticket[], filters: BacklogFilters): Ticket[] {
  return tickets.filter((ticket) => {
    if (filters.milestone && ticket.milestone !== filters.milestone) return false;
    if (filters.kind && ticket.kind !== filters.kind) return false;
    if (filters.executorRole && executorRoleOf(ticket) !== filters.executorRole) return false;
    return true;
  });
}

export function groupBacklog(tickets: Ticket[]): Record<BacklogGroup, Ticket[]> {
  const groups: Record<BacklogGroup, Ticket[]> = { ready: [], blocked: [], active: [] };
  for (const ticket of tickets) {
    const group = backlogGroupFor(ticket);
    if (group) groups[group].push(ticket);
  }
  return groups;
}
