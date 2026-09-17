import { NavLink } from "react-router-dom";
import { useTicketmaster } from "../hooks/useTicketmaster";

const LINKS: Array<{ to: string; label: string; built: boolean }> = [
  { to: "/status", label: "Status", built: true },
  { to: "/backlog", label: "Backlog", built: true },
  { to: "/graph", label: "Graph", built: false },
  { to: "/rooms", label: "Rooms", built: false },
  { to: "/review", label: "Review", built: false },
  { to: "/decisions", label: "Decisions", built: false },
  { to: "/providers", label: "Providers", built: false },
];

export function Nav() {
  const { status, actor, setActor } = useTicketmaster();
  return (
    <header className="app-nav">
      <div className="app-nav__brand">Ticketmaster</div>
      <nav className="app-nav__links">
        {LINKS.map((link) => (
          <NavLink
            key={link.to}
            to={link.to}
            className={({ isActive }) =>
              "app-nav__link" +
              (isActive ? " app-nav__link--active" : "") +
              (link.built ? "" : " app-nav__link--stub")
            }
          >
            {link.label}
            {!link.built && <span className="app-nav__badge">stub</span>}
          </NavLink>
        ))}
      </nav>
      <div className="app-nav__meta">
        <span className={`connection-dot connection-dot--${status}`} title={`connection: ${status}`} />
        <span data-testid="connection-status">{status}</span>
        <label className="app-nav__actor">
          acting as
          <input
            value={actor}
            onChange={(e) => setActor(e.target.value)}
            aria-label="actor id"
            data-testid="actor-input"
          />
        </label>
      </div>
    </header>
  );
}
