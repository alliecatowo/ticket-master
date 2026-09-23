// Who the web client acts as. Accept, reject and retry are human-only on the server
// (`tm-core`'s `Store::accept`/`reject`/`retry` refuse any actor that is not `human:<handle>`), and
// every other mutation needs an actor too, so the actor is always `human:<handle>`.
//
// Only the handle is stored. The one place that turns it into an actor is `actorFor`, which the
// API client (`client.ts`) calls for every request body it builds.

export const DEFAULT_HANDLE = "web";
export const HANDLE_STORAGE_KEY = "tm.web.actor";

/** Longest handle kept; the rest is dropped (it shows in every event's actor column). */
export const MAX_HANDLE_LENGTH = 40;

/**
 * Turn whatever the user typed (or an older stored value) into a valid handle: control characters
 * dropped, trimmed, a pasted `human:` prefix removed, runs of whitespace joined with `-`, at most
 * `MAX_HANDLE_LENGTH` characters, and `web` when nothing is left. The server only requires the
 * handle to be non-empty (`ParticipantId`'s validator in `crates/tm-types/src/id.rs`).
 */
export function normalizeHandle(input: string | null | undefined): string {
  // eslint-disable-next-line no-control-regex
  let handle = (input ?? "").replace(/[\u0000-\u001f\u007f]/g, " ").trim();
  while (handle.toLowerCase().startsWith("human:")) handle = handle.slice("human:".length).trim();
  handle = [...handle.replace(/\s+/g, "-")].slice(0, MAX_HANDLE_LENGTH).join("");
  return handle.length > 0 ? handle : DEFAULT_HANDLE;
}

/** The participant id for a handle: always `human:<handle>`. */
export function actorFor(handle: string): string {
  return `human:${normalizeHandle(handle)}`;
}

/** Minimal storage surface, so tests can pass a plain object instead of `localStorage`. */
export interface HandleStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

function defaultStorage(): HandleStorage | undefined {
  try {
    return typeof localStorage === "undefined" ? undefined : localStorage;
  } catch {
    // Some browsers throw on `localStorage` access with storage disabled.
    return undefined;
  }
}

/** The saved handle, normalized (an older build stored `human:web` or a bare name here). */
export function loadHandle(storage: HandleStorage | undefined = defaultStorage()): string {
  try {
    return normalizeHandle(storage?.getItem(HANDLE_STORAGE_KEY));
  } catch {
    return DEFAULT_HANDLE;
  }
}

/** Save a handle; returns the normalized value that was stored. */
export function saveHandle(
  input: string,
  storage: HandleStorage | undefined = defaultStorage(),
): string {
  const handle = normalizeHandle(input);
  try {
    storage?.setItem(HANDLE_STORAGE_KEY, handle);
  } catch {
    // Not persisted; the session still uses it.
  }
  return handle;
}
