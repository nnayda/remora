import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import { AgentForm } from "./AgentForm";
import type {
  AgentInputDto,
  EditorAgentDto,
  EditorHostDto,
  EditorProjectDto,
  HostInputDto,
  ProjectInputDto,
} from "./bindings";
import {
  getEditableConfig,
  insertAgent,
  insertHost,
  insertProject,
  removeAgent,
  removeHost,
  removeProject,
  updateAgent,
  updateHost,
  updateProject,
} from "./bridge";
import { buildSettingsModel, type SettingsModel } from "./config-editor-model";
import { formErrorMessage } from "./form-error";
import { HostForm } from "./HostForm";
import { ProjectForm } from "./ProjectForm";

interface SettingsDialogProps {
  /** Re-read the sidebar's (redacted) config after a mutation. */
  onConfigChanged: () => void;
  onClose: () => void;
}

/** Which body the dialog shows: the entity list, or one open entity form. */
type View =
  | { kind: "list" }
  | { kind: "host"; mode: "create" | "edit"; initial?: EditorHostDto }
  | { kind: "project"; mode: "create" | "edit"; initial?: EditorProjectDto }
  | { kind: "agent"; mode: "create" | "edit"; initial?: EditorAgentDto };

/**
 * Config management modal: list/add/edit/remove hosts, projects, and agents,
 * persisted through the editor bridge (ADR-0006). A thin shell over
 * `config-editor-model` and the entity forms; modal mechanics (focus trap, Esc,
 * inline errors) mirror `NewSessionDialog`. Component render is covered by
 * manual QA — vitest runs in node with no DOM; the model is unit-tested.
 */
