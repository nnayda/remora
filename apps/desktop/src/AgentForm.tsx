import { useState } from "react";
import type { AgentInputDto, EditorAgentDto } from "./bindings";
import {
  addArg,
  agentFormFromDto,
  emptyAgentForm,
  type FormMode,
  moveArg,
  removeArg,
  setArg,
  toAgentInput,
  validateAgentForm,
} from "./config-editor-model";
import { formErrorMessage } from "./form-error";

interface AgentFormProps {
  mode: FormMode;
  initial?: EditorAgentDto;
  onSubmit: (id: string, input: AgentInputDto) => Promise<void>;
  onCancel: () => void;
}

/** Create/edit an agent, including the argv editor. Row edits go through the
 * pure `config-editor-model` helpers; thin shell (manual-QA). */
export function AgentForm({
  mode,
  initial,
  onSubmit,
  onCancel,
}: AgentFormProps) {
  const [form, setForm] = useState(() =>
    initial ? agentFormFromDto(initial) : emptyAgentForm(),
  );
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const setCommand = (command: string[]) => setForm((f) => ({ ...f, command }));

  async function submit() {
    const invalid = validateAgentForm(form, mode);
    if (invalid) {
      setError(invalid);
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await onSubmit(form.id, toAgentInput(form));
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
      <h3>{mode === "create" ? "Add agent" : `Edit agent ${form.id}`}</h3>
      {mode === "create" && (
        <label>
          Id
          <input
            value={form.id}
            onChange={(e) => setForm((f) => ({ ...f, id: e.target.value }))}
            placeholder="claude"
            // biome-ignore lint/a11y/noAutofocus: first field of an opened form
            autoFocus
          />
        </label>
      )}
      <span className="dialog-label">Command</span>
      <ul className="argv-editor">
        {form.command.map((arg, i) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: argv rows are positional
          <li key={i} className="argv-row">
            <input
              aria-label={`argument ${i + 1}`}
              value={arg}
              onChange={(e) =>
                setCommand(setArg(form.command, i, e.target.value))
              }
              placeholder={i === 0 ? "claude" : "--flag"}
            />
            <button
              type="button"
              aria-label={`move argument ${i + 1} up`}
              disabled={i === 0}
              onClick={() => setCommand(moveArg(form.command, i, -1))}
            >
              ↑
            </button>
            <button
              type="button"
              aria-label={`move argument ${i + 1} down`}
              disabled={i === form.command.length - 1}
              onClick={() => setCommand(moveArg(form.command, i, 1))}
            >
              ↓
            </button>
            <button
              type="button"
              aria-label={`remove argument ${i + 1}`}
              disabled={form.command.length === 1}
              onClick={() => setCommand(removeArg(form.command, i))}
            >
              ✕
            </button>
          </li>
        ))}
      </ul>
      <button
        type="button"
        className="argv-add"
        onClick={() => setCommand(addArg(form.command))}
      >
        + Add argument
      </button>
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
