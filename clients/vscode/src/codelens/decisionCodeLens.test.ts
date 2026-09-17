import { describe, expect, it } from "vitest";
import type { Decision } from "../ticketmaster/types";
import {
  computeDecisionCodeLenses,
  decisionsAffectingFile,
} from "./decisionCodeLens";

function decision(overrides: Partial<Decision> & { id: string }): Decision {
  return {
    subject: "auth flow",
    decision: "use OAuth device flow",
    reason: "no browser available headless",
    evidence: [],
    affectedTickets: ["T-1"],
    affectedDocs: ["src/auth/**"],
    author: "human:allie",
    ts: "2026-01-01T00:00:00Z",
    supersedes: null,
    supersededBy: null,
    ...overrides,
  };
}

describe("decisionsAffectingFile", () => {
  it("includes a decision whose affectedDocs glob matches the file", () => {
    const d = decision({ id: "D-1" });
    expect(decisionsAffectingFile([d], "src/auth/login.rs")).toEqual([d]);
  });

  it("excludes a decision whose affectedDocs glob does not match", () => {
    const d = decision({ id: "D-1" });
    expect(decisionsAffectingFile([d], "src/web/index.ts")).toEqual([]);
  });

  it("excludes a superseded decision", () => {
    const d = decision({ id: "D-1", supersededBy: "D-2" });
    expect(decisionsAffectingFile([d], "src/auth/login.rs")).toEqual([]);
  });

  it("orders matches most recent first", () => {
    const older = decision({ id: "D-1", ts: "2026-01-01T00:00:00Z" });
    const newer = decision({ id: "D-2", ts: "2026-01-02T00:00:00Z" });
    expect(decisionsAffectingFile([older, newer], "src/auth/login.rs")).toEqual([
      newer,
      older,
    ]);
  });
});

describe("computeDecisionCodeLenses", () => {
  it("produces one lens per affecting decision, anchored to line 0", () => {
    const d = decision({ id: "D-1", decision: "use OAuth device flow" });
    const lenses = computeDecisionCodeLenses([d], "src/auth/login.rs");
    expect(lenses).toEqual([
      {
        line: 0,
        title: "D-1: use OAuth device flow",
        decisionId: "D-1",
        tooltip: "no browser available headless",
      },
    ]);
  });

  it("returns an empty array when no decision affects the file", () => {
    const d = decision({ id: "D-1" });
    expect(computeDecisionCodeLenses([d], "src/web/index.ts")).toEqual([]);
  });
});