export function SettingsDialog({
  onConfigChanged,
  onClose,
}: SettingsDialogProps) {
  const [model, setModel] = useState<SettingsModel | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [view, setView] = useState<View>({ kind: "list" });
  const dialogRef = useRef<HTMLDivElement>(null);

  /** Re-read the editable config and refresh the sidebar. */
  async function reload() {
    const dto = await getEditableConfig();
    setModel(buildSettingsModel(dto));
    onConfigChanged();
  }

  // Initial load. A read failure (unreadable/unparseable file) is a banner; a
  // *degraded* (semantically invalid) file still loads, into recovery mode.
  useEffect(() => {
    let live = true;
    getEditableConfig()
      .then((dto) => {
        if (live) setModel(buildSettingsModel(dto));
      })
      .catch((err) => {
        if (live) setLoadError(formErrorMessage(err));
      });
    return () => {
      live = false;
    };
  }, []);

  // Focus the dialog on open; restore focus to the opener on close.
  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    dialogRef.current?.focus();
    return () => previouslyFocused?.focus?.();
  }, []);

  // Move focus when the body switches. Entering a form lands focus on its first
  // control (Edit forms have no autoFocus — the id field is create-only); going
  // back to the list keeps focus inside the modal rather than dropping it to
  // <body> off the now-unmounted form button. Runs after the mount effect, so
  // that effect still captures the opener (not the dialog) for restore-on-close.
  // biome-ignore lint/correctness/useExhaustiveDependencies: re-run on view change
  useEffect(() => {
    if (view.kind === "list") {
      dialogRef.current?.focus();
      return;
    }
    dialogRef.current
      ?.querySelector<HTMLElement>(
        "form input, form select, form textarea, form button",
      )
      ?.focus();
  }, [view]);

  function onKeyDown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      // Esc backs out of an open form to the list; from the list it closes.
      if (view.kind === "list") onClose();
      else setView({ kind: "list" });
      return;
    }
    if (e.key !== "Tab") return;
    // Disabled controls (e.g. the argv editor's reorder/remove buttons at the
    // ends) aren't tabbable, so they must not bound the trap — else Tab off a
    // disabled first/last escapes the modal.
    const focusable = Array.from(
      dialogRef.current?.querySelectorAll<HTMLElement>(
        'button, input, select, textarea, a[href], [tabindex]:not([tabindex="-1"])',
      ) ?? [],
    ).filter((el) => !el.hasAttribute("disabled"));
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

  /** Run a mutation, then reload + return to the list. Errors propagate so the
   * caller (a form, or the remove handler) can show them inline. */
  async function afterMutation() {
    await reload();
    setView({ kind: "list" });
  }

  async function submitHost(id: string, input: HostInputDto) {
    if (view.kind !== "host") return;
    if (view.mode === "create") await insertHost(id, input);
    else await updateHost(id, input);
    await afterMutation();
  }
  async function submitProject(id: string, input: ProjectInputDto) {
    if (view.kind !== "project") return;
    if (view.mode === "create") await insertProject(id, input);
    else await updateProject(id, input);
    await afterMutation();
  }
  async function submitAgent(id: string, input: AgentInputDto) {
    if (view.kind !== "agent") return;
    if (view.mode === "create") await insertAgent(id, input);
    else await updateAgent(id, input);
    await afterMutation();
  }

  /** Remove an entry; a rejection (e.g. a referenced host, or a delete that
   * leaves a degraded file still invalid) shows inline above the list. */
  function remove(fn: (id: string) => Promise<void>, id: string) {
    setListError(null);
    fn(id)
      .then(() => reload())
      .catch((err) => setListError(formErrorMessage(err)));
  }

  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: backdrop key handling
    <div className="dialog-backdrop" onKeyDown={onKeyDown}>
      <div
        ref={dialogRef}
        className="dialog settings-dialog"
        role="dialog"
        aria-modal="true"
        aria-label="Settings"
        tabIndex={-1}
      >
        <h2>Settings</h2>
        {loadError ? (
          <>
            <p className="dialog-error" role="alert">
              Could not read config: {loadError}
            </p>
            <div className="dialog-actions">
              <button type="button" onClick={onClose}>
                Close
              </button>
            </div>
          </>
        ) : model === null ? (
          <p className="hint">Loading…</p>
        ) : view.kind === "host" ? (
          <HostForm
            // Keyed by identity so a switch to a different entity remounts with
            // fresh state instead of reusing the once-initialized form.
            key={`host-${view.mode}-${view.initial?.id ?? "new"}`}
            mode={view.mode}
            initial={view.initial}
            onSubmit={submitHost}
            onCancel={() => setView({ kind: "list" })}
          />
        ) : view.kind === "project" ? (
          <ProjectForm
            key={`project-${view.mode}-${view.initial?.id ?? "new"}`}
            mode={view.mode}
            initial={view.initial}
            hostIds={model.hosts.map((h) => h.id)}
            agentIds={model.agents.map((a) => a.id)}
            onSubmit={submitProject}
            onCancel={() => setView({ kind: "list" })}
          />
        ) : view.kind === "agent" ? (
          <AgentForm
            key={`agent-${view.mode}-${view.initial?.id ?? "new"}`}
            mode={view.mode}
            initial={view.initial}
            onSubmit={submitAgent}
            onCancel={() => setView({ kind: "list" })}
          />
        ) : (
          <SettingsList
            model={model}
            listError={listError}
            onAdd={(kind) => {
              setListError(null);
              setView({ kind, mode: "create" });
            }}
            onEditHost={(initial) =>
              setView({ kind: "host", mode: "edit", initial })
            }
            onEditProject={(initial) =>
              setView({ kind: "project", mode: "edit", initial })
            }
            onEditAgent={(initial) =>
              setView({ kind: "agent", mode: "edit", initial })
            }
            onRemoveHost={(id) => remove(removeHost, id)}
            onRemoveProject={(id) => remove(removeProject, id)}
            onRemoveAgent={(id) => remove(removeAgent, id)}
            onClose={onClose}
          />
        )}
      </div>
    </div>
  );
}

interface SettingsListProps {
  model: SettingsModel;
  listError: string | null;
  onAdd: (kind: "host" | "project" | "agent") => void;
  onEditHost: (initial: EditorHostDto) => void;
  onEditProject: (initial: EditorProjectDto) => void;
  onEditAgent: (initial: EditorAgentDto) => void;
  onRemoveHost: (id: string) => void;
  onRemoveProject: (id: string) => void;
  onRemoveAgent: (id: string) => void;
  onClose: () => void;
}

