import { useMemo } from "react";
import { useTicketmaster } from "../hooks/useTicketmaster";
import { buildStatusReport } from "./statusModel";
import { PresenceBar } from "../components/PresenceBar";

const SECTIONS: Array<{ key: keyof ReturnType<typeof buildStatusReport>; label: string }> = [
  { key: "closed", label: "Closed" },
  { key: "repaired", label: "Repaired" },
  { key: "reconciled", label: "Reconciled" },
  { key: "benchmarked", label: "Benchmarked" },
  { key: "needsYou", label: "Needs you" },
];

/** The "since you left" report (SPEC.md §18.2), built from the live event stream. */
export function StatusView() {
  const { store, status } = useTicketmaster();
  const report = useMemo(() => buildStatusReport(store.recentEvents), [store.recentEvents]);

  return (
    <section className="page" data-testid="status-view">
      <h1 className="page__title">Status</h1>
      <p className="page__subtitle">
        At event #{store.head} ({status}). Built from the {store.recentEvents.length} events seen
        since this page connected (the last 200 at most); reloading the page starts over.
      </p>
      <PresenceBar />
      {SECTIONS.map(({ key, label }) => (
        <div className="status-section" key={key}>
          <h2>
            {label} <span className="status-count">{report[key].length}</span>
          </h2>
          {report[key].length === 0 ? (
            <p className="hint">Nothing here yet.</p>
          ) : (
            <ul>
              {report[key].map((event) => (
                <li key={event.seq}>
                  <code>{event.subject}</code> — {event.kind} — {event.actor}
                </li>
              ))}
            </ul>
          )}
        </div>
      ))}
    </section>
  );
}
