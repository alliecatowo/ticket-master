// The tickets view's model: which group a ticket sits in, its one-line summary, and what a
// human can do with it. A port of `crates/tm-cli/src/tickets/overview.rs` (`group_for`,
// `summary_for`) and `crates/tm-cli/src/tui/tickets_view.rs` (`choices`), so the web client and
// the TUI's tickets screen (D-019 §2) group and describe a ticket the same way. Pure: the clock
// is an argument.

import type { LiveLease, ProjectStoreSnapshot, TicketActivity } from "../api/store";
import type { Evidence, FailureRecord, Ticket, TicketId, TicketState } from "../api/types";

export type TicketGroup = "needs_input" | "working" | "review" | "queued" | "completed";

/** Display order, as in the TUI. */
export const GROUP_ORDER: TicketGroup[] = ["needs_input", "working", "review", "queued", "completed"];

export const GROUP_LABEL: Record<TicketGroup, string> = {
  needs_input: "Needs input",
  working: "Working",
  review: "Ready for review",
  queued: "Queued",
  completed: "Completed",
};

/** How a row's summary reads. */
export type SummaryTone = "normal" | "failure" | "stopped" | "success";

/**
 * The group for a ticket in `state`. `awaitingApproval` only counts while a worker is actually
 * mid-run (leased or running): an approval request whose worker died must not pin a ticket under
 * Needs input forever.
 */
export function groupFor(state: TicketState, awaitingApproval: boolean): TicketGroup {
  switch (state) {
    case "escalated":
      return "needs_input";
    case "leased":
    case "running":
      return awaitingApproval ? "needs_input" : "working";
    case "verifying":
    case "auditing":
      return "working";
    case "submitted":
      return "review";
    case "ready":
    case "blocked":
    case "draft":
    case "rework":
    case "replan":
    case "recovery":
      return "queued";
    case "closed":
    case "cancelled":
      return "completed";
  }
}

export function isTerminal(state: TicketState): boolean {
  return state === "closed" || state === "cancelled";
}

/** A numbered choice a ticket offers, as in the TUI's peek (`Choice`). */
export type Choice = "accept" | "reject" | "retry" | "retry_with_guidance" | "queue";

export const CHOICE_LABEL: Record<Choice, string> = {
  accept: "Accept",
  reject: "Reject",
  retry: "Retry",
  retry_with_guidance: "Retry with guidance",
  queue: "Queue it",
};

/** Choices that need words from the human before they can be sent. */
export const CHOICE_NEEDS_TEXT: Record<Choice, boolean> = {
  accept: false,
  reject: true,
  retry: false,
  retry_with_guidance: true,
  queue: false,
};

/** What a ticket in `state` offers, in the TUI's numbering order (`tickets_view.rs`'s `choices`). */
export function choicesFor(state: TicketState): Choice[] {
  switch (state) {
    case "submitted":
      return ["accept", "reject"];
    case "escalated":
      return ["retry", "retry_with_guidance"];
    case "draft":
      return ["queue"];
    default:
      return [];
  }
}

/**
 * Whether Cancel is offered. Every non-terminal state accepts `Trigger::Cancel`
 * (`crates/tm-core/src/machine.rs`), and a human actor skips the authority check.
 */
export function canCancel(state: TicketState): boolean {
  return !isTerminal(state);
}

/** The objective's first clause, at most 32 characters (`short_title` in `screens/tickets.rs`). */
export function shortTitle(objective: string, maxWidth = 32): string {
  const line = objective.split("\n").find((l) => l.trim().length > 0) ?? "";
  let end = line.length;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    let clauseEnd = false;
    if (";:,([—–".includes(c)) clauseEnd = true;
    else if (".!?".includes(c)) clauseEnd = i + 1 >= line.length || /\s/.test(line[i + 1]);
    else if (c === "-") clauseEnd = line.slice(0, i).endsWith(" ") && line.slice(i + 1).startsWith(" ");
    if (clauseEnd && line.slice(0, i).trim().length > 0) {
      end = i;
      break;
    }
  }
  const clause = line.slice(0, end).trim();
  if ([...clause].length <= maxWidth) return clause;
  let out = "";
  for (const word of clause.split(/\s+/)) {
    const next = out ? `${out} ${word}` : word;
    if ([...next].length > maxWidth - 1) break;
    out = next;
  }
  if (!out) out = [...clause].slice(0, maxWidth - 1).join("");
  return `${out}…`;
}

/** `5s`, `12m`, `3h`, `2d` (`compact_age` in `screens/tickets.rs`). */
export function compactAge(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}

/** Collapse whitespace to one line and drop control characters. */
export function oneLine(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/[\u0000-\u001f\u007f]/g, " ").replace(/\s+/g, " ").trim();
}

