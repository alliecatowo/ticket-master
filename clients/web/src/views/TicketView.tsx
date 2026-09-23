import { useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { PresenceBar } from "../components/PresenceBar";
import { TicketActions } from "../components/TicketActions";
import { StateBadge, WorkerGlyph } from "../components/TicketRow";
import { useTicketmaster } from "../hooks/useTicketmaster";
import { describeEvent } from "./timelineModel";
import {
  choicesFor,
  compactAge,
  GROUP_LABEL,
  oneLine,
  overviewOf,
  shortTitle,
  type TicketOverview,
} from "./ticketsModel";

/**
 * One ticket: objective, state and attempts, failure history, the submission and its evidence,
 * the event timeline, and the actions its state allows (the TUI's peek, as a page).
 */
export function TicketView() {
  const { id } = useParams<{ id: string }>();
  const { store, now, loaded } = useTicketmaster();
  const ticket = id ? store.tickets.get(id) : undefined;
  const overview = useMemo(() => (ticket ? overviewOf(ticket, store, now) : undefined), [ticket, store, now]);

  if (!ticket || !overview) {
    return (
      <section className="page" data-testid="ticket-view">
        <BackLink />
        <h1 className="page__title">{id}</h1>
        <p className="hint">{loaded ? "No ticket with this id in this project." : "Loading…"}</p>
      </section>
    );
  }

  return (
    <section className="page" data-testid="ticket-view">
      <BackLink />
      <header className="detail__header">
        <div className="detail__heading">
          <div className="detail__ids">
            <WorkerGlyph overview={overview} />
            <span className="detail__id">{overview.id}</span>
            <StateBadge state={overview.state} />
            <span className={`detail__group detail__group--${overview.group}`}>
              {GROUP_LABEL[overview.group]}
            </span>
          </div>
          <h1 className="detail__title">{shortTitle(oneLine(overview.objective), 96) || overview.id}</h1>
          <p className={`detail__summary tone--${overview.tone}`}>{overview.summary}</p>
        </div>
      </header>

      <ActionPanel overview={overview} />

      <div className="detail__grid">
        <div className="detail__main">
          <Card title="Objective">
            <p className="prose">{overview.objective}</p>
          </Card>

          {(overview.submission || overview.evidence.length > 0) && (
            <Card title="Submission">
              {overview.submission ? (
                <p className="prose">{overview.submission}</p>
              ) : (
                <p className="hint">{store.historyReady ? "No summary recorded." : "Reading the event log…"}</p>
              )}
              {overview.evidence.length > 0 && (
                <ul className="evidence">
                  {overview.evidence.map((e, i) => (
                    <li key={`${e.artifact}-${i}`}>
                      <span className="chip">{e.kind}</span>
                      <code>{e.artifact}</code>
                      <span className="evidence__summary">{e.summary}</span>
                      <span className="muted">by {e.produced_by}</span>
                    </li>
                  ))}
                </ul>
              )}
            </Card>
          )}

          <Card title={`Failures${overview.failures.length ? ` (${overview.failures.length})` : ""}`}>
            {overview.failures.length === 0 ? (
              <p className="hint">None recorded.</p>
            ) : (
              <ol className="failures">
                {overview.failures.map((f, i) => (
                  <li key={i}>
                    <span className="failures__attempt">Attempt {f.attempt}</span>
                    <span className="chip chip--bad">{f.class.replace(/_/g, " ")}</span>
                    <span className="failures__detail">{f.detail}</span>
                    <time className="muted" dateTime={f.at} title={new Date(f.at).toLocaleString()}>
                      {compactAge(now - Date.parse(f.at))} ago
                    </time>
                  </li>
                ))}
              </ol>
            )}
          </Card>

          <Timeline ticketId={overview.id} />
        </div>

        <aside className="detail__side">
          <Card title="Details">
            <dl className="facts">
              <dt>State</dt>
              <dd>{overview.state}</dd>
              <dt>Attempts</dt>
              <dd>
                {overview.attempts} of {overview.maxAttempts}
              </dd>
              <dt>Worker</dt>
              <dd>{overview.worker ?? <span className="muted">none</span>}</dd>
              {overview.latestActivity && (
                <>
                  <dt>Latest</dt>
                  <dd className="mono">{overview.latestActivity}</dd>
                </>
              )}
              {overview.waitingSince && (
                <>
                  <dt>Waiting</dt>
                  <dd>
                    {compactAge(now - Date.parse(overview.waitingSince))}{" "}
                    {overview.waitingFor === "approval" ? "for an approval" : "since it escalated"}
                  </dd>
                </>
              )}
              <dt>Kind</dt>
              <dd>{ticket.kind}</dd>
              <dt>Priority</dt>
              <dd>{ticket.priority}</dd>
              <dt>Created</dt>
              <dd title={new Date(ticket.created).toLocaleString()}>{compactAge(now - Date.parse(ticket.created))} ago</dd>
              <dt>Updated</dt>
              <dd title={new Date(overview.updated).toLocaleString()}>{compactAge(now - Date.parse(overview.updated))} ago</dd>
              {ticket.milestone && (
                <>
                  <dt>Milestone</dt>
                  <dd>{ticket.milestone}</dd>
                </>
              )}
              {ticket.parent && (
                <>
                  <dt>Parent</dt>
                  <dd>
                    <Link to={`/ticket/${ticket.parent}`}>{ticket.parent}</Link>
                  </dd>
                </>
              )}
              {ticket.children.length > 0 && (
                <>
                  <dt>Children</dt>
                  <dd>
                    {ticket.children.map((c, i) => (
                      <span key={c}>
                        {i > 0 && ", "}
                        <Link to={`/ticket/${c}`}>{c}</Link>
                      </span>
                    ))}
                  </dd>
                </>
              )}
              {ticket.dependencies.length > 0 && (
                <>
                  <dt>Depends on</dt>
                  <dd>
                    {ticket.dependencies.map((d, i) => (
                      <span key={d}>
                        {i > 0 && ", "}
                        <Link to={`/ticket/${d}`}>{d}</Link>
                      </span>
                    ))}
                  </dd>
                </>
              )}
            </dl>
          </Card>
          <Card title="Presence">
            <PresenceBar ticket={overview.id} />
          </Card>
        </aside>
      </div>
    </section>
  );
}

function BackLink() {
  return (
    <Link to="/" className="back">
      ← Tickets
    </Link>
  );
}

function Card({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="card">
      <h2 className="card__title">{title}</h2>
      {children}
    </section>
  );
}

const PANEL_COPY: Record<string, { title: string; body: (o: TicketOverview) => string }> = {
  submitted: {
    title: "Waiting for your review",
    body: () => "Accept closes the ticket. Reject sends it back to a worker with your reason.",
  },
  escalated: {
    title: "Needs your decision",
    body: (o) =>
      `The worker stopped after ${o.attempts} attempt${o.attempts === 1 ? "" : "s"}. Retry gives it a fresh round of attempts; guidance is added to the objective for the next one.`,
  },
  draft: {
    title: "Draft, not queued",
    body: () => "Queue it and a background worker picks it up.",
  },
};

/** The state's choices up front, like the TUI's peek, with Cancel beside them. */
function ActionPanel({ overview }: { overview: TicketOverview }) {
  const copy = PANEL_COPY[overview.state];
  const hasChoices = choicesFor(overview.state).length > 0;
  if (!hasChoices) {
    return (
      <div className="panel panel--quiet">
        <TicketActions overview={overview} />
      </div>
    );
  }
  return (
    <div className={`panel panel--${overview.group}`} data-testid="action-panel">
      {copy && (
        <div className="panel__copy">
          <p className="panel__title">{copy.title}</p>
          <p className="panel__body">{copy.body(overview)}</p>
        </div>
      )}
      <TicketActions overview={overview} keys />
    </div>
  );
}

function Timeline({ ticketId }: { ticketId: string }) {
  const { store, now } = useTicketmaster();
  const [all, setAll] = useState(false);
  const entries = useMemo(
    () => (store.timelines.get(ticketId) ?? []).map(describeEvent).reverse(),
    [store.timelines, ticketId],
  );
  const shown = all ? entries : entries.filter((e) => !e.minor);
  const hidden = entries.length - shown.length;
  return (
    <section className="card">
      <div className="card__head">
        <h2 className="card__title">Timeline</h2>
        {(hidden > 0 || all) && (
          <button type="button" className="linkish" onClick={() => setAll(!all)}>
            {all ? "Hide bookkeeping" : `Show all ${entries.length} events`}
          </button>
        )}
      </div>
      {shown.length === 0 ? (
        <p className="hint">{store.historyReady ? "No events for this ticket yet." : "Reading the event log…"}</p>
      ) : (
        <ol className="timeline">
          {shown.map((e) => (
            <li key={e.seq} className={`timeline__item tone--${e.tone}`} data-seq={e.seq}>
              <span className="timeline__dot" aria-hidden />
              <span className="timeline__text">{e.text}</span>
              <span className="timeline__meta">
                {e.actor} ·{" "}
                <time dateTime={e.ts} title={`#${e.seq} · ${new Date(e.ts).toLocaleString()}`}>
                  {compactAge(now - Date.parse(e.ts))} ago
                </time>
              </span>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
