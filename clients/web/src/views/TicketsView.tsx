import { useMemo, useState } from "react";
import { DispatchBox } from "../components/DispatchBox";
import { TicketRow } from "../components/TicketRow";
import { useTicketmaster } from "../hooks/useTicketmaster";
import {
  countsLine,
  GROUP_LABEL,
  GROUP_ORDER,
  groupOverviews,
  overviewsOf,
  type TicketGroup,
} from "./ticketsModel";

const COMPLETED_FOLD = 5;

const GROUP_HINT: Record<TicketGroup, string> = {
  needs_input: "Escalated, or waiting on an approval",
  working: "A worker holds it",
  review: "Submitted work waiting for you",
  queued: "Waiting for a worker",
  completed: "Closed or stopped",
};

/**
 * The home screen: Claude Code's agent view with tickets as rows (D-019 §2), grouped Needs input
 * / Working / Ready for review / Queued / Completed, with a dispatch box that creates and queues
 * a ticket for a background worker.
 */
export function TicketsView() {
  const { store, now, loaded, status } = useTicketmaster();
  const groups = useMemo(() => groupOverviews(overviewsOf(store, now)), [store, now]);
  const [collapsed, setCollapsed] = useState<Partial<Record<TicketGroup, boolean>>>({});
  const [showAllCompleted, setShowAllCompleted] = useState(false);
  const total = store.tickets.size;
  const counts = countsLine(groups);

  return (
    <section className="page" data-testid="tickets-view">
      <header className="page__header">
        <div>
          <h1 className="page__title">Tickets</h1>
          <p className="page__subtitle" data-testid="tickets-counts">
            {!loaded ? "Loading…" : total === 0 ? "No tickets yet" : counts}
          </p>
        </div>
      </header>

      <DispatchBox />

      {loaded && total === 0 && (
        <div className="empty">
          <p className="empty__title">Nothing here yet</p>
          <p className="empty__body">
            Describe a task above and press Enter. It becomes a ticket, a background worker picks it
            up, and it shows here as it moves from Queued to Working to Ready for review.
          </p>
        </div>
      )}
      {!loaded && status !== "live" && (
        <div className="empty">
          <p className="empty__title">Connecting to tm serve…</p>
          <p className="empty__body">Tickets appear as soon as the server answers.</p>
        </div>
      )}

      {GROUP_ORDER.filter((g) => groups[g].length > 0).map((group) => {
        const rows = groups[group];
        const isCollapsed = collapsed[group] ?? false;
        const folded = group === "completed" && !showAllCompleted && rows.length > COMPLETED_FOLD;
        const visible = folded ? rows.slice(0, COMPLETED_FOLD) : rows;
        return (
          <section key={group} className={`group group--${group}`} data-testid={`group-${group}`}>
            <button
              type="button"
              className="group__head"
              aria-expanded={!isCollapsed}
              onClick={() => setCollapsed((c) => ({ ...c, [group]: !isCollapsed }))}
            >
              <span className="group__chevron" aria-hidden>
                {isCollapsed ? "▸" : "▾"}
              </span>
              <span className="group__dot" aria-hidden />
              <span className="group__label">{GROUP_LABEL[group]}</span>
              <span className="group__count">{rows.length}</span>
              <span className="group__hint">{GROUP_HINT[group]}</span>
            </button>
            {!isCollapsed && (
              <ul className="rows">
                {visible.map((o) => (
                  <TicketRow key={o.id} overview={o} />
                ))}
                {folded && (
                  <li className="row row--more">
                    <button type="button" className="row__more" onClick={() => setShowAllCompleted(true)}>
                      … {rows.length - COMPLETED_FOLD} more
                    </button>
                  </li>
                )}
              </ul>
            )}
          </section>
        );
      })}
    </section>
  );
}
