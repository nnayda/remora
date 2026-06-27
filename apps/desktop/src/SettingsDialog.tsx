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
import "./SettingsDialog.css";
import { Button, Dialog, IconButton } from "./ui";
import { Cpu, Folder, Plus, Server, Settings, Trash } from "./ui/icons";

interface SettingsDialogProps {
  /** Re-read the sidebar's (redacted) config after a mutation. */
  onConfigChanged: () => void;
  onClose: () => void;
  /** Body to show on open. Defaults to the entity list; deep-link to a form
   * (e.g. the new-project "+" in the sidebar) by passing that view. */
  initialView?: View;
}

/** Which body the dialog shows: the entity list, or one open entity form. */
export type View =
  | { kind: "list" }
  | { kind: "host"; mode: "create" | "edit"; initial?: EditorHostDto }
  | { kind: "project"; mode: "create" | "edit"; initial?: EditorProjectDto }
  | { kind: "agent"; mode: "create" | "edit"; initial?: EditorAgentDto };

/**
 * Config management modal: list/add/edit/remove hosts, projects, and agents,
 * persisted through the editor bridge (ADR-0006). A thin shell over
 * `config-editor-model` and the entity forms. The design `<Dialog>` is
 * presentational, so the modal mechanics that mirror `NewSessionDialog` — focus
 * trap, Esc, restore-focus, inline errors — stay owned here. Component render is
 * covered by manual QA — vitest runs in node with no DOM; the model is unit-tested.
 */
export function SettingsDialog({
  onConfigChanged,
  onClose,
  initialView = { kind: "list" },
}: SettingsDialogProps) {
  const [model, setModel] = useState<SettingsModel | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [view, setView] = useState<View>(initialView);
  // Anchored inside the presentational Dialog body; the focus trap operates on
  // the enclosing `.rmra-dialog` element resolved via `dialogRoot()`.
  const anchorRef = useRef<HTMLDivElement>(null);
  const dialogRoot = () =>
    anchorRef.current?.closest<HTMLElement>(".rmra-dialog") ?? null;

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
  // biome-ignore lint/correctness/useExhaustiveDependencies: mount-only — set initial focus once and restore on unmount.
  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    dialogRoot()?.focus();
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
      dialogRoot()?.focus();
      return;
    }
    dialogRoot()
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
      dialogRoot()?.querySelectorAll<HTMLElement>(
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

  // Forms render their own actions; the list/error states get a Close footer.
  const footer =
    view.kind === "list" && model !== null ? (
      <Button variant="ghost" onClick={onClose}>
        Close
      </Button>
    ) : loadError ? (
      <Button variant="ghost" onClick={onClose}>
        Close
      </Button>
    ) : undefined;

  return (
    <Dialog
      className="settings-dialog"
      title="Settings"
      description="Manage hosts, projects, and agents."
      icon={<Settings size={18} />}
      onClose={onClose}
      footer={footer}
      // Presentational Dialog: own the trap + Esc + restore-focus here.
      onKeyDown={onKeyDown}
      tabIndex={-1}
      aria-label="Settings"
    >
      <div ref={anchorRef}>
        {loadError ? (
          <p className="settings-error" role="alert">
            Could not read config: {loadError}
          </p>
        ) : model === null ? (
          <p className="settings-loading">Loading…</p>
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
          />
        )}
      </div>
    </Dialog>
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
}: SettingsListProps) {
  if (model.degraded) {
    return (
      <DegradedRecovery
        model={model}
        listError={listError}
        onRemoveHost={onRemoveHost}
        onRemoveProject={onRemoveProject}
        onRemoveAgent={onRemoveAgent}
      />
    );
  }
  return (
    <div className="settings-body">
      {listError && (
        <p className="settings-error" role="alert">
          {listError}
        </p>
      )}
      <Section
        title="Hosts"
        icon={<Server size={14} />}
        onAdd={() => onAdd("host")}
      >
        {model.hosts.map((h) => (
          <EntityRow
            key={h.id}
            id={h.id}
            name={h.name ?? undefined}
            badge={h.transport.kind}
            editLabel={`Edit host ${h.id}`}
            removeLabel={`Remove host ${h.id}`}
            onEdit={() => onEditHost(h)}
            onRemove={() => onRemoveHost(h.id)}
          />
        ))}
      </Section>
      <Section
        title="Projects"
        icon={<Folder size={14} />}
        onAdd={() => onAdd("project")}
      >
        {model.projects.map((p) => (
          <EntityRow
            key={p.id}
            id={p.id}
            name={p.name ?? undefined}
            badge={p.hostId}
            editLabel={`Edit project ${p.id}`}
            removeLabel={`Remove project ${p.id}`}
            onEdit={() => onEditProject(p)}
            onRemove={() => onRemoveProject(p.id)}
          />
        ))}
      </Section>
      <Section
        title="Agents"
        icon={<Cpu size={14} />}
        onAdd={() => onAdd("agent")}
      >
        {model.agents.map((a) => (
          <EntityRow
            key={a.id}
            id={a.id}
            badge={a.command.length === 0 ? "(plain shell)" : a.command[0]}
            editLabel={`Edit agent ${a.id}`}
            removeLabel={`Remove agent ${a.id}`}
            onEdit={() => onEditAgent(a)}
            onRemove={() => onRemoveAgent(a.id)}
          />
        ))}
      </Section>
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
}: Omit<
  SettingsListProps,
  "onAdd" | "onEditHost" | "onEditProject" | "onEditAgent"
>) {
  return (
    <div className="settings-body">
      <p className="settings-error" role="alert">
        Your config file has problems and can't be edited normally. Delete the
        offending entries to recover:
      </p>
      <ul className="settings-issues">
        {model.issues.map((issue) => (
          <li key={issue}>{issue}</li>
        ))}
      </ul>
      {listError && (
        <p className="settings-error" role="alert">
          {listError}
        </p>
      )}
      <DegradedSection
        title="Hosts"
        icon={<Server size={14} />}
        ids={model.present.hosts}
        onRemove={onRemoveHost}
      />
      <DegradedSection
        title="Projects"
        icon={<Folder size={14} />}
        ids={model.present.projects}
        onRemove={onRemoveProject}
      />
      <DegradedSection
        title="Agents"
        icon={<Cpu size={14} />}
        ids={model.present.agents}
        onRemove={onRemoveAgent}
      />
    </div>
  );
}

