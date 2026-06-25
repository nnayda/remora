import { type KeyboardEvent, useEffect, useMemo, useState } from "react";
import type { ConfigDto } from "./bindings";
import { buildNewSessionModel, resolveSelection } from "./new-session-model";
import type { OpenResult, SpawnInput } from "./session-store";
import { OPEN_CANCELLED } from "./session-store";
import { isValidSlug, normalizeSlugInput } from "./spawn-input";
import { Button, Dialog, Input, Select } from "./ui";
import { Terminal } from "./ui/icons";

/** Stable id for the dialog panel so the focus trap can query within it
 * (the presentational <Dialog> renders its own scrim/panel and forwards
 * neither a ref nor focus/Esc handling — this component owns that). */
const DIALOG_ID = "new-session-dialog";
const FOCUSABLE =
  'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])';

interface NewSessionDialogProps {
  /** Per-device config; drives the project and agent pickers. */
  config: ConfigDto;
  /** Project to pre-scope to (e.g. a sidebar per-project "+"). Clamped to a
   * real project — a stale/unknown id falls back to the first project, same as
   * the global "+ New session" entry point (which passes nothing). */
  initialProjectId?: string;
  openSession: (input: SpawnInput) => Promise<OpenResult>;
  onOpened: (attached: boolean) => void;
  onClose: () => void;
}

/** Best-effort human message from an unknown thrown open-session error. */
function errorMessage(error: unknown): string {
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return "Could not open the session.";
}

/** Thin modal shell. Owns the connecting + error UI; never leaves an orphan tab. */
export function NewSessionDialog({
  config,
  initialProjectId = "",
  openSession,
  onOpened,
  onClose,
}: NewSessionDialogProps) {
  const model = useMemo(() => buildNewSessionModel(config), [config]);
  const initial = resolveSelection(model, initialProjectId);
  const [projectId, setProjectId] = useState(initial.projectId);
  const [sessionId, setSessionId] = useState("");
  // Agent defaults to the selected project's default; project changes reset it.
  const [agent, setAgent] = useState(initial.agent);
  const [base, setBase] = useState("");
  const [workspace, setWorkspace] = useState(initial.workspace);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const selectedProject =
    model.projects.find((p) => p.id === projectId) ?? null;
  // Membership guards (not mere non-empty) so a stale selection that matches no
  // option can't submit a project/agent the config no longer has.
  const valid =
    selectedProject !== null &&
    model.agents.includes(agent) &&
    isValidSlug(sessionId);

  // Focus the first field on open; restore focus to the opener on close. Prefers
  // the first focusable control inside the dialog body (the project <select>),
  // falling back to the panel's first focusable (e.g. the × / Close button in
  // the no-projects state, where the body has no focusable control).
  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const panel = document.getElementById(DIALOG_ID);
    const target =
      panel
        ?.querySelector(".rmra-dialog__body")
        ?.querySelector<HTMLElement>(FOCUSABLE) ??
      panel?.querySelector<HTMLElement>(FOCUSABLE);
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
      setWorkspace(sel.workspace);
      // Clear a typed base so a ref entered for the gone project isn't
      // silently submitted against its replacement (empty → that project's
      // configured/detected default).
      setBase("");
    }
  }, [model, projectId]);

  /** Switch the selected project and reset the agent, workspace, and base to
   * that project's defaults (an empty base falls through to the project's
   * configured/detected default rather than carrying a stale ref across). */
  function selectProject(id: string) {
    const sel = resolveSelection(model, id);
    setProjectId(sel.projectId);
    setAgent(sel.agent);
    setWorkspace(sel.workspace);
    setBase("");
  }

  /** Validate, open the session, and reflect the outcome (success closes the
   * dialog; a real error shows inline; the button never sticks on "Connecting…"). */
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
        base: base.trim() === "" ? null : base.trim(),
        workspace,
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

  /** Modal keyboard handling: Esc closes, Tab is trapped within the dialog.
   * The presentational <Dialog> handles neither, so we wire it here on the
   * panel (this handler is attached to the dialog panel via the spread). */
  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
      return;
    }
    if (e.key !== "Tab") return;
    const focusable = e.currentTarget.querySelectorAll<HTMLElement>(FOCUSABLE);
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

  const hasProjects = model.projects.length > 0;
  const createLabel = submitting ? "Connecting…" : "Open";

  const footer = hasProjects ? (
    <>
      <Button variant="ghost" onClick={onClose}>
        Cancel
      </Button>
      <Button
        type="submit"
        form="new-session-form"
        variant="primary"
        disabled={!valid}
        loading={submitting}
      >
        {createLabel}
      </Button>
    </>
  ) : (
    <Button variant="ghost" onClick={onClose}>
      Close
    </Button>
  );

  return (
    <Dialog
      open
      id={DIALOG_ID}
      title="Start a new session"
      description="Runs in a fresh remote sandbox attached to a workspace."
      icon={<Terminal size={18} />}
      onClose={onClose}
      onKeyDown={onKeyDown}
      footer={footer}
    >
      {!hasProjects ? (
        <p className="rmra-field__hint rmra-field__hint--error" role="alert">
          No projects configured. Add a project to your config.toml to start a
          session.
        </p>
      ) : (
        <form
          id="new-session-form"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "var(--space-8)",
          }}
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
        >
          <div className="rmra-field">
            <Select
              label="Project"
              value={projectId}
              onChange={(value) => selectProject(value)}
              options={model.projects.map((p) => ({
                value: p.id,
                label: `${p.label} (${p.hostLabel})`,
              }))}
            />
            {selectedProject && (
              <span className="rmra-field__hint">
                Host: {selectedProject.hostLabel}
              </span>
            )}
          </div>
          <Input
            label="Session"
            value={sessionId}
            mono
            onChange={(e) => setSessionId(normalizeSlugInput(e.target.value))}
          />
          <div className="rmra-field">
            <Select
              label="Agent"
              value={agent}
              onChange={(value) => setAgent(value)}
              options={model.agents.map((id) => ({
                value: id,
                label:
                  selectedProject?.defaultAgent === id ? `${id} (default)` : id,
              }))}
            />
            <span className="rmra-field__hint">
              Applies only when spawning a new session.
            </span>
          </div>
          <Input
            label="Base"
            value={base}
            mono
            placeholder="origin/main (auto-detected if empty)"
            hint="Start-point for the new worktree. Empty = project default / detected."
            onChange={(e) => setBase(e.target.value)}
          />
          <div className="rmra-field">
            <Select
              label="Workspace"
              value={workspace}
              onChange={(value) => setWorkspace(value as typeof workspace)}
              options={[
                {
                  value: "worktree",
                  label: `worktree${
                    selectedProject?.defaultWorkspace === "worktree"
                      ? " (default)"
                      : ""
                  }`,
                },
                {
                  value: "shared",
                  label: `shared${
                    selectedProject?.defaultWorkspace === "shared"
                      ? " (default)"
                      : ""
                  }`,
                },
              ]}
            />
            {workspace === "shared" && (
              <span className="rmra-field__hint">
                Shared sessions reuse the project directory and can clobber each
                other.
              </span>
            )}
          </div>
          {error && (
            <p
              className="rmra-field__hint rmra-field__hint--error"
              role="alert"
            >
              {error}
            </p>
          )}
        </form>
      )}
    </Dialog>
  );
}
