// apps/desktop/src/NewSessionDialog.tsx
import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import type { OpenResult, SpawnInput } from "./session-store";
import { OPEN_CANCELLED } from "./session-store";
import { isValidSlug } from "./spawn-input";

interface NewSessionDialogProps {
  openSession: (input: SpawnInput) => Promise<OpenResult>;
  onOpened: (attached: boolean) => void;
  onClose: () => void;
}

function errorMessage(error: unknown): string {
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return "Could not open the session.";
}

/** Thin modal shell. Owns the connecting + error UI; never leaves an orphan tab. */
export function NewSessionDialog({
  openSession,
  onOpened,
  onClose,
}: NewSessionDialogProps) {
  const [projectId, setProjectId] = useState("");
  const [sessionId, setSessionId] = useState("");
  const [agent, setAgent] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const firstFieldRef = useRef<HTMLInputElement>(null);

  const agentOk = agent === "" || isValidSlug(agent);
  const valid = isValidSlug(projectId) && isValidSlug(sessionId) && agentOk;

  // Focus the first field on open; restore focus to the opener on close.
  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    firstFieldRef.current?.focus();
    return () => previouslyFocused?.focus?.();
  }, []);

  async function submit() {
    if (!valid || submitting) return;
    setSubmitting(true);
    setError(null);
    const result = await openSession({
      projectId,
      sessionId,
      agent: agent === "" ? null : agent,
    });
    if (result.ok) {
      onOpened(result.attached);
      return; // App closes the dialog
    }
    setSubmitting(false);
    // OPEN_CANCELLED means the open was cancelled externally (e.g. app teardown,
    // which unmounts this dialog anyway). Show nothing for it; surface real errors.
    if (result.error !== OPEN_CANCELLED) {
      setError(errorMessage(result.error));
    }
  }

  // Esc closes; Tab is trapped within the dialog.
  function onKeyDown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
      return;
    }
    if (e.key !== "Tab") return;
    const focusable = dialogRef.current?.querySelectorAll<HTMLElement>(
      'button, input, select, textarea, a[href], [tabindex]:not([tabindex="-1"])',
    );
    if (!focusable || focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }

  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: backdrop key handling
    <div className="dialog-backdrop" onKeyDown={onKeyDown}>
      <div
        ref={dialogRef}
        className="dialog"
        role="dialog"
        aria-modal="true"
        aria-label="New session"
      >
        <h2>New session</h2>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
        >
          <label>
            Project
            <input
              ref={firstFieldRef}
              value={projectId}
              onChange={(e) => setProjectId(e.target.value)}
            />
          </label>
          <label>
            Session
            <input
              value={sessionId}
              onChange={(e) => setSessionId(e.target.value)}
            />
          </label>
          <label>
            Agent (optional)
            <input value={agent} onChange={(e) => setAgent(e.target.value)} />
            <span className="hint">
              Applies only when spawning a new session.
            </span>
          </label>
          {error && (
            <p className="dialog-error" role="alert">
              {error}
            </p>
          )}
          <div className="dialog-actions">
            <button type="button" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" disabled={!valid || submitting}>
              {submitting ? "Connecting…" : "Open"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
