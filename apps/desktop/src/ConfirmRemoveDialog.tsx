import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import type { WorkspaceModeDto } from "./bindings";
import type { RemoveResult } from "./session-store";

interface ConfirmRemoveDialogProps {
  projectId: string;
  sessionId: string;
  workspace: WorkspaceModeDto | null;
  onConfirm: (force: boolean) => Promise<RemoveResult>;
  onClose: () => void;
}

const REASON_COPY: Record<string, string> = {
  uncommitted: "uncommitted changes",
  notOnRemote: "commits not on any remote",
  both: "uncommitted changes and commits not on any remote",
};

/** Two-stage confirm dialog for removing a session.
 *
 * Stage 1: asks for confirmation, varying copy when workspace === "worktree".
 * Stage 2 (force): shown only when the first attempt returns {ok:false, dirty};
 *   lets the user confirm with force=true.
 *
 * Buttons are disabled while a call is in flight. Esc closes. Tab is trapped. */
export function ConfirmRemoveDialog({
  projectId,
  sessionId,
  workspace,
  onConfirm,
  onClose,
}: ConfirmRemoveDialogProps) {
  type Stage = { kind: "confirm" } | { kind: "force"; dirtyReason: string };
  const [stage, setStage] = useState<Stage>({ kind: "confirm" });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const firstButtonRef = useRef<HTMLButtonElement>(null);

  // Focus the primary action button on open; restore focus on close.
  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    firstButtonRef.current?.focus();
    return () => previouslyFocused?.focus?.();
  }, []);

  // Re-focus the primary action button when transitioning to stage 2.
  useEffect(() => {
    if (stage.kind === "force") {
      firstButtonRef.current?.focus();
    }
  }, [stage]);

  async function handleConfirm(force: boolean) {
    if (busy) return;
    setBusy(true);
    setError(null);
    let result: RemoveResult;
    try {
      result = await onConfirm(force);
    } catch (err) {
      setBusy(false);
      setError("An unexpected error occurred.");
      console.error("removeSession threw", err);
      return;
    }
    setBusy(false);
    if (result.ok) {
      onClose();
      return;
    }
    if (!force && "dirty" in result && result.dirty) {
      // Escalate to the force stage with the reason from the server.
      setStage({
        kind: "force",
        dirtyReason: REASON_COPY[result.dirty] ?? result.dirty,
      });
      return;
    }
    // Error from a force attempt (or unexpected non-dirty error on first attempt).
    const msg =
      result.error instanceof Error
        ? result.error.message
        : typeof result.error === "string"
          ? result.error
          : "Could not remove the session.";
    setError(msg);
  }

  /** Modal keyboard handling: Esc closes, Tab is trapped within the dialog. */
  function onKeyDown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      if (!busy) onClose();
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

  const worktreeNote =
    workspace === "worktree" ? " and deletes its worktree and branch." : ".";

  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: backdrop key handling
    <div className="dialog-backdrop" onKeyDown={onKeyDown}>
      <div
        ref={dialogRef}
        className="dialog"
        role="dialog"
        aria-modal="true"
        aria-label={
          stage.kind === "confirm" ? "Remove session" : "Confirm force remove"
        }
      >
        {stage.kind === "confirm" ? (
          <>
            <h2>Remove session</h2>
            <p>
              Remove session{" "}
              <strong>
                {projectId}/{sessionId}
              </strong>
              ? This kills tmux{worktreeNote}
            </p>
            {error && (
              <p className="dialog-error" role="alert">
                {error}
              </p>
            )}
            <div className="dialog-actions">
              <button type="button" onClick={onClose} disabled={busy}>
                Cancel
              </button>
              <button
                ref={firstButtonRef}
                type="button"
                onClick={() => void handleConfirm(false)}
                disabled={busy}
              >
                {busy ? "Removing…" : "Remove"}
              </button>
            </div>
          </>
        ) : (
          <>
            <h2>Remove anyway?</h2>
            <p>This session has {stage.dirtyReason}. Remove anyway?</p>
            {error && (
              <p className="dialog-error" role="alert">
                {error}
              </p>
            )}
            <div className="dialog-actions">
              <button type="button" onClick={onClose} disabled={busy}>
                Cancel
              </button>
              <button
                ref={firstButtonRef}
                type="button"
                onClick={() => void handleConfirm(true)}
                disabled={busy}
              >
                {busy ? "Removing…" : "Remove anyway"}
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