/** `running`, `awaiting approval` — a state as words. */
export function stateWord(state: TicketState): string {
  return state.replace(/_/g, " ");
}

export interface SummaryContext {
  activity: TicketActivity;
  /** The live lease holder, if any. */
  worker: string | null;
  /** Dependencies that are not closed yet. */
  openDependencies: TicketId[];
  /** Epoch millis, for retry countdowns. */
  now: number;
}

function latestActivity(act: TicketActivity): string | undefined {
  const command = act.lastCommand
    ? { text: `$ ${oneLine(act.lastCommand.text)}`, at: Date.parse(act.lastCommand.at) }
    : undefined;
  const step = act.lastStep ? { text: oneLine(act.lastStep.text), at: Date.parse(act.lastStep.at) } : undefined;
  if (command && step) return step.at > command.at ? step.text : command.text;
  return (command ?? step)?.text;
}

/**
 * The one-line summary for `ticket` and how it reads (`summary_for` in `overview.rs`). One
 * deliberate difference: the web client cannot tell whether `tm serve` was started with
 * `--no-workers`, so a queued ticket reads "queued · waiting for a worker" either way.
 */
export function summaryFor(ticket: Ticket, ctx: SummaryContext): { text: string; tone: SummaryTone } {
  const act = ctx.activity;
  const lastFailure = ticket.failures.length
    ? oneLine(ticket.failures[ticket.failures.length - 1].detail)
    : undefined;
  const plural = (n: number) => (n === 1 ? "" : "s");
  const normal = (text: string) => ({ text, tone: "normal" as const });
  switch (ticket.state) {
    case "escalated": {
      const reason = act.escalation && oneLine(act.escalation.reason) ? oneLine(act.escalation.reason) : lastFailure;
      if (reason && reason === lastFailure) {
        return normal(`gave up after ${ticket.attempts} attempt${plural(ticket.attempts)}: ${reason}`);
      }
      return normal(reason ?? "escalated: needs a decision from you");
    }
    case "leased":
    case "running": {
      if (act.pendingApproval) {
        const note = oneLine(act.pendingApproval.note);
        return normal(note ? `approval needed: ${note}` : "approval needed");
      }
      if (!ctx.worker) return normal("no worker attached; its lease lapsed");
      if (ticket.state === "leased") return normal(`starting attempt ${Math.max(ticket.attempts, 1)}`);
      return normal(latestActivity(act) ?? `working on attempt ${Math.max(ticket.attempts, 1)}`);
    }
    case "verifying":
      return normal("verifying the submission");
    case "auditing":
      return normal("auditing the verification");
    case "submitted":
      return normal(act.submission && oneLine(act.submission) ? oneLine(act.submission) : "submitted for review");
    case "rework":
    case "replan":
    case "recovery":
      return {
        text: lastFailure ? `attempt ${ticket.attempts} failed: ${lastFailure}` : stateWord(ticket.state),
        tone: "failure",
      };
    case "ready": {
      if (ticket.failures.length) {
        const wait = act.retryAt ? Date.parse(act.retryAt) - ctx.now : 0;
        const retry = wait > 0 ? `retrying in ${Math.ceil(wait / 1000)}s` : "retrying";
        return { text: `attempt ${ticket.attempts} failed: ${lastFailure ?? ""} · ${retry}`, tone: "failure" };
      }
      return normal("queued · waiting for a worker");
    }
    case "blocked":
      return normal(ctx.openDependencies.length ? `blocked on ${ctx.openDependencies.join(", ")}` : "blocked");
    case "draft":
      return normal("draft, not queued yet");
    case "closed":
      return {
        text: act.submission && oneLine(act.submission) ? `result: ${oneLine(act.submission)}` : "closed",
        tone: "success",
      };
    case "cancelled":
      return {
        text: act.cancelReason && oneLine(act.cancelReason) ? `stopped: ${oneLine(act.cancelReason)}` : "stopped",
        tone: "stopped",
      };
  }
}

/** One ticket as the tickets view shows it (`TicketOverview` in `overview.rs`). */
export interface TicketOverview {
  id: TicketId;
  title: string;
  objective: string;
  state: TicketState;
  group: TicketGroup;
  summary: string;
  tone: SummaryTone;
  /** What a Needs-input ticket waits for. */
  waitingFor: "approval" | "escalation" | null;
  waitingSince: string | null;
  worker: string | null;
  /** A worker holds it and is working it (not just holding it, not waiting on approval). */
  working: boolean;
  attempts: number;
  maxAttempts: number;
  latestActivity: string | null;
  failures: FailureRecord[];
  submission: string | null;
  submittedAt: string | null;
  evidence: Evidence[];
  created: string;
  /** The later of the projection's `updated` and the last event about the ticket. */
  updated: string;
  /** Since creation; frozen at the run's length once completed. */
  ageMs: number;
}

