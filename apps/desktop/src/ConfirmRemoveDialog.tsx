import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import type { WorkspaceModeDto } from "./bindings";
import { type RemoveResult, removeErrorMessage } from "./session-store";
import { Button, Dialog } from "./ui";
import { AlertTriangle } from "./ui/icons";

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
 * Buttons are disabled while a call is in flight. Esc closes. Tab is trapped.
 *
 * The design-system <Dialog> is presentational (no focus trap / Esc / focus
 * restore), so this component keeps that behavior itself. */
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

  function requestClose() {
    if (!busy) onClose();
  }

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
    // Error from a force attempt (or unexpected non-dirty error on first
    // attempt). Surfaces a BridgeError's message instead of a generic string.
    setError(removeErrorMessage(result));
  }

  /** Modal keyboard handling: Esc closes, Tab is trapped within the dialog. */
  function onKeyDown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      requestClose();
      return;
    }
    if (e.key !== "Tab") return;
    const focusable = e.currentTarget.querySelectorAll<HTMLElement>(
      'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
    );
    if (focusable.length === 0) return;
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

  const isForce = stage.kind === "force";
  const isShared = workspace === "shared";

  const footer = (
    <>
      <Button variant="ghost" onClick={onClose} disabled={busy}>
        Cancel
      </Button>
      <Button
        ref={firstButtonRef}
        variant={isShared && !isForce ? "primary" : "danger"}
        onClick={() => void handleConfirm(isForce)}
        disabled={busy}
        loading={busy}
      >
        {isForce ? "Remove anyway" : isShared ? "Close" : "Remove"}
      </Button>
    </>
  );

  return (
    <Dialog
      open
      title={
        isForce
          ? "Remove anyway?"
          : isShared
            ? "Close session"
            : "Remove session"
      }
      description={
        isForce
          ? undefined
          : isShared
            ? "This closes the tmux session."
            : "This kills the tmux session and cannot be undone."
      }
      icon={<AlertTriangle size={18} />}
      onClose={requestClose}
      onKeyDown={onKeyDown}
      footer={footer}
    >
      {isForce ? (
        <p>This session has {stage.dirtyReason}. Remove anyway?</p>
      ) : isShared ? (
        <p>
          Close session{" "}
          <strong style={{ fontFamily: "var(--font-mono)" }}>
            {projectId}/{sessionId}
          </strong>
          ? This closes the tmux session.
        </p>
      ) : (
        <p>
          Remove session{" "}
          <strong style={{ fontFamily: "var(--font-mono)" }}>
            {projectId}/{sessionId}
          </strong>
          ? This kills tmux{worktreeNote}
        </p>
      )}
      {error && (
        <p role="alert" style={{ color: "var(--danger)" }}>
          {error}
        </p>
      )}
    </Dialog>
  );
}
