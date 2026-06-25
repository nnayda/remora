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
import { normalizeSlugInput } from "./spawn-input";
import { Button, Input, Select } from "./ui";

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
      <div className="settings-form">
        <h3 className="settings-form__title">
          {mode === "create" ? "Add project" : `Edit project ${form.id}`}
        </h3>
        <p className="settings-error" role="alert">
          Add a host and an agent first — a project must reference both.
        </p>
        <div className="settings-form__actions">
          <Button type="button" variant="ghost" onClick={onCancel}>
            Back
          </Button>
        </div>
      </div>
    );
  }

  return (
    <form
      className="settings-form"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <h3 className="settings-form__title">
        {mode === "create" ? "Add project" : `Edit project ${form.id}`}
      </h3>
      {mode === "create" && (
        <Input
          label="Id"
          mono
          value={form.id}
          onChange={(e) => set("id", normalizeSlugInput(e.target.value))}
          placeholder="api"
          autoFocus
        />
      )}
      <Input
        label="Name"
        value={form.name}
        onChange={(e) => set("name", e.target.value)}
        placeholder="optional display name"
      />
      <Select
        label="Host"
        mono
        value={form.hostId}
        onChange={(value) => set("hostId", value)}
        options={hostIds}
      />
      <Input
        label="Path"
        mono
        value={form.path}
        onChange={(e) => set("path", e.target.value)}
        placeholder="/srv/api or ~/code/api"
      />
      <Select
        label="Workspace"
        mono
        value={form.workspace}
        onChange={(value) => set("workspace", value as typeof form.workspace)}
        options={[
          { value: "worktree", label: "worktree" },
          { value: "shared", label: "shared" },
        ]}
      />
      <Select
        label="Agent"
        mono
        value={form.agent}
        onChange={(value) => set("agent", value)}
        options={agentIds}
      />
      <Input
        label="Base (optional)"
        mono
        value={form.base}
        placeholder="origin/main (auto-detected if empty)"
        onChange={(e) => set("base", e.target.value)}
      />
      {error && (
        <p className="settings-error" role="alert">
          {error}
        </p>
      )}
      <div className="settings-form__actions">
        <Button type="button" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        <Button type="submit" variant="primary" loading={submitting}>
          {submitting ? "Saving…" : "Save"}
        </Button>
      </div>
    </form>
  );
}
