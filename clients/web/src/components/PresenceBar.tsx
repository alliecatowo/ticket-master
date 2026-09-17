import { useTicketmaster } from "../hooks/useTicketmaster";
import type { TicketId } from "../api/types";

/**
 * Presence + path leases for one ticket (or, with no `ticket` filter, everything active).
 * Rendered wherever work is shown, per SPEC.md §18.2: "presence and path leases are shown
 * wherever work is shown, because collision prevention is the same substrate for humans and
 * agents."
 */
export function PresenceBar({ ticket }: { ticket?: TicketId }) {
  const { presence } = useTicketmaster();
  const participants = ticket
    ? presence.participants.filter((p) => p.ticket === ticket)
    : presence.participants;
  const leases = ticket
    ? presence.path_leases.filter((l) => l.ticket === ticket)
    : presence.path_leases;

  if (participants.length === 0 && leases.length === 0) {
    return (
      <div className="presence-bar presence-bar--empty" data-testid="presence-bar">
        No active presence or path leases
      </div>
    );
  }

  return (
    <div className="presence-bar" data-testid="presence-bar">
      {participants.map((p) => (
        <span className="presence-chip" key={`${p.participant}-${p.ticket ?? "none"}`}>
          {p.participant}
          {p.action ? ` · ${p.action}` : ""}
          {p.file ? ` · ${p.file}` : ""}
        </span>
      ))}
      {leases.map((l) => (
        <span className="presence-chip presence-chip--lease" key={l.lease}>
          lease {l.lease} · {l.holder} · {l.mode} · {l.paths.join(", ")}
        </span>
      ))}
    </div>
  );
}
