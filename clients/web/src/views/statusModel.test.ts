import { describe, expect, it } from "vitest";
import { buildStatusReport } from "./statusModel";
import type { WireEvent } from "../api/types";

function ev(seq: number, kind: string): WireEvent {
  return {
    seq,
    ts: "2026-01-01T00:00:00Z",
    kind,
    subject: "T-1",
    actor: "human:allie",
    session: null,
    causation: null,
    correlation: null,
    payload: {},
  };
}

describe("buildStatusReport", () => {
  it("buckets known event kinds into the right section", () => {
    const report = buildStatusReport([
      ev(1, "ticket.closed"),
      ev(2, "ticket.reopened"),
      ev(3, "doc.reconciled"),
      ev(4, "harness.benchmarked"),
      ev(5, "ticket.escalated"),
    ]);
    expect(report.closed).toHaveLength(1);
    expect(report.repaired).toHaveLength(1);
    expect(report.reconciled).toHaveLength(1);
    expect(report.benchmarked).toHaveLength(1);
    expect(report.needsYou).toHaveLength(1);
  });

  it("ignores event kinds it doesn't classify", () => {
    const report = buildStatusReport([ev(1, "ticket.heartbeat"), ev(2, "presence.updated")]);
    expect(Object.values(report).every((bucket) => bucket.length === 0)).toBe(true);
  });

  it("returns empty buckets for an empty event list", () => {
    const report = buildStatusReport([]);
    expect(report.closed).toEqual([]);
    expect(report.needsYou).toEqual([]);
  });
});
