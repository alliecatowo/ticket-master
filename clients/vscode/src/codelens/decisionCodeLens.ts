/**
 * Pure logic for surfacing decisions (SPEC.md 4.6) that affect the
 * currently open file as CodeLens entries at the top of the document. No
 * `vscode` import here; see `./decisionCodeLensProvider.ts` for the host
 * wiring.
 */
import type { Decision } from "@ticketmaster/client";
import { matchesAny } from "../shared/glob";

export interface DecisionCodeLensItem {
  /** 0-based line the lens should be anchored to. Always 0 for now: a
   * decision affects a whole file, not a specific line, per SPEC.md 4.6
   * (`affected_paths: Vec<PathPattern>` has no line granularity). */
  line: number;
  title: string;
  decisionId: string;
  tooltip: string;
}

/** Decisions affecting `filePath`, superseded ones excluded (a superseded
 * decision is no longer the operative rationale; SPEC.md 4.6 says
 * "the old decision is never mutated in the log" but a reader should follow
 * `superseded_by` rather than resurface the stale one). Most recent first. */
export function decisionsAffectingFile(
  decisions: Decision[],
  filePath: string,
): Decision[] {
  return decisions
    .filter((d) => d.superseded_by === null)
    .filter((d) => matchesAny(d.affected_paths, filePath))
    .sort((a, b) => (a.ts < b.ts ? 1 : -1));
}

export function computeDecisionCodeLenses(
  decisions: Decision[],
  filePath: string,
): DecisionCodeLensItem[] {
  return decisionsAffectingFile(decisions, filePath).map((d) => ({
    line: 0,
    title: `${d.id}: ${d.decision}`,
    decisionId: d.id,
    tooltip: d.reason,
  }));
}
