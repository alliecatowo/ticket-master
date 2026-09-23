import { useState } from "react";
import { Link } from "react-router-dom";
import { useTicketmaster } from "../hooks/useTicketmaster";

/**
 * The dispatch input: describe a task, and it becomes a work ticket that is queued for a
 * background worker (create + activate, like the TUI's dispatch input). Enter sends,
 * Shift+Enter adds a newline.
 */
export function DispatchBox() {
  const { dispatch, notify, status } = useTicketmaster();
  const [text, setText] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [last, setLast] = useState<{ id: string; queued: boolean; note?: string } | null>(null);

  const send = async () => {
    const objective = text.trim();
    if (!objective || pending) return;
    setPending(true);
    setError(null);
    try {
      const result = await dispatch(objective);
      setText("");
      if (result.queued) {
        setLast({ id: result.ticket.id, queued: true });
        notify("good", `Dispatched ${result.ticket.id} to a background worker`);
      } else {
        setLast({ id: result.ticket.id, queued: false, note: result.queueError });
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(false);
    }
  };

  return (
    <form
      className="dispatch"
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
    >
      <label htmlFor="dispatch-input" className="sr-only">
        Describe a task for a background worker
      </label>
      <textarea
        id="dispatch-input"
        className="dispatch__input"
        placeholder="Describe a task for a background worker"
        value={text}
        rows={text.includes("\n") ? 4 : 2}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            void send();
          }
        }}
        disabled={pending}
        data-testid="dispatch-input"
      />
      <div className="dispatch__bar">
        <span className="dispatch__hint">
          {error ? (
            <span className="text-bad" role="alert">
              {error}
            </span>
          ) : last && !last.queued ? (
            <span className="text-warn" role="status">
              Created <Link to={`/ticket/${last.id}`}>{last.id}</Link> but couldn't queue it
              {last.note ? `: ${last.note}` : ""}. It is a draft; open it to queue it.
            </span>
          ) : status !== "live" ? (
            "Not connected to tm serve; dispatching will fail until it's back."
          ) : (
            <span className="dispatch__keys">
              <kbd>Enter</kbd> dispatches · <kbd>Shift</kbd>+<kbd>Enter</kbd> for a new line
            </span>
          )}
        </span>
        <button type="submit" className="btn btn--primary" disabled={pending || !text.trim()}>
          {pending ? "Dispatching…" : "Dispatch"}
        </button>
      </div>
    </form>
  );
}
