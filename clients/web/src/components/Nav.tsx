import { useEffect, useRef, useState } from "react";
import { NavLink } from "react-router-dom";
import { useTicketmaster } from "../hooks/useTicketmaster";

const PRIMARY: Array<{ to: string; label: string; end?: boolean }> = [
  { to: "/", label: "Tickets", end: true },
  { to: "/review", label: "Review" },
  { to: "/status", label: "Status" },
  { to: "/backlog", label: "Backlog" },
];

const UNBUILT: Array<{ to: string; label: string }> = [
  { to: "/graph", label: "Graph" },
  { to: "/rooms", label: "Rooms" },
  { to: "/decisions", label: "Decisions" },
  { to: "/providers", label: "Providers" },
];

export function Nav() {
  const { store } = useTicketmaster();
  let reviewCount = 0;
  for (const t of store.tickets.values()) if (t.state === "submitted") reviewCount++;

  return (
    <header className="topbar">
      <div className="topbar__inner">
        <NavLink to="/" className="brand" aria-label="Ticketmaster home">
          <span className="brand__mark" aria-hidden>
            ▟
          </span>
          Ticketmaster
        </NavLink>
        <nav className="topnav" aria-label="Main">
          {PRIMARY.map((link) => (
            <NavLink
              key={link.to}
              to={link.to}
              end={link.end}
              className={({ isActive }) => "topnav__link" + (isActive ? " topnav__link--active" : "")}
            >
              {link.label}
              {link.to === "/review" && reviewCount > 0 && (
                <span className="topnav__count" data-testid="review-count">
                  {reviewCount}
                </span>
              )}
            </NavLink>
          ))}
          <span className="topnav__sep" aria-hidden />
          {UNBUILT.map((link) => (
            <NavLink
              key={link.to}
              to={link.to}
              className={({ isActive }) =>
                "topnav__link topnav__link--stub" + (isActive ? " topnav__link--active" : "")
              }
              title="Not built yet"
            >
              {link.label}
            </NavLink>
          ))}
        </nav>
        <div className="topbar__meta">
          <ConnectionPill />
          <IdentityChip />
        </div>
      </div>
    </header>
  );
}

function ConnectionPill() {
  const { status, error } = useTicketmaster();
  const label = status === "live" ? "Live" : status === "connecting" ? "Connecting" : "Reconnecting";
  return (
    <span className={`conn conn--${status}`} title={error ?? `connection: ${status}`}>
      <span className="conn__dot" aria-hidden />
      <span data-testid="connection-status">{label}</span>
    </span>
  );
}

/** Who this browser acts as: `human:<handle>`, editable, saved in localStorage. */
function IdentityChip() {
  const { handle, actor, setHandle } = useTicketmaster();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(handle);
  const input = useRef<HTMLInputElement>(null);
  const cancelled = useRef(false);

  useEffect(() => {
    if (editing) input.current?.select();
  }, [editing]);

  if (!editing) {
    return (
      <button
        type="button"
        className="identity"
        onClick={() => {
          setDraft(handle);
          cancelled.current = false;
          setEditing(true);
        }}
        title="Accept, reject and retry are recorded under this name. Click to change it."
        data-testid="identity"
      >
        <span className="identity__label">acting as</span>
        <span className="identity__actor">{actor}</span>
      </button>
    );
  }

  const save = () => {
    if (cancelled.current) return;
    setHandle(draft);
    setEditing(false);
  };
  return (
    <form
      className="identity identity--editing"
      onSubmit={(e) => {
        e.preventDefault();
        save();
      }}
    >
      <span className="identity__prefix">human:</span>
      <input
        ref={input}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={save}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            cancelled.current = true;
            setEditing(false);
          }
        }}
        aria-label="Your name"
        placeholder="web"
        data-testid="identity-input"
        size={Math.max(6, draft.length + 1)}
      />
    </form>
  );
}
