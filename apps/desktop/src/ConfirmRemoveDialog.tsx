import { type KeyboardEvent, useEffect, useRef } from "react";
import type { DirtyReasonDto, WorkspaceModeDto } from "./bindings";
import { Button, Dialog } from "./ui";
import { AlertTriangle } from "./ui/icons";

interface ConfirmRemoveDialogProps {
  projectId: string;
  sessionId: string;
  workspace: WorkspaceModeDto | null;
  /** Set when a backgrounded removal came back WorkspaceDirty: App re-opens
   * the dialog with the reason and it starts directly at the force stage. */
  forceReason?: DirtyReasonDto | null;
  /** Fire the removal (force=true from the force stage). Fire-and-forget:
   * App backgrounds the call, owns the follow-up (dirty re-prompt / error
   * notice), and closes this dialog — the dialog never awaits or self-closes
   * on confirm. */
  onConfirm: (force: boolean) => void;
  onClose: () => void;
}

const REASON_COPY: Record<string, string> = {
  uncommitted: "uncommitted changes",
  notOnRemote: "commits not on any remote",
  both: "uncommitted changes and commits not on any remote",
};

/** Two-stage confirm dialog for removing a session.
 *
 * Confirm stage: asks for confirmation, varying copy when workspace === "worktree".
 * Force stage: rendered when `forceReason` is set — the backgrounded first
 *   attempt returned WorkspaceDirty and App re-opened the dialog to confirm
 *   force=true. The stage is a prop, not local state: removal runs in the
 *   background, so the dirty answer arrives after this dialog has unmounted.
 *
 * Confirm fires `onConfirm` and nothing else; there is no in-flight state to
 * disable buttons over. Esc closes. Tab is trapped.
 *
 * The design-system <Dialog> is presentational (no focus trap / Esc / focus
 * restore), so this component keeps that behavior itself. */
export function ConfirmRemoveDialog({
  projectId,
  sessionId,
  workspace,
  forceReason = null,
  onConfirm,
  onClose,
}: ConfirmRemoveDialogProps) {
  const firstButtonRef = useRef<HTMLButtonElement>(null);

  // Focus the primary action button on open; restore focus on close.
  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    firstButtonRef.current?.focus();
    return () => previouslyFocused?.focus?.();
  }, []);

  /** Modal keyboard handling: Esc closes, Tab is trapped within the dialog. */
  function onKeyDown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
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

  const isForce = forceReason != null;
  const dirtyCopy = isForce ? (REASON_COPY[forceReason] ?? forceReason) : null;
  const isShared = workspace === "shared";

  const footer = (
    <>
      <Button variant="ghost" onClick={onClose}>
        Cancel
      </Button>
      <Button
        ref={firstButtonRef}
        variant={isShared && !isForce ? "primary" : "danger"}
        onClick={() => onConfirm(isForce)}
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
      onClose={onClose}
      onKeyDown={onKeyDown}
      footer={footer}
    >
      {isForce ? (
        <p>
          Session{" "}
          <strong style={{ fontFamily: "var(--font-mono)" }}>
            {projectId}/{sessionId}
          </strong>{" "}
          has {dirtyCopy}. Remove anyway?
        </p>
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
    </Dialog>
  );
}
