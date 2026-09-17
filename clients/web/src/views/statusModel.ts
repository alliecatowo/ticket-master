import type { WireEvent } from "../api/types";

export interface StatusReport {
  closed: WireEvent[];
  repaired: WireEvent[];
  reconciled: WireEvent[];
  benchmarked: WireEvent[];
  needsYou: WireEvent[];
}

/**
 * Derive the "since you left" report (SPEC.md §18.2: "closed, repaired, reconciled,
 * benchmarked, needs-you") from the store's recent-events buffer.
 *
 * This is a heuristic grouping over `EventKind` (see `crates/tm-events/src/kind.rs`), not a
 * dedicated server-side report endpoint — `tm-server` doesn't expose one, so the web app builds
 * it client-side from the same event log every other view reconciles against. Kept as a pure
 * function of `WireEvent[]` so it's unit-testable without a store or a client.
 */
export function buildStatusReport(events: WireEvent[]): StatusReport {
  const report: StatusReport = {
    closed: [],
    repaired: [],
    reconciled: [],
    benchmarked: [],
    needsYou: [],
  };
  for (const event of events) {
    switch (event.kind) {
      case "ticket.closed":
        report.closed.push(event);
        break;
      case "ticket.reopened":
      case "ticket.audit_rejected":
      case "ticket.retry_scheduled":
        report.repaired.push(event);
        break;
      case "doc.reconciled":
        report.reconciled.push(event);
        break;
      case "harness.benchmarked":
        report.benchmarked.push(event);
        break;
      case "ticket.escalated":
      case "approval.requested":
      case "ticket.budget_exhausted":
        report.needsYou.push(event);
        break;
      default:
        break;
    }
  }
  return report;
}
