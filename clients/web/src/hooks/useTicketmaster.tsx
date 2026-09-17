import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { TicketmasterClient } from "../api/client";
import { ProjectStore, type ProjectStoreSnapshot } from "../api/store";
import type { PresenceSnapshot, TransitionRequest } from "../api/types";

export type ConnectionStatus = "connecting" | "live" | "reconnecting" | "error";

const ACTOR_STORAGE_KEY = "tm.web.actor";
const DEFAULT_ACTOR = "human:web";

interface TicketmasterContextValue {
  client: TicketmasterClient;
  status: ConnectionStatus;
  error: string | null;
  store: ProjectStoreSnapshot;
  presence: PresenceSnapshot;
  actor: string;
  setActor: (actor: string) => void;
  transition: (ticket: string, body: TransitionRequest) => Promise<void>;
}

const TicketmasterContext = createContext<TicketmasterContextValue | null>(null);

const EMPTY_STORE: ProjectStoreSnapshot = { head: 0, tickets: new Map(), recentEvents: [] };
const EMPTY_PRESENCE: PresenceSnapshot = { participants: [], path_leases: [] };

/**
 * Owns one `TicketmasterClient`, one `ProjectStore`, and the SSE subscription that keeps it
 * live. Every view reads from this context rather than talking to the client directly, so
 * connection lifecycle and reconnect state live in exactly one place.
 */
export function TicketmasterProvider({ children }: { children: ReactNode }) {
  const client = useMemo(() => new TicketmasterClient(), []);
  const storeRef = useRef(new ProjectStore());
  const [store, setStore] = useState<ProjectStoreSnapshot>(EMPTY_STORE);
  const [presence, setPresence] = useState<PresenceSnapshot>(EMPTY_PRESENCE);
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  const [error, setError] = useState<string | null>(null);
  const [actor, setActorState] = useState<string>(
    () => localStorage.getItem(ACTOR_STORAGE_KEY) ?? DEFAULT_ACTOR,
  );

  const setActor = useCallback((next: string) => {
    localStorage.setItem(ACTOR_STORAGE_KEY, next);
    setActorState(next);
  }, []);

  useEffect(() => {
    const controller = new AbortController();

    async function run() {
      try {
        const snapshot = await client.getState();
        storeRef.current.seed(snapshot);
        setStore(storeRef.current.snapshot());
        setStatus("live");
        setError(null);

        for await (const event of client.subscribe(snapshot.head, controller.signal)) {
          if (storeRef.current.apply(event)) setStore(storeRef.current.snapshot());
          setStatus("live");
        }
      } catch (err) {
        if (controller.signal.aborted) return;
        setStatus("error");
        setError(err instanceof Error ? err.message : String(err));
      }
    }

    run();
    return () => controller.abort();
  }, [client]);

  // Presence is polled rather than streamed: it's derived server-side from a TTL sweep
  // (`crates/tm-server/src/presence.rs`), not an event-sourced projection, so there is nothing
  // to reconcile from the event log for it.
  useEffect(() => {
    let cancelled = false;
    async function poll() {
      try {
        const snapshot = await client.getPresence();
        if (!cancelled) setPresence(snapshot);
      } catch {
        // Presence is best-effort UI chrome; a failed poll just leaves the last-known snapshot.
      }
    }
    poll();
    const id = setInterval(poll, 5000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [client]);

  const transition = useCallback(
    async (ticket: string, body: TransitionRequest) => {
      await client.transition(ticket, body);
      // Optimistic-update-then-reconcile per SPEC.md §18.2: the SSE subscription above will
      // deliver the resulting ticket.state_changed event and reconcile the store; nothing to do
      // here beyond letting the mutation's own errors propagate to the caller.
    },
    [client],
  );

  const value = useMemo<TicketmasterContextValue>(
    () => ({ client, status, error, store, presence, actor, setActor, transition }),
    [client, status, error, store, presence, actor, setActor, transition],
  );

  return <TicketmasterContext.Provider value={value}>{children}</TicketmasterContext.Provider>;
}

export function useTicketmaster(): TicketmasterContextValue {
  const ctx = useContext(TicketmasterContext);
  if (!ctx) throw new Error("useTicketmaster must be used within a TicketmasterProvider");
  return ctx;
}
