// `domain.ts` re-exports the identifiers `generated.ts` covers (`TicketId`, `ParticipantId`,
// `TicketKind`, `TicketState`); only re-export the generated names it doesn't, to avoid a
// duplicate-export collision.
export type { CreateTicketRequest, TransitionRequest as GeneratedTransitionRequest } from "./generated.js";
export * from "./domain.js";
export { TicketmasterClient, type TicketmasterClientOptions } from "./client.js";
export { ProjectStore, type ProjectStoreState } from "./store.js";
export { subscribeToEvents, readSseFrames, frameToEvent, type ReconnectOptions, type RawSseFrame } from "./sse.js";