/** How many characters of the objective's first clause a row shows. */
export const ROW_TITLE_WIDTH = 44;

function liveHolder(leases: Map<string, LiveLease>, ticket: TicketId, now: number): string | null {
  for (const lease of leases.values()) {
    if (lease.ticket === ticket && lease.expiresAt > now) return lease.holder;
  }
  return null;
}

export function overviewOf(ticket: Ticket, store: ProjectStoreSnapshot, now: number): TicketOverview {
  const activity = store.activity.get(ticket.id) ?? {};
  // A closed or cancelled ticket has no worker, even if a lease on it has not lapsed yet.
  const worker = isTerminal(ticket.state) ? null : liveHolder(store.leases, ticket.id, now);
  const openDependencies = ticket.dependencies.filter((d) => {
    const dep = store.tickets.get(d);
    return dep !== undefined && dep.state !== "closed";
  });
  const awaitingApproval =
    activity.pendingApproval !== undefined && (ticket.state === "leased" || ticket.state === "running");
  const group = groupFor(ticket.state, awaitingApproval);
  const { text, tone } = summaryFor(ticket, { activity, worker, openDependencies, now });
  const created = Date.parse(ticket.created);
  // The projection's `updated` is not bumped by every state change (a close or cancel leaves
  // it where it was), so the log's last event about the ticket is the better "last touched".
  const timeline = store.timelines.get(ticket.id);
  const lastEventAt = timeline?.length ? timeline[timeline.length - 1].ts : null;
  const lastTouched = lastEventAt && Date.parse(lastEventAt) > Date.parse(ticket.updated) ? lastEventAt : ticket.updated;
  const ageMs = Math.max(0, (group === "completed" ? Date.parse(lastTouched) : now) - created) || 0;
  let waitingFor: TicketOverview["waitingFor"] = null;
  let waitingSince: string | null = null;
  if (group === "needs_input") {
    waitingFor = awaitingApproval ? "approval" : "escalation";
    waitingSince = awaitingApproval
      ? (activity.pendingApproval?.at ?? null)
      : (activity.escalation?.at ?? ticket.updated);
  }
  const retry = ticket.retry as { max_attempts?: number } | null;
  return {
    id: ticket.id,
    // The TUI fits 32 columns; a browser row has room for more before the summary.
    title: shortTitle(oneLine(ticket.objective), ROW_TITLE_WIDTH) || ticket.id,
    objective: ticket.objective.trim(),
    state: ticket.state,
    group,
    summary: text,
    tone,
    waitingFor,
    waitingSince,
    worker,
    working:
      worker !== null &&
      ["leased", "running", "verifying", "auditing"].includes(ticket.state) &&
      !awaitingApproval,
    attempts: ticket.attempts,
    maxAttempts: Math.max(retry?.max_attempts ?? ticket.attempts, ticket.attempts),
    latestActivity: latestActivity(activity) ?? null,
    failures: ticket.failures ?? [],
    submission: activity.submission ? oneLine(activity.submission) : null,
    submittedAt: activity.submittedAt ?? null,
    evidence: store.evidence.filter((e) => e.ticket === ticket.id),
    created: ticket.created,
    updated: lastTouched,
    ageMs,
  };
}

/** The numeric part of `T-12`, for newest-first ordering. */
export function idNumber(id: TicketId): number {
  const n = Number(id.split("-")[1]);
  return Number.isFinite(n) ? n : 0;
}

/**
 * Group overviews for display: newest first in the live groups, most recently finished first
 * in Completed (the TUI's `order`).
 */
export function groupOverviews(overviews: TicketOverview[]): Record<TicketGroup, TicketOverview[]> {
  const groups: Record<TicketGroup, TicketOverview[]> = {
    needs_input: [],
    working: [],
    review: [],
    queued: [],
    completed: [],
  };
  for (const o of overviews) groups[o.group].push(o);
  for (const g of GROUP_ORDER) {
    groups[g].sort((a, b) =>
      g === "completed" ? Date.parse(b.updated) - Date.parse(a.updated) : idNumber(b.id) - idNumber(a.id),
    );
  }
  return groups;
}

/** Every ticket in the store as an overview. */
export function overviewsOf(store: ProjectStoreSnapshot, now: number): TicketOverview[] {
  return [...store.tickets.values()].map((t) => overviewOf(t, store, now));
}

/** `2 needs input · 1 working · …`, skipping empty groups. */
export function countsLine(groups: Record<TicketGroup, TicketOverview[]>): string {
  return GROUP_ORDER.filter((g) => groups[g].length > 0)
    .map((g) => `${groups[g].length} ${GROUP_LABEL[g].toLowerCase()}`)
    .join(" · ");
}
