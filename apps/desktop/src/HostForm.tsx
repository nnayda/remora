import { useState } from "react";
import type { EditorHostDto, HostInputDto } from "./bindings";
import {
  emptyHostForm,
  type FormMode,
  hostFormFromDto,
  toHostInput,
  validateHostForm,
} from "./config-editor-model";
import { formErrorMessage } from "./form-error";
import { normalizeSlugInput } from "./spawn-input";
import { Button, Input, Select, Switch } from "./ui";

interface HostFormProps {
  mode: FormMode;
  /** Prefill for edit; omitted (create) starts blank. */
  initial?: EditorHostDto;
  /** Persist; rejects with the typed `BridgeError` (shown inline). */
  onSubmit: (id: string, input: HostInputDto) => Promise<void>;
  onCancel: () => void;
}

/** Create/edit a host. State + validation live in `config-editor-model`; this is
 * the thin form shell (covered by manual QA — vitest runs in node, no DOM). */
export function HostForm({ mode, initial, onSubmit, onCancel }: HostFormProps) {
  const [form, setForm] = useState(() =>
    initial ? hostFormFromDto(initial) : emptyHostForm(),
  );
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  function set<K extends keyof typeof form>(key: K, value: (typeof form)[K]) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  async function submit() {
    const invalid = validateHostForm(form, mode);
    if (invalid) {
      setError(invalid);
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await onSubmit(form.id, toHostInput(form));
    } catch (err) {
      setError(formErrorMessage(err));
    } finally {
      setSubmitting(false);
    }
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
        {mode === "create" ? "Add host" : `Edit host ${form.id}`}
      </h3>
      {mode === "create" && (
        <Input
          label="Id"
          mono
          value={form.id}
          onChange={(e) => set("id", normalizeSlugInput(e.target.value))}
          placeholder="devbox"
          // biome-ignore lint/a11y/noAutofocus: first field of an opened form
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
        label="Transport"
        mono
        value={form.kind}
        onChange={(value) => set("kind", value as typeof form.kind)}
        options={[
          { value: "ssh", label: "ssh" },
          { value: "kubectl", label: "kubectl" },
        ]}
      />
      {form.kind === "ssh" ? (
        <>
          <Input
            label="Host"
            mono
            value={form.sshHost}
            onChange={(e) => set("sshHost", e.target.value)}
            placeholder="hostname or ssh alias"
          />
          <Input
            label="User"
            mono
            value={form.user}
            onChange={(e) => set("user", e.target.value)}
            placeholder="optional"
          />
          <Input
            label="Port"
            mono
            value={form.port}
            onChange={(e) => set("port", e.target.value)}
            placeholder="optional (default 22)"
          />
        </>
      ) : (
        <>
          <Input
            label="Pod"
            mono
            value={form.pod}
            onChange={(e) => set("pod", e.target.value)}
            placeholder={
              form.podIsCommand ? "kubectl … -o name | head -n1" : "pod name"
            }
            hint={
              form.podIsCommand
                ? "The command itself (a bare pipeline) — don't wrap it in $(…) or backticks."
                : undefined
            }
          />
          <Switch
            className="settings-form__toggle"
            label="Resolve via shell command"
            checked={form.podIsCommand}
            onChange={(checked) => set("podIsCommand", checked)}
          />
          <Input
            label="Namespace"
            mono
            value={form.namespace}
            onChange={(e) => set("namespace", e.target.value)}
            placeholder={
              form.namespaceIsCommand ? "shell command…" : "optional"
            }
          />
          <Switch
            className="settings-form__toggle"
            label="Resolve via shell command"
            checked={form.namespaceIsCommand}
            onChange={(checked) => set("namespaceIsCommand", checked)}
          />
          <Input
            label="Context"
            mono
            value={form.context}
            onChange={(e) => set("context", e.target.value)}
            placeholder={form.contextIsCommand ? "shell command…" : "optional"}
          />
          <Switch
            className="settings-form__toggle"
            label="Resolve via shell command"
            checked={form.contextIsCommand}
            onChange={(checked) => set("contextIsCommand", checked)}
          />
          <Input
            label="Container"
            mono
            value={form.container}
            onChange={(e) => set("container", e.target.value)}
            placeholder={
              form.containerIsCommand ? "shell command…" : "optional"
            }
          />
          <Switch
            className="settings-form__toggle"
            label="Resolve via shell command"
            checked={form.containerIsCommand}
            onChange={(checked) => set("containerIsCommand", checked)}
          />
        </>
      )}
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
