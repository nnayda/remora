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
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <h3>{mode === "create" ? "Add host" : `Edit host ${form.id}`}</h3>
      {mode === "create" && (
        <label>
          Id
          <input
            value={form.id}
            onChange={(e) => set("id", e.target.value)}
            placeholder="devbox"
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
        Transport
        <select
          value={form.kind}
          onChange={(e) => set("kind", e.target.value as typeof form.kind)}
        >
          <option value="ssh">ssh</option>
          <option value="kubectl">kubectl</option>
        </select>
      </label>
      {form.kind === "ssh" ? (
        <>
          <label>
            Host
            <input
              value={form.sshHost}
              onChange={(e) => set("sshHost", e.target.value)}
              placeholder="hostname or ssh alias"
            />
          </label>
          <label>
            User
            <input
              value={form.user}
              onChange={(e) => set("user", e.target.value)}
              placeholder="optional"
            />
          </label>
          <label>
            Port
            <input
              value={form.port}
              onChange={(e) => set("port", e.target.value)}
              placeholder="optional (default 22)"
            />
          </label>
        </>
      ) : (
        <>
          <label>
            Pod
            <input
              value={form.pod}
              onChange={(e) => set("pod", e.target.value)}
            />
          </label>
          <label>
            Namespace
            <input
              value={form.namespace}
              onChange={(e) => set("namespace", e.target.value)}
              placeholder="optional"
            />
          </label>
          <label>
            Context
            <input
              value={form.context}
              onChange={(e) => set("context", e.target.value)}
              placeholder="optional"
            />
          </label>
          <label>
            Container
            <input
              value={form.container}
              onChange={(e) => set("container", e.target.value)}
              placeholder="optional"
            />
          </label>
        </>
      )}
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