/** The list body: three entity sections, or the degraded-recovery view. */
function SettingsList({
  model,
  listError,
  onAdd,
  onEditHost,
  onEditProject,
  onEditAgent,
  onRemoveHost,
  onRemoveProject,
  onRemoveAgent,
  onClose,
}: SettingsListProps) {
  if (model.degraded) {
    return (
      <DegradedRecovery
        model={model}
        listError={listError}
        onRemoveHost={onRemoveHost}
        onRemoveProject={onRemoveProject}
        onRemoveAgent={onRemoveAgent}
        onClose={onClose}
      />
    );
  }
  return (
    <div className="settings-body">
      {listError && (
        <p className="dialog-error" role="alert">
          {listError}
        </p>
      )}
      <Section title="Hosts" onAdd={() => onAdd("host")}>
        {model.hosts.map((h) => (
          <EntityRow
            key={h.id}
            label={h.name ? `${h.name} (${h.id})` : h.id}
            badge={h.transport.kind}
            onEdit={() => onEditHost(h)}
            onRemove={() => onRemoveHost(h.id)}
          />
        ))}
      </Section>
      <Section title="Projects" onAdd={() => onAdd("project")}>
        {model.projects.map((p) => (
          <EntityRow
            key={p.id}
            label={p.name ? `${p.name} (${p.id})` : p.id}
            badge={p.hostId}
            onEdit={() => onEditProject(p)}
            onRemove={() => onRemoveProject(p.id)}
          />
        ))}
      </Section>
      <Section title="Agents" onAdd={() => onAdd("agent")}>
        {model.agents.map((a) => (
          <EntityRow
            key={a.id}
            label={a.id}
            badge={a.command[0] ?? ""}
            onEdit={() => onEditAgent(a)}
            onRemove={() => onRemoveAgent(a.id)}
          />
        ))}
      </Section>
      <div className="dialog-actions">
        <button type="button" onClick={onClose}>
          Close
        </button>
      </div>
    </div>
  );
}

/** Invalid-base recovery: show what's broken and let the user delete entries
 * until the file validates (ADR-0006). Add/edit stay disabled here. */
function DegradedRecovery({
  model,
  listError,
  onRemoveHost,
  onRemoveProject,
  onRemoveAgent,
  onClose,
}: Omit<
  SettingsListProps,
  "onAdd" | "onEditHost" | "onEditProject" | "onEditAgent"
>) {
  return (
    <div className="settings-body">
      <p className="dialog-error" role="alert">
        Your config file has problems and can't be edited normally. Delete the
        offending entries to recover:
      </p>
      <ul className="settings-issues">
        {model.issues.map((issue) => (
          <li key={issue}>{issue}</li>
        ))}
      </ul>
      {listError && (
        <p className="dialog-error" role="alert">
          {listError}
        </p>
      )}
      <DegradedSection
        title="Hosts"
        ids={model.present.hosts}
        onRemove={onRemoveHost}
      />
      <DegradedSection
        title="Projects"
        ids={model.present.projects}
        onRemove={onRemoveProject}
      />
      <DegradedSection
        title="Agents"
        ids={model.present.agents}
        onRemove={onRemoveAgent}
      />
      <div className="dialog-actions">
        <button type="button" onClick={onClose}>
          Close
        </button>
      </div>
    </div>
  );
}

function DegradedSection({
  title,
  ids,
  onRemove,
}: {
  title: string;
  ids: string[];
  onRemove: (id: string) => void;
}) {
  if (ids.length === 0) return null;
  return (
    <section className="settings-section">
      <h3>{title}</h3>
      <ul className="settings-list">
        {ids.map((id) => (
          <li key={id} className="settings-row">
            <span className="settings-row-label">{id}</span>
            <button type="button" onClick={() => onRemove(id)}>
              Remove
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

function Section({
  title,
  onAdd,
  children,
}: {
  title: string;
  onAdd: () => void;
  children: React.ReactNode;
}) {
  return (
    <section className="settings-section">
      <div className="settings-section-head">
        <h3>{title}</h3>
        <button type="button" onClick={onAdd}>
          + Add
        </button>
      </div>
      <ul className="settings-list">{children}</ul>
    </section>
  );
}

function EntityRow({
  label,
  badge,
  onEdit,
  onRemove,
}: {
  label: string;
  badge: string;
  onEdit: () => void;
  onRemove: () => void;
}) {
  return (
    <li className="settings-row">
      <span className="settings-row-label">{label}</span>
      {badge && <span className="settings-badge">{badge}</span>}
      <button type="button" onClick={onEdit}>
        Edit
      </button>
      <button type="button" onClick={onRemove}>
        Remove
      </button>
    </li>
  );
}
