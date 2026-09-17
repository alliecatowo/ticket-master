/**
 * Pure logic for turning active leases (SPEC.md 4.5) into editor
 * decorations for the file currently open, so a human can see "this file is
 * leased by T-184" before editing it. No `vscode` import here; see
 * `./leaseDecorationProvider.ts` for the host wiring.
 */
import type { Lease } from "../ticketmaster/types";
import { matchesAny } from "../shared/glob";

/** A `vscode.Range`-shaped value, expressed without importing `vscode` so
 * this module stays host-independent. `leaseDecorationProvider.ts` maps
 * this 1:1 onto `new vscode.Range(...)`. */
export interface SimpleRange {
  startLine: number;
  startCharacter: number;
  endLine: number;
  endCharacter: number;
}

export interface FileLeaseDecoration {
  /** The whole-document range to decorate. */
  range: SimpleRange;
  /** Leases whose resource claims cover this file, most recently acquired
   * first. */
  leases: Lease[];
  /** Human-readable hover/status text, e.g. "Leased by T-184 (agent:claude/a81)". */
  message: string;
}

/** Find every live lease whose resource claims cover `filePath`. */
export function findLeasesForPath(leases: Lease[], filePath: string): Lease[] {
  return leases
    .filter((lease) =>
      lease.resources.some((claim) => matchesAny(claim.patterns, filePath)),
    )
    .sort((a, b) => (a.acquired < b.acquired ? 1 : -1));
}

/** Map a lease list onto a whole-document range: every line in the open
 * editor, so the decoration is visible regardless of scroll position. */
export function leaseToRange(lineCount: number): SimpleRange {
  const lastLine = Math.max(0, lineCount - 1);
  return {
    startLine: 0,
    startCharacter: 0,
    endLine: lastLine,
    endCharacter: Number.MAX_SAFE_INTEGER,
  };
}

function formatMessage(leases: Lease[]): string {
  if (leases.length === 1) {
    return `Leased by ${leases[0].ticket} (${leases[0].holder})`;
  }
  return `Leased by ${leases.map((l) => l.ticket).join(", ")}`;
}

/**
 * Compute the decoration for a single open document, or `undefined` when no
 * lease covers it (the host should clear any existing decoration in that
 * case).
 */
export function computeLeaseDecoration(
  leases: Lease[],
  filePath: string,
  lineCount: number,
): FileLeaseDecoration | undefined {
  const matches = findLeasesForPath(leases, filePath);
  if (matches.length === 0) {
    return undefined;
  }
  return {
    range: leaseToRange(lineCount),
    leases: matches,
    message: formatMessage(matches),
  };
}
