import { useMemo } from "react";
import { Link } from "react-router-dom";
import { TicketActions } from "../components/TicketActions";
import { useTicketmaster } from "../hooks/useTicketmaster";
import { compactAge, groupOverviews, overviewsOf } from "./ticketsModel";

/**
 * Review: every submission waiting for a human, oldest first, with its summary and evidence and
 * Accept / Reject inline. Escalated tickets are listed underneath, since they wait on a person
 * too, but their Retry lives on the ticket page.
 */
export function ReviewView() {
  const { store, now, loaded } = useTicketmaster();
  const groups = useMemo(() => groupOverviews(overviewsOf(store, now)), [store, now]);
  const review = [...groups.review].reverse();
  const needsInput = groups.needs_input;

  return (
    <section className="page" data-testid="review-view">
      <header className="page__header">
        <div>
          <h1 className="page__title">Review</h1>
          <p className="page__subtitle">
            {!loaded
              ? "Loading…"
              : review.length === 0
                ? "Nothing is waiting for review"
                : `${review.length} submission${review.length === 1 ? "" : "s"} waiting for you`}
          </p>
        </div>
      </header>

      {loaded && review.length === 0 && (
        <div className="empty">
          <p className="empty__title">You're all caught up</p>
          <p className="empty__body">
            When a worker submits work it lands here with its summary and evidence, for you to
            accept or send back.
          </p>
        </div>
      )}

      <ul className="reviews">
        {review.map((o) => (
          <li key={o.id} className="review" data-testid={`review-${o.id}`}>
            <div className="review__head">
              <Link to={`/ticket/${o.id}`} className="review__id">
                {o.id}
              </Link>
              <Link to={`/ticket/${o.id}`} className="review__title">
                {o.title}
              </Link>
              <span className="review__age muted">
                submitted {o.submittedAt ? `${compactAge(now - Date.parse(o.submittedAt))} ago` : ""} · attempt{" "}
                {o.attempts}
              </span>
            </div>
            <p className="review__summary">{o.submission ?? <span className="muted">{store.historyReady ? "No summary." : "Reading the event log…"}</span>}</p>
            {o.evidence.length > 0 && (
              <ul className="evidence evidence--inline">
                {o.evidence.map((e, i) => (
                  <li key={`${e.artifact}-${i}`}>
                    <span className="chip">{e.kind}</span>
                    <code>{e.artifact}</code>
                    <span className="evidence__summary">{e.summary}</span>
                  </li>
                ))}
              </ul>
            )}
            <details className="review__objective">
              <summary>Objective</summary>
              <p className="prose">{o.objective}</p>
            </details>
            <TicketActions overview={o} showCancel={false} compact />
          </li>
        ))}
      </ul>

      {needsInput.length > 0 && (
        <section className="also">
          <h2 className="also__title">Also waiting on you</h2>
          <ul className="also__list">
            {needsInput.map((o) => (
              <li key={o.id}>
                <Link to={`/ticket/${o.id}`}>
                  <code>{o.id}</code> {o.title}
                </Link>
                <span className="muted"> — {o.summary}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </section>
  );
}
