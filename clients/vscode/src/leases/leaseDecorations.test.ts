import type { Lease } from "@ticketmaster/client";
import { describe, expect, it } from "vitest";
import {
  computeLeaseDecoration,
  findLeasesForPath,
  leaseToRange,
} from "./leaseDecorations";

function lease(overrides: Partial<Lease> & { id: string }): Lease {
  return {
    ticket: "T-1",
    holder: "agent:claude/a81",
    authority: null,
    resources: [{ paths: ["src/auth/**"], mode: "exclusive" }],
    acquired: "2026-01-01T00:00:00Z",
    heartbeat: "2026-01-01T00:00:01Z",
    ttl_seconds: 60,
    epoch: 1,
    ...overrides,
  };
}

describe("findLeasesForPath", () => {
  it("finds a lease whose resource claim covers the path", () => {
    const l = lease({ id: "L-1" });
    expect(findLeasesForPath([l], "src/auth/login.rs")).toEqual([l]);
  });

  it("excludes a lease whose resource claim does not cover the path", () => {
    const l = lease({ id: "L-1" });
    expect(findLeasesForPath([l], "src/web/index.ts")).toEqual([]);
  });

  it("orders multiple matching leases most-recently-acquired first", () => {
    const older = lease({ id: "L-1", ticket: "T-1", acquired: "2026-01-01T00:00:00Z" });
    const newer = lease({ id: "L-2", ticket: "T-2", acquired: "2026-01-02T00:00:00Z" });
    expect(findLeasesForPath([older, newer], "src/auth/login.rs")).toEqual([
      newer,
      older,
    ]);
  });
});

describe("leaseToRange", () => {
  it("spans from the first to the last line of the document", () => {
    expect(leaseToRange(10)).toEqual({
      startLine: 0,
      startCharacter: 0,
      endLine: 9,
      endCharacter: Number.MAX_SAFE_INTEGER,
    });
  });

  it("clamps to line 0 for an empty document", () => {
    expect(leaseToRange(0).endLine).toBe(0);
  });
});

describe("computeLeaseDecoration", () => {
  it("returns undefined when nothing leases the file", () => {
    expect(computeLeaseDecoration([], "src/auth/login.rs", 20)).toBeUndefined();
  });

  it("returns a decoration naming the ticket and holder for a single lease", () => {
    const l = lease({ id: "L-1" });
    const decoration = computeLeaseDecoration([l], "src/auth/login.rs", 20);
    expect(decoration).toBeDefined();
    expect(decoration!.leases).toEqual([l]);
    expect(decoration!.message).toBe("Leased by T-1 (agent:claude/a81)");
    expect(decoration!.range.endLine).toBe(19);
  });

  it("names every ticket when several leases cover the file", () => {
    const a = lease({ id: "L-1", ticket: "T-1", acquired: "2026-01-01T00:00:00Z" });
    const b = lease({ id: "L-2", ticket: "T-2", acquired: "2026-01-02T00:00:00Z" });
    const decoration = computeLeaseDecoration([a, b], "src/auth/login.rs", 5);
    expect(decoration!.message).toBe("Leased by T-2, T-1");
  });
});
