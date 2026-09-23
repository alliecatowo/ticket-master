import { Link } from "react-router-dom";
import type { TicketState } from "../api/types";
import { compactAge, stateWord, type TicketOverview } from "../views/ticketsModel";

/**
 * The worker glyph, as in the TUI: `✽` (animated) while a worker works it, `✻` when a worker
 * holds it without working, `∙` with no worker.
 */
export function WorkerGlyph({ overview }: { overview: TicketOverview }) {
  if (overview.working) {
    return (
      <span className="glyph glyph--working" title={`${overview.worker} is working on it`}>
        ✽
      </span>
    );
  }
  if (overview.worker) {
    return (
      <span className="glyph glyph--attached" title={`held by ${overview.worker}`}>
        ✻
      </span>
    );
  }
  return (
    <span className="glyph" aria-hidden>
      ∙
    </span>
  );
}

export function StateBadge({ state }: { state: TicketState }) {
  return <span className={`badge badge--${state}`}>{stateWord(state)}</span>;
}

/** One row of the tickets list: glyph, id, title, summary, state and age. */
export function TicketRow({ overview }: { overview: TicketOverview }) {
  return (
    <li className={`row row--${overview.group}`} data-testid={`row-${overview.id}`}>
      <Link to={`/ticket/${encodeURIComponent(overview.id)}`} className="row__link">
        <WorkerGlyph overview={overview} />
        <span className="row__id">{overview.id}</span>
        <span className="row__main">
          <span className="row__title">{overview.title}</span>
          <span className={`row__summary tone--${overview.tone}`}>{overview.summary}</span>
        </span>
        <span className="row__meta">
          <StateBadge state={overview.state} />
          <span className="row__age" title={`created ${new Date(overview.created).toLocaleString()}`}>
            {compactAge(overview.ageMs)}
          </span>
        </span>
      </Link>
    </li>
  );
}
