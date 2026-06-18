import {
  type KeyboardEvent,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { ConfigDto } from "./bindings";
import { buildNewSessionModel, resolveSelection } from "./new-session-model";
import type { OpenResult, SpawnInput } from "./session-store";
import { OPEN_CANCELLED } from "./session-store";
import { isValidSlug } from "./spawn-input";

interface NewSessionDialogProps {
  /** Per-device config; drives the project and agent pickers. */
  config: ConfigDto;
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
  config,
  openSession,
  onOpened,
  onClose,
}: NewSessionDialogProps) {
  const model = useMemo(() => buildNewSessionModel(config), [config]);
  const initial = resolveSelection(model, "");
  const [projectId, setProjectId] = useState(initial.projectId);
  const [sessionId, setSessionId] = useState("");
  // Agent defaults to the selected project's default; project changes reset it.
  const [agent, setAgent] = useState(initial.agent);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const firstFieldRef = useRef<HTMLSelectElement>(null);

  const selectedProject =
    model.projects.find((p) => p.id === projectId) ?? null;
  // Membership guards (not mere non-empty) so a stale selection that matches no
  // option can't submit a project/agent the config no longer has.
  const valid =
    selectedProject !== null &&
    model.agents.includes(agent) &&
    isValidSlug(sessionId);

  // Focus the first field on open; restore focus to the opener on close. Falls
  // back to the first focusable control (e.g. Close in the no-projects state,
  // where the project <select> isn't rendered).
  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const target =
      firstFieldRef.current ??
      dialogRef.current?.querySelector<HTMLElement>(
        'button, select, input, [tabindex]:not([tabindex="-1"])',
      );
    target?.focus();
    return () => previouslyFocused?.focus?.();
  }, []);

  // Re-clamp the selection if config changes while the dialog is open (manual
  // refresh, or the first config load arriving after open). A no-op while
  // projectId is still valid, so a user's agent override stands.
  useEffect(() => {
    if (!model.projects.some((p) => p.id === projectId)) {
      const sel = resolveSelection(model, projectId);
      setProjectId(sel.projectId);
      setAgent(sel.agent);
    }
  }, [model, projectId]);

  function selectProject(id: string) {
    const sel = resolveSelection(model, id);
    setProjectId(sel.projectId);
    setAgent(sel.agent);
  }

  async function submit() {
    if (!valid || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      // Send null when the agent is the project default so spawn resolves the
      // live default (preserving SpawnInput's "null = project default" path);
      // an explicit override sends its id.
      const result = await openSession({
        projectId,
        sessionId,
        agent: agent === selectedProject?.defaultAgent ? null : agent,
      });
      if (result.ok) {
        onOpened(result.attached);
        return; // App closes the dialog
      }
      // OPEN_CANCELLED means the open was cancelled externally (e.g. app
      // teardown, which unmounts this dialog anyway). Show nothing for it;
      // surface real errors.
      if (result.error !== OPEN_CANCELLED) {
        setError(errorMessage(result.error));
      }
    } catch (err) {
      // openSession is contracted to resolve, not reject — this guards the
      // unexpected (e.g. a subscriber throwing during commit) so the button
      // never sticks on "Connecting…".
      console.error("Failed to open session", err);
      setError(errorMessage(err));
    } finally {
      setSubmitting(false);
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

  const hasProjects = model.projects.length > 0;

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
        {!hasProjects ? (
          <>
            <p className="dialog-error" role="alert">
              No projects configured. Add a project to your config.toml to start
              a session.
            </p>
            <div className="dialog-actions">
              <button type="button" onClick={onClose}>
                Close
              </button>
            </div>
          </>
        ) : (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void submit();
            }}
          >
            <label>
              Project
              <select
                ref={firstFieldRef}
                value={projectId}
                onChange={(e) => selectProject(e.target.value)}
              >
                {model.projects.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.label} ({p.hostLabel})
                  </option>
                ))}
              </select>
            </label>
            {selectedProject && (
              <p className="hint">Host: {selectedProject.hostLabel}</p>
            )}
            <label>
              Session
              <input
                value={sessionId}
                onChange={(e) => setSessionId(e.target.value)}
              />
            </label>
            <label>
              Agent
              <select value={agent} onChange={(e) => setAgent(e.target.value)}>
                {model.agents.map((id) => (
                  <option key={id} value={id}>
                    {id}
                    {selectedProject?.defaultAgent === id ? " (default)" : ""}
                  </option>
                ))}
              </select>
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
        )}
      </div>
    </div>
  );
}
