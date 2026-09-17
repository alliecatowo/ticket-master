// GENERATED FILE — do not edit by hand.
//
// Produced by `pnpm gen` (scripts/gen.mjs) from the JSON Schema served at `GET /schema`
// (source for this run: schema/snapshot.json (no live server)). Re-run `pnpm gen` after the server's schema changes; CI
// regenerates and fails the build on drift, so this file and the server can never silently
// disagree (SPEC.md §18.1).
//
// `GET /schema` is itself partial (see the comment on this file's generator): only the
// definitions below exist there today. The rest of the Ticketmaster wire domain is hand-written
// in `src/domain.ts`.

/** Wire pattern: ^(T|V|A)-[0-9]+$ */
export type TicketId = string;

export type ParticipantId = string;

export type TicketKind = "work" | "verification" | "audit" | "investigation" | "recovery" | "harness";

export type TicketState = "draft" | "blocked" | "ready" | "leased" | "running" | "submitted" | "verifying" | "auditing" | "rework" | "replan" | "recovery" | "escalated" | "closed" | "cancelled";

export interface CreateTicketRequest {
  "kind": TicketKind;
  "objective": string;
  "parent"?: TicketId;
  "priority"?: number;
  "actor": ParticipantId;
}

/** externally tagged: one of activate|trigger|submit|verify|audit|close|cancel|reopen|fail */
export type TransitionRequest = Record<string, unknown>;