function DegradedSection({
  title,
  icon,
  ids,
  onRemove,
}: {
  title: string;
  icon: React.ReactNode;
  ids: string[];
  onRemove: (id: string) => void;
}) {
  if (ids.length === 0) return null;
  return (
    <section className="settings-section">
      <div className="settings-section__head">
        <span className="settings-section__title">
          {icon}
          {title}
        </span>
      </div>
      <ul className="settings-list">
        {ids.map((id) => (
          <li key={id} className="settings-row">
            <div className="settings-row__main">
              <span className="settings-row__id">{id}</span>
            </div>
            <div className="settings-row__actions">
              <IconButton
                size="sm"
                label={`Remove ${id}`}
                onClick={() => onRemove(id)}
              >
                <Trash size={14} />
              </IconButton>
            </div>
          </li>
        ))}
      </ul>
    </section>
  );
}

function Section({
  title,
  icon,
  onAdd,
  children,
}: {
  title: string;
  icon: React.ReactNode;
  onAdd: () => void;
  children: React.ReactNode;
}) {
  const hasRows = Array.isArray(children)
    ? children.length > 0
    : Boolean(children);
  return (
    <section className="settings-section">
      <div className="settings-section__head">
        <span className="settings-section__title">
          {icon}
          {title}
        </span>
        <Button
          size="sm"
          variant="secondary"
          icon={<Plus size={14} />}
          onClick={onAdd}
        >
          Add
        </Button>
      </div>
      <ul className="settings-list">
        {hasRows ? children : <li className="settings-empty">None yet.</li>}
      </ul>
    </section>
  );
}

function EntityRow({
  id,
  name,
  badge,
  editLabel,
  removeLabel,
  onEdit,
  onRemove,
}: {
  id: string;
  name?: string;
  badge: string;
  editLabel: string;
  removeLabel: string;
  onEdit: () => void;
  onRemove: () => void;
}) {
  return (
    <li className="settings-row">
      <div className="settings-row__main">
        {name && <span className="settings-row__name">{name}</span>}
        <span className="settings-row__id">{id}</span>
      </div>
      {badge && <span className="settings-row__badge">{badge}</span>}
      <div className="settings-row__actions">
        <IconButton size="sm" label={editLabel} onClick={onEdit}>
          <Settings size={14} />
        </IconButton>
        <IconButton size="sm" label={removeLabel} onClick={onRemove}>
          <Trash size={14} />
        </IconButton>
      </div>
    </li>
  );
}
