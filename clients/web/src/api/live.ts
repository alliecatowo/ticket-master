import { describeError, type TicketmasterClient } from "./client";
import { ProjectStore, type ProjectStoreSnapshot } from "./store";

export type ConnectionStatus = "connecting" | "live" | "reconnecting";

export interface LiveState {
  status: ConnectionStatus;
  /** Why the last connection attempt failed or dropped, while reconnecting. */
  error: string | null;
  /** When the next reconnect attempt starts (epoch millis), while reconnecting. */
  retryAt: number | null;
  /** Whether a `GET /state` snapshot has ever loaded (before that, "no tickets" means "unknown"). */
  loaded: boolean;
  store: ProjectStoreSnapshot;
}

export interface LiveProjectOptions {
  /** Delays between reconnect attempts, in ms; the last one repeats. */
  backoffMs?: number[];
  /** How long to coalesce store changes before notifying (the initial replay is many events). */
  flushMs?: number;
  /** Injectable clock for `retryAt`. */
  now?: () => number;
}

const DEFAULT_BACKOFF = [1000, 2000, 5000, 10000];

/**
 * Keeps a `ProjectStore` live against `tm serve`: read `GET /state`, stream `GET /events` from
 * where the store left off, and when the stream errors or ends, wait and do both again.
 *
 * Every (re)connect re-reads `/state` so the materialized tickets are the server's, not a
 * reconstruction; the stream resumes from `store.cursor()`, so history (activity, timelines) is
 * read once and never duplicated. Framework-agnostic: React sees it only through `start`'s
 * listener (`hooks/useTicketmaster.tsx`).
 */
export class LiveProject {
  readonly store: ProjectStore;
  private readonly backoff: number[];
  private readonly flushMs: number;
  private readonly now: () => number;

  constructor(
    private readonly client: TicketmasterClient,
    options: LiveProjectOptions = {},
    store: ProjectStore = new ProjectStore(),
  ) {
    this.store = store;
    this.backoff = options.backoffMs?.length ? options.backoffMs : DEFAULT_BACKOFF;
    this.flushMs = options.flushMs ?? 50;
    this.now = options.now ?? Date.now;
  }

  /** Start the loop; `listener` gets every state change. Returns a function that stops it. */
  start(listener: (state: LiveState) => void): () => void {
    const controller = new AbortController();
    const signal = controller.signal;
    let state: LiveState = {
      status: "connecting",
      error: null,
      retryAt: null,
      loaded: false,
      store: this.store.snapshot(),
    };
    let flushTimer: ReturnType<typeof setTimeout> | null = null;

    const emit = (patch: Partial<LiveState>) => {
      if (signal.aborted) return;
      state = { ...state, ...patch };
      listener(state);
    };
    const flushSoon = () => {
      if (flushTimer !== null || signal.aborted) return;
      flushTimer = setTimeout(() => {
        flushTimer = null;
        emit({ store: this.store.snapshot() });
      }, this.flushMs);
    };

    const run = async () => {
      let attempt = 0;
      while (!signal.aborted) {
        try {
          const snapshot = await this.client.getState();
          if (signal.aborted) return;
          this.store.seed(snapshot);
          attempt = 0;
          emit({
            status: "live",
            error: null,
            retryAt: null,
            loaded: true,
            store: this.store.snapshot(),
          });
          for await (const event of this.client.streamEvents(this.store.cursor(), signal)) {
            if (this.store.apply(event)) flushSoon();
          }
          if (signal.aborted) return;
          // A clean end is still a lost connection (the server restarted or shut down).
          throw new Error("The event stream closed.");
        } catch (err) {
          if (signal.aborted) return;
          const delay = this.backoff[Math.min(attempt, this.backoff.length - 1)];
          attempt += 1;
          emit({
            status: state.loaded ? "reconnecting" : "connecting",
            error: describeError(err),
            retryAt: this.now() + delay,
            store: this.store.snapshot(),
          });
          await sleep(delay, signal);
        }
      }
    };

    void run();
    return () => {
      controller.abort();
      if (flushTimer !== null) clearTimeout(flushTimer);
    };
  }
}

/** Resolves after `ms`, or immediately when `signal` aborts. */
function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve();
    const timer = setTimeout(done, ms);
    function done() {
      clearTimeout(timer);
      signal.removeEventListener("abort", done);
      resolve();
    }
    signal.addEventListener("abort", done);
  });
}
