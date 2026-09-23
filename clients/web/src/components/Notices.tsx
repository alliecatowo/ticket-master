import { useEffect, useState } from "react";
import { useTicketmaster } from "../hooks/useTicketmaster";

/** Transient confirmations ("Accepted T-3") in the corner. */
export function Notices() {
  const { notices, dismiss } = useTicketmaster();
  return (
    <div className="notices" aria-live="polite">
      {notices.map((n) => (
        <div key={n.id} className={`notice notice--${n.tone}`} role="status">
          <span>{n.text}</span>
          <button type="button" className="notice__close" onClick={() => dismiss(n.id)} aria-label="Dismiss">
            ×
          </button>
        </div>
      ))}
    </div>
  );
}

/** A strip under the top bar while the client cannot reach `tm serve`. */
export function ConnectionBanner() {
  const { status, error, retryAt, loaded } = useTicketmaster();
  const [, tick] = useState(0);
  useEffect(() => {
    if (status === "live") return;
    const id = setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(id);
  }, [status]);
  if (status === "live" || !error) return null;
  const seconds = retryAt ? Math.max(0, Math.ceil((retryAt - Date.now()) / 1000)) : 0;
  return (
    <div className="banner" role="alert" data-testid="connection-banner">
      <strong>{loaded ? "Lost the connection to tm serve." : "Can't reach tm serve."}</strong>{" "}
      {error} {seconds > 0 ? `Retrying in ${seconds}s.` : "Retrying…"}
      {loaded && " What you see may be out of date."}
    </div>
  );
}
