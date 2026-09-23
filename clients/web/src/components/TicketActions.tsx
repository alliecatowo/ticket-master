import { useEffect, useRef, useState } from "react";
import type { TicketAction } from "../api/client";
import { useTicketmaster } from "../hooks/useTicketmaster";
import {
  canCancel,
  CHOICE_LABEL,
  CHOICE_NEEDS_TEXT,
  choicesFor,
  type Choice,
  type TicketOverview,
} from "../views/ticketsModel";

type Mode = Choice | "cancel" | null;

const PROMPT: Partial<Record<Choice, { label: string; placeholder: string; send: string }>> = {
  reject: {
    label: "What should the next attempt fix?",
    placeholder: "e.g. The tests don't cover the empty-input case.",
    send: "Send back",
  },
  retry_with_guidance: {
    label: "Guidance for the next attempt",
    placeholder: "e.g. Use the existing retry helper instead of a new loop.",
    send: "Retry with guidance",
  },
};

const DONE: Record<Choice | "cancel", string> = {
  accept: "Accepted",
  reject: "Sent back",
  retry: "Retrying",
  retry_with_guidance: "Retrying, with your guidance,",
  queue: "Queued",
  cancel: "Cancelled",
};

function actionFor(choice: Choice | "cancel", text: string): TicketAction {
  switch (choice) {
    case "accept":
      return { type: "accept" };
    case "reject":
      return { type: "reject", reason: text };
    case "retry":
      return { type: "retry" };
    case "retry_with_guidance":
      return { type: "retry", guidance: text };
    case "queue":
      return { type: "queue" };
    case "cancel":
      return { type: "cancel", reason: text };
  }
}

/**
 * The actions a ticket's state allows, as in the TUI's peek: a submission offers 1 Accept / 2
 * Reject, an escalated ticket 1 Retry / 2 Retry with guidance, a draft 1 Queue it. Reject and
 * guidance ask for words and refuse an empty answer. Cancel (any non-terminal state) asks for
 * confirmation. `only` limits the choices (Review shows accept/reject inline); `keys` lets `1`/`2`
 * pick a choice when focus is not in a text field.
 */
export function TicketActions({
  overview,
  showCancel = true,
  keys = false,
  compact = false,
}: {
  overview: TicketOverview;
  showCancel?: boolean;
  keys?: boolean;
  compact?: boolean;
}) {
  const { act, notify } = useTicketmaster();
  const [mode, setMode] = useState<Mode>(null);
  const [text, setText] = useState("");
  const [pending, setPending] = useState<Mode>(null);
  const [error, setError] = useState<string | null>(null);
  const field = useRef<HTMLTextAreaElement>(null);
  const choices = choicesFor(overview.state);
  const cancellable = showCancel && canCancel(overview.state);

  // A state change (from this action or anyone else's) resets the panel.
  useEffect(() => {
    setMode(null);
    setText("");
    setError(null);
  }, [overview.state]);

  useEffect(() => {
    if (mode && mode !== "cancel") field.current?.focus();
  }, [mode]);

  const run = async (choice: Choice | "cancel") => {
    if (pending) return;
    setPending(choice);
    setError(null);
    try {
      await act(overview.id, actionFor(choice, text));
      notify("good", `${DONE[choice]} ${overview.id}`);
      setMode(null);
      setText("");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(null);
    }
  };

  const pick = (choice: Choice) => {
    if (CHOICE_NEEDS_TEXT[choice]) {
      setMode(choice);
      setText("");
      setError(null);
    } else {
      void run(choice);
    }
  };

  useEffect(() => {
    if (!keys || mode !== null) return;
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      if (target && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName))) return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      const index = Number(e.key) - 1;
      if (Number.isInteger(index) && index >= 0 && index < choices.length) {
        e.preventDefault();
        pick(choices[index]);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  if (choices.length === 0 && !cancellable) return null;

  if (mode === "cancel") {
    return (
      <div className="actions actions--confirm" role="group" aria-label="Confirm cancel">
        <p className="actions__question">
          Cancel <strong>{overview.id}</strong>? Any attempt in progress stops, and the ticket moves
          to Completed as stopped.
        </p>
        <input
          className="field"
          value={text}
          onChange={(e) => setText(e.target.value)}
          placeholder="Reason (optional)"
          aria-label="Reason for cancelling"
        />
        <div className="actions__row">
          <button type="button" className="btn btn--danger" onClick={() => void run("cancel")} disabled={pending !== null}>
            {pending === "cancel" ? "Cancelling…" : `Cancel ${overview.id}`}
          </button>
          <button type="button" className="btn btn--ghost" onClick={() => setMode(null)} disabled={pending !== null}>
            Keep it
          </button>
        </div>
        {error && <ActionError message={error} />}
      </div>
    );
  }

  if (mode && PROMPT[mode]) {
    const prompt = PROMPT[mode]!;
    const empty = text.trim().length === 0;
    return (
      <form
        className="actions actions--prompt"
        onSubmit={(e) => {
          e.preventDefault();
          if (!empty) void run(mode);
        }}
      >
        <label className="actions__label" htmlFor={`prompt-${overview.id}`}>
          {prompt.label}
        </label>
        <textarea
          id={`prompt-${overview.id}`}
          ref={field}
          className="field"
          rows={3}
          value={text}
          placeholder={prompt.placeholder}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && !empty) {
              e.preventDefault();
              void run(mode);
            }
            if (e.key === "Escape") setMode(null);
          }}
        />
        <div className="actions__row">
          <button
            type="submit"
            className={mode === "reject" ? "btn btn--danger" : "btn btn--primary"}
            disabled={empty || pending !== null}
            title={empty ? "Say what should change first" : undefined}
          >
            {pending === mode ? "Sending…" : prompt.send}
          </button>
          <button type="button" className="btn btn--ghost" onClick={() => setMode(null)} disabled={pending !== null}>
            Back
          </button>
          <span className="actions__hint">
            <kbd>⌘</kbd>/<kbd>Ctrl</kbd>+<kbd>Enter</kbd> sends
          </span>
        </div>
        {error && <ActionError message={error} />}
      </form>
    );
  }

  return (
    <div className={`actions${compact ? " actions--compact" : ""}`}>
      <div className="actions__row">
        {choices.map((choice, i) => (
          <button
            key={choice}
            type="button"
            className={
              i === 0 ? "btn btn--primary" : choice === "reject" ? "btn btn--danger-quiet" : "btn"
            }
            onClick={() => pick(choice)}
            disabled={pending !== null}
            data-testid={`action-${choice}`}
          >
            {keys && <kbd className="btn__key">{i + 1}</kbd>}
            {pending === choice ? `${CHOICE_LABEL[choice]}…` : CHOICE_LABEL[choice]}
            {CHOICE_NEEDS_TEXT[choice] && "…"}
          </button>
        ))}
        {cancellable && (
          <button
            type="button"
            className="btn btn--ghost btn--push"
            onClick={() => {
              setMode("cancel");
              setText("");
              setError(null);
            }}
            disabled={pending !== null}
            data-testid="action-cancel"
          >
            Cancel ticket…
          </button>
        )}
      </div>
      {error && <ActionError message={error} />}
    </div>
  );
}

function ActionError({ message }: { message: string }) {
  return (
    <p className="actions__error" role="alert">
      {message}
    </p>
  );
}
