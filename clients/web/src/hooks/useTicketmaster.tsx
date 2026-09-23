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
import {
  describeError,
  TicketmasterClient,
  type DispatchResult,
  type TicketAction,
} from "../api/client";
import { actorFor, loadHandle, saveHandle } from "../api/identity";
import { LiveProject, type ConnectionStatus, type LiveState } from "../api/live";
import type { ProjectStoreSnapshot } from "../api/store";
import type { PresenceSnapshot, TicketId } from "../api/types";

export type { ConnectionStatus };

export interface Notice {
  id: number;
  tone: "good" | "bad" | "info";
  text: string;
}

interface TicketmasterContextValue {
  client: TicketmasterClient;
  status: ConnectionStatus;
  error: string | null;
  retryAt: number | null;
  /** Whether `GET /state` has loaded at least once. */
  loaded: boolean;
  store: ProjectStoreSnapshot;
  presence: PresenceSnapshot;
  /** The human handle; mutations act as `human:<handle>`. */
  handle: string;
  actor: string;
  setHandle: (handle: string) => void;
  /** Perform a ticket action as the current human. Throws a user-facing Error on failure. */
  act: (ticket: TicketId, action: TicketAction) => Promise<void>;
  dispatch: (objective: string) => Promise<DispatchResult>;
  /** Epoch millis, ticking every few seconds, for ages and countdowns. */
  now: number;
  notices: Notice[];
  notify: (tone: Notice["tone"], text: string) => void;
  dismiss: (id: number) => void;
  /** Skip the reconnect backoff and try again now. */
  retryNow: () => void;
}

const TicketmasterContext = createContext<TicketmasterContextValue | null>(null);

const EMPTY_PRESENCE: PresenceSnapshot = { participants: [], path_leases: [] };

/**
 * Owns one `TicketmasterClient` and one `LiveProject` (the `/state` + `/events` loop that keeps
 * the store live and reconnects). Every view reads from this context, so connection lifecycle,
 * reconnect state and identity live in exactly one place.
 */
export function TicketmasterProvider({
  children,
  client: injected,
}: {
  children: ReactNode;
  client?: TicketmasterClient;
}) {
  const client = useMemo(
    () => injected ?? new TicketmasterClient({ handle: loadHandle() }),
    [injected],
  );
  const live = useMemo(() => new LiveProject(client), [client]);
  const [liveState, setLiveState] = useState<LiveState>(() => ({
    status: "connecting",
    error: null,
    retryAt: null,
    loaded: false,
    store: live.store.snapshot(),
  }));
  const [presence, setPresence] = useState<PresenceSnapshot>(EMPTY_PRESENCE);
  const [handle, setHandleState] = useState(client.handle);
  const [now, setNow] = useState(() => Date.now());
  const [notices, setNotices] = useState<Notice[]>([]);
  const noticeId = useRef(0);

  useEffect(() => live.start(setLiveState), [live]);

  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 5000);
    return () => clearInterval(id);
  }, []);

  // Presence is polled rather than streamed: it's derived server-side from a TTL sweep
  // (`crates/tm-server/src/presence.rs`), not an event-sourced projection.
  useEffect(() => {
    let cancelled = false;
    async function poll() {
      try {
        const snapshot = await client.getPresence();
        if (!cancelled) setPresence(snapshot);
      } catch {
        // Best-effort chrome; a failed poll keeps the last snapshot.
      }
    }
    void poll();
    const id = setInterval(poll, 5000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [client]);

  const setHandle = useCallback(
    (next: string) => {
      const saved = saveHandle(next);
      client.handle = saved;
      setHandleState(saved);
    },
    [client],
  );

  const dismiss = useCallback((id: number) => {
    setNotices((all) => all.filter((n) => n.id !== id));
  }, []);

  const notify = useCallback(
    (tone: Notice["tone"], text: string) => {
      const id = ++noticeId.current;
      setNotices((all) => [...all.slice(-3), { id, tone, text }]);
      setTimeout(() => dismiss(id), tone === "bad" ? 9000 : 5000);
    },
    [dismiss],
  );

  const act = useCallback(
    async (ticket: TicketId, action: TicketAction) => {
      try {
        // The resulting events arrive over the stream and reconcile the store; there is no
        // separate speculative local state to drift from it.
        await client.act(ticket, action);
      } catch (err) {
        throw new Error(describeError(err));
      }
    },
    [client],
  );

  const dispatch = useCallback(
    async (objective: string) => {
      try {
        return await client.dispatch(objective);
      } catch (err) {
        throw new Error(describeError(err));
      }
    },
    [client],
  );

  const value = useMemo<TicketmasterContextValue>(
    () => ({
      client,
      status: liveState.status,
      error: liveState.error,
      retryAt: liveState.retryAt,
      loaded: liveState.loaded,
      store: liveState.store,
      presence,
      handle,
      actor: actorFor(handle),
      setHandle,
      act,
      dispatch,
      now,
      notices,
      notify,
      dismiss,
      retryNow: () => live.retryNow(),
    }),
    [client, live, liveState, presence, handle, setHandle, act, dispatch, now, notices, notify, dismiss],
  );

  return <TicketmasterContext.Provider value={value}>{children}</TicketmasterContext.Provider>;
}

export function useTicketmaster(): TicketmasterContextValue {
  const ctx = useContext(TicketmasterContext);
  if (!ctx) throw new Error("useTicketmaster must be used within a TicketmasterProvider");
  return ctx;
}
