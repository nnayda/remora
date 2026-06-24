import { useState } from "react";
import type { EditorProjectDto, ProjectInputDto } from "./bindings";
import {
  emptyProjectForm,
  type FormMode,
  projectFormFromDto,
  toProjectInput,
  validateProjectForm,
} from "./config-editor-model";
import { formErrorMessage } from "./form-error";

interface ProjectFormProps {
  mode: FormMode;
  initial?: EditorProjectDto;
  /** Existing host/agent ids — the dropdown options and membership guard, so a
   * project can never be created against a dangling reference. */
  hostIds: string[];
  agentIds: string[];
  onSubmit: (id: string, input: ProjectInputDto) => Promise<void>;
  onCancel: () => void;
}

/** Create/edit a project. Thin shell over `config-editor-model` (manual-QA). */
export function ProjectForm({
  mode,
  initial,
  hostIds,
  agentIds,
  onSubmit,
  onCancel,
}: ProjectFormProps) {
  const [form, setForm] = useState(() =>
    initial ? projectFormFromDto(initial) : emptyProjectForm(hostIds, agentIds),
  );
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  function set<K extends keyof typeof form>(key: K, value: (typeof form)[K]) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  async function submit() {
    const invalid = validateProjectForm(form, mode, hostIds, agentIds);
    if (invalid) {
      setError(invalid);
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await onSubmit(form.id, toProjectInput(form));
    } catch (err) {
      setError(formErrorMessage(err));
    } finally {
      setSubmitting(false);
    }
  }

  // A project references a host and an agent; without either it can't be made.
  if (hostIds.length === 0 || agentIds.length === 0) {
    return (
      <div>
        <h3>{mode === "create" ? "Add project" : `Edit project ${form.id}`}</h3>
        <p className="dialog-error" role="alert">
          Add a host and an agent first — a project must reference both.
        </p>
        <div className="dialog-actions">
          <button type="button" onClick={onCancel}>
            Back
          </button>
        </div>
      </div>
    );
  }

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <h3>{mode === "create" ? "Add project" : `Edit project ${form.id}`}</h3>
      {mode === "create" && (
        <label>
          Id
          <input
            value={form.id}
            onChange={(e) => set("id", e.target.value)}
            placeholder="api"
            // biome-ignore lint/a11y/noAutofocus: first field of an opened form
            autoFocus
          />
        </label>
      )}
      <label>
        Name
        <input
          value={form.name}
          onChange={(e) => set("name", e.target.value)}
          placeholder="optional display name"
        />
      </label>
      <label>
        Host
        <select
          value={form.hostId}
          onChange={(e) => set("hostId", e.target.value)}
        >
          {hostIds.map((id) => (
            <option key={id} value={id}>
              {id}
            </option>
          ))}
        </select>
      </label>
      <label>
        Path
        <input
          value={form.path}
          onChange={(e) => set("path", e.target.value)}
          placeholder="/srv/api or ~/code/api"
        />
      </label>
      <label>
        Workspace
        <select
          value={form.workspace}
          onChange={(e) =>
            set("workspace", e.target.value as typeof form.workspace)
          }
        >
          <option value="worktree">worktree</option>
          <option value="shared">shared</option>
        </select>
      </label>
      <label>
        Agent
        <select
          value={form.agent}
          onChange={(e) => set("agent", e.target.value)}
        >
          {agentIds.map((id) => (
            <option key={id} value={id}>
              {id}
            </option>
          ))}
        </select>
      </label>
      <label>
        Base (optional)
        <input
          value={form.base}
          placeholder="origin/main (auto-detected if empty)"
          onChange={(e) => set("base", e.target.value)}
        />
      </label>
      {error && (
        <p className="dialog-error" role="alert">
          {error}
        </p>
      )}
      <div className="dialog-actions">
        <button type="button" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" disabled={submitting}>
          {submitting ? "Saving…" : "Save"}
        </button>
      </div>
    </form>
  );
}
