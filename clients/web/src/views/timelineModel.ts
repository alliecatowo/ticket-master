// How one event in a ticket's timeline reads. Pure.

import type { WireEvent } from "../api/types";
import { oneLine } from "./ticketsModel";

export type TimelineTone = "normal" | "good" | "bad" | "muted";

export interface TimelineEntry {
  seq: number;
  ts: string;
  actor: string;
  kind: string;
  text: string;
  tone: TimelineTone;
  /** Bookkeeping (usage, sessions, heartbeats): hidden unless the user asks for every event. */
  minor: boolean;
}

const MINOR_KINDS = new Set([
  "usage.recorded",
  "authority.reverted",
  "authority.granted",
  "goal.reoriented",
  "provider.selected",
  "session.started",
  "session.ended",
  "session.joined",
  "session.left",
  "ticket.heartbeat",
  "presence.updated",
  "artifact.created",
  "effect.journaled",
  "effect.completed",
  "resource.claimed",
  "resource.released",
]);

function field(event: WireEvent, name: string): string | undefined {
  const payload = event.payload as Record<string, unknown> | null;
  const value = payload?.[name];
  return typeof value === "string" ? value : undefined;
}

/** `Label: text`, or just `Label` when the event carried no text (an empty reason, say). */
function labelled(label: string, text: string | undefined): string {
  const t = oneLine(text ?? "");
  return t ? `${label}: ${t}` : label;
}

export function describeEvent(event: WireEvent): TimelineEntry {
  const base = { seq: event.seq, ts: event.ts, actor: event.actor, kind: event.kind };
  const entry = (text: string, tone: TimelineTone = "normal", minor = false): TimelineEntry => ({
    ...base,
    text: oneLine(text),
    tone,
    minor: minor || MINOR_KINDS.has(event.kind),
  });
  const payload = (event.payload ?? {}) as Record<string, unknown>;
  switch (event.kind) {
    case "ticket.created":
      return entry(labelled("Created", field(event, "title")));
    case "ticket.updated": {
      const fields = (payload.fields ?? {}) as Record<string, unknown>;
      if (fields.evidence_attached) {
        const e = fields.evidence_attached as { artifact?: string; summary?: string };
        return entry(`Evidence attached: ${e.artifact ?? ""}${e.summary ? ` (${e.summary})` : ""}`);
      }
      if (typeof fields.objective === "string") return entry("Objective updated");
      if (typeof fields.attempts === "number") return entry(`Attempt ${fields.attempts} started`);
      if (Array.isArray(fields.failures)) return entry("Failure recorded", "muted", true);
      if (fields.retry && Object.keys(fields).length === 1) return entry("Attempts renewed", "muted", true);
      return entry(`Updated ${Object.keys(fields).join(", ")}`, "muted", true);
    }
    case "ticket.state_changed":
      return entry(`${field(event, "from") ?? "?"} → ${field(event, "to") ?? "?"}`, "muted");
    case "ticket.leased":
      return entry(`Leased to ${field(event, "holder") ?? "a worker"}`);
    case "ticket.lease_released":
      return entry("Lease released", "muted", true);
    case "ticket.lease_expired":
      return entry("Lease expired", "bad");
    case "ticket.submitted":
      return entry(labelled("Submitted", field(event, "summary")), "good");
    case "ticket.verified":
      return entry("Verified", "good");
    case "ticket.verification_failed":
      return entry(labelled("Rejected", field(event, "reason")), "bad");
    case "ticket.audited":
      return entry("Audited", "good");
    case "ticket.audit_rejected":
      return entry(labelled("Audit rejected", field(event, "reason")), "bad");
    case "ticket.closed": {
      const reason = field(event, "reason");
      return entry(reason ? `Closed: ${reason}` : "Closed", "good");
    }
    case "ticket.cancelled": {
      const reason = field(event, "reason");
      return entry(reason ? `Cancelled: ${reason}` : "Cancelled", "bad");
    }
    case "ticket.reopened":
      return entry("Reopened");
    case "ticket.failed":
      return entry(labelled("Failed", field(event, "reason")), "bad");
    case "ticket.retry_scheduled":
      return entry(`Retry ${payload.attempt ?? ""} scheduled`, "muted");
    case "ticket.escalated":
      return entry(labelled("Escalated", field(event, "reason")), "bad");
    case "ticket.budget_exhausted":
      return entry("Budget exhausted", "bad");
    case "ticket.budget_handoff":
      return entry("Handed back near its budget", "muted");
    case "ticket.forked":
      return entry("Forked");
    case "ticket.child_added":
      return entry(`Child ${field(event, "child") ?? ""} added`);
    case "ticket.dependency_added":
      return entry(`Now depends on ${field(event, "depends_on") ?? ""}`);
    case "goal.set":
      return entry(labelled("Goal", field(event, "text")), "muted", true);
    case "goal.step_added":
      return entry(labelled("Step", field(event, "text")));
    case "goal.step_completed":
      return entry("Step done", "muted", true);
    case "goal.claimed_complete":
      return entry("Worker says the goal is complete");
    case "command.started":
      return entry(`$ ${field(event, "command") ?? ""}`);
    case "command.completed":
      return entry(`Finished $ ${field(event, "command") ?? ""}`, "muted", true);
    case "approval.requested":
      return entry(labelled("Asked for approval", field(event, "note")));
    case "approval.decided":
      return entry(payload.approved ? "Approval granted" : "Approval denied", payload.approved ? "good" : "bad");
    case "provider.selected":
      return entry(`Model ${field(event, "provider") ?? ""}/${field(event, "model") ?? ""}`, "muted");
    case "usage.recorded":
      return entry(`${payload.tokens ?? 0} tokens`, "muted");
    case "provider.exhausted":
      return entry(labelled(`Provider ${field(event, "provider") ?? ""} exhausted`, field(event, "reason")), "muted", true);
    case "session.started":
      return entry(`Session ${field(event, "session") ?? ""} started`, "muted");
    case "session.ended":
      return entry(`Session ${field(event, "session") ?? ""} ended`, "muted");
    default:
      return entry(event.kind, "muted", MINOR_KINDS.has(event.kind));
  }
}
