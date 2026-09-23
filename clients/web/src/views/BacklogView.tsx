import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { useTicketmaster } from "../hooks/useTicketmaster";
import {
  applyBacklogFilters,
  BACKLOG_GROUPS,
  executorRoleOf,
  groupBacklog,
  type BacklogFilters,
} from "./backlogModel";

const GROUP_LABEL: Record<(typeof BACKLOG_GROUPS)[number], string> = {
  ready: "Ready",
  blocked: "Blocked",
  active: "Active",
};

/** Ready / blocked / active, filterable by milestone, kind, executor role (SPEC.md §18.2). */
export function BacklogView() {
  const { store } = useTicketmaster();
  const tickets = useMemo(() => [...store.tickets.values()], [store.tickets]);

  const [filters, setFilters] = useState<BacklogFilters>({
    milestone: null,
    kind: null,
    executorRole: null,
  });

  const milestones = useMemo(
    () => [...new Set(tickets.map((t) => t.milestone).filter((m): m is string => !!m))].sort(),
    [tickets],
  );
  const kinds = useMemo(() => [...new Set(tickets.map((t) => t.kind))].sort(), [tickets]);
  const roles = useMemo(
    () => [...new Set(tickets.map(executorRoleOf).filter((r): r is string => !!r))].sort(),
    [tickets],
  );

  const filtered = useMemo(() => applyBacklogFilters(tickets, filters), [tickets, filters]);
  const groups = useMemo(() => groupBacklog(filtered), [filtered]);

  return (
    <section className="page" data-testid="backlog-view">
      <h1 className="page__title">Backlog</h1>
      <div className="backlog-filters">
        <FilterSelect
          label="Milestone"
          value={filters.milestone}
          options={milestones}
          onChange={(milestone) => setFilters((f) => ({ ...f, milestone }))}
        />
        <FilterSelect
          label="Kind"
          value={filters.kind}
          options={kinds}
          onChange={(kind) => setFilters((f) => ({ ...f, kind }))}
        />
        <FilterSelect
          label="Executor role"
          value={filters.executorRole}
          options={roles}
          onChange={(executorRole) => setFilters((f) => ({ ...f, executorRole }))}
        />
      </div>
      {BACKLOG_GROUPS.map((group) => (
        <div className="backlog-group" key={group}>
          <h2>
            {GROUP_LABEL[group]} <span className="status-count">{groups[group].length}</span>
          </h2>
          {groups[group].length === 0 ? (
            <p className="hint">Nothing in this group.</p>
          ) : (
            <ul className="ticket-list">
              {groups[group].map((ticket) => (
                <li key={ticket.id}>
                  <Link to={`/ticket/${encodeURIComponent(ticket.id)}`}>
                    <code>{ticket.id}</code> {ticket.objective}
                  </Link>
                  <span className="ticket-meta">
                    {ticket.kind} · {ticket.state}
                    {ticket.milestone ? ` · ${ticket.milestone}` : ""}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>
      ))}
    </section>
  );
}

function FilterSelect({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: string | null;
  options: string[];
  onChange: (value: string | null) => void;
}) {
  return (
    <label className="backlog-filters__field">
      {label}
      <select
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value || null)}
        aria-label={label}
      >
        <option value="">All</option>
        {options.map((option) => (
          <option key={option} value={option}>
            {option}
          </option>
        ))}
      </select>
    </label>
  );
}
