import { useMemo, useState } from "react";
import { useParams } from "react-router-dom";
import { useTicketmaster } from "../hooks/useTicketmaster";
import { PresenceBar } from "../components/PresenceBar";
import type { TransitionRequest } from "../api/types";

/**
 * Objective, state, authority, evidence, failures, artifacts, event history (SPEC.md §18.2).
 *
 * Evidence and artifacts are read from `store.recentEvents` (this session's live tail) plus the
 * ticket's own `failures` field — there is no dedicated "evidence for ticket X" endpoint, so a
 * ticket's full historical evidence/artifact list before this client connected is not shown here
 * (see README's "not built" list for the full caveat).
 */
export function TicketView() {
  const { id } = useParams<{ id: string }>();
  const { store, transition, actor } = useTicketmaster();
  const [pending, setPending] = useState<string | null>(null);
  const [mutationError, setMutationError] = useState<string | null>(null);

  const ticket = id ? store.tickets.get(id) : undefined;
  const history = useMemo(
    () => store.recentEvents.filter((e) => e.subject === id),
    [store.recentEvents, id],
  );

  if (!ticket) {
    return (
      <section data-testid="ticket-view">
        <h1>Ticket {id}</h1>
        <p className="hint">Not found in the current materialized view.</p>
      </section>
    );
  }

  async function run(kind: string, body: TransitionRequest) {
    setPending(kind);
    setMutationError(null);
    try {
      await transition(ticket!.id, body);
    } catch (err) {
      setMutationError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(null);
    }
  }

  return (
    <section data-testid="ticket-view">
      <h1>
        {ticket.id} <span className="ticket-state">{ticket.state}</span>
      </h1>
      <p>{ticket.objective}</p>
      <PresenceBar ticket={ticket.id} />

      <div className="ticket-actions">
        <button
          disabled={pending !== null || ticket.state !== "draft"}
          onClick={() => run("activate", { activate: { actor } })}
        >
          {pending === "activate" ? "Activating…" : "Activate"}
        </button>
        <button
          disabled={pending !== null || ticket.state === "closed" || ticket.state === "cancelled"}
          onClick={() => run("close", { close: { actor, reason: "closed from web canvas" } })}
        >
          {pending === "close" ? "Closing…" : "Close"}
        </button>
        <button
          disabled={pending !== null || ticket.state !== "closed"}
          onClick={() => run("reopen", { reopen: { actor, reason: "reopened from web canvas" } })}
        >
          {pending === "reopen" ? "Reopening…" : "Reopen"}
        </button>
      </div>
      {mutationError && <p className="error">Mutation failed: {mutationError}</p>}

      <dl className="ticket-fields">
        <dt>Kind</dt>
        <dd>{ticket.kind}</dd>
        <dt>Authority</dt>
        <dd>
          <code>{JSON.stringify(ticket.authority)}</code>
        </dd>
        <dt>Milestone</dt>
        <dd>{ticket.milestone ?? "—"}</dd>
        <dt>Priority</dt>
        <dd>{ticket.priority}</dd>
        <dt>Attempts</dt>
        <dd>{ticket.attempts}</dd>
      </dl>

      <h2>Failures</h2>
      {ticket.failures.length === 0 ? (
        <p className="hint">None recorded.</p>
      ) : (
        <ul>
          {ticket.failures.map((f, i) => (
            <li key={i}>
              <code>{JSON.stringify(f)}</code>
            </li>
          ))}
        </ul>
      )}

      <h2>Event history (this session)</h2>
      {history.length === 0 ? (
        <p className="hint">No events observed for this ticket since the client connected.</p>
      ) : (
        <ul>
          {history.map((event) => (
            <li key={event.seq}>
              #{event.seq} {event.kind} — {event.actor} — {event.ts}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
