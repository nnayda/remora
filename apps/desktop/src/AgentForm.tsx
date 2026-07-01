import { useState } from "react";
import type { AgentInputDto, EditorAgentDto } from "./bindings";
import {
  addArg,
  agentFormFromDto,
  applyClaudeTemplate,
  emptyAgentForm,
  type FormMode,
  moveArg,
  removeArg,
  setArg,
  toAgentInput,
  validateAgentForm,
} from "./config-editor-model";
import { formErrorMessage } from "./form-error";
import { normalizeSlugInput } from "./spawn-input";
import { Button, IconButton, Input, Switch } from "./ui";
import { ArrowUp, Plus, Trash } from "./ui/icons";

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
      className="settings-form"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <h3 className="settings-form__title">
        {mode === "create" ? "Add agent" : `Edit agent ${form.id}`}
      </h3>
      {mode === "create" && (
        <Input
          label="Id"
          mono
          value={form.id}
          onChange={(e) =>
            setForm((f) => ({ ...f, id: normalizeSlugInput(e.target.value) }))
          }
          placeholder="claude"
          // biome-ignore lint/a11y/noAutofocus: first field of an opened form
          autoFocus
        />
      )}
      <Switch
        className="settings-form__toggle"
        label="No command (plain shell)"
        checked={form.plainShell}
        onChange={(checked) => setForm((f) => ({ ...f, plainShell: checked }))}
      />
      {mode === "create" && (
        <div>
          <Button
            type="button"
            variant="secondary"
            size="sm"
            onClick={() => setForm(applyClaudeTemplate)}
          >
            Claude Code (activity markers)
          </Button>
        </div>
      )}
      <span className="settings-form__fieldlabel">Command</span>
      <ul className="settings-argv" aria-disabled={form.plainShell}>
        {form.command.map((arg, i) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: argv rows are positional
          <li key={i} className="settings-argv__row">
            <Input
              aria-label={`argument ${i + 1}`}
              mono
              value={arg}
              disabled={form.plainShell}
              onChange={(e) =>
                setCommand(setArg(form.command, i, e.target.value))
              }
              placeholder={i === 0 ? "claude" : "--flag"}
            />
            <IconButton
              size="sm"
              label={`move argument ${i + 1} up`}
              disabled={i === 0 || form.plainShell}
              onClick={() => setCommand(moveArg(form.command, i, -1))}
            >
              <ArrowUp size={14} />
            </IconButton>
            <IconButton
              size="sm"
              label={`move argument ${i + 1} down`}
              disabled={i === form.command.length - 1 || form.plainShell}
              onClick={() => setCommand(moveArg(form.command, i, 1))}
            >
              <ArrowUp size={14} style={{ transform: "rotate(180deg)" }} />
            </IconButton>
            <IconButton
              size="sm"
              label={`remove argument ${i + 1}`}
              disabled={form.command.length === 1 || form.plainShell}
              onClick={() => setCommand(removeArg(form.command, i))}
            >
              <Trash size={14} />
            </IconButton>
          </li>
        ))}
      </ul>
      <div>
        <Button
          type="button"
          variant="secondary"
          size="sm"
          icon={<Plus size={14} />}
          disabled={form.plainShell}
          onClick={() => setCommand(addArg(form.command))}
        >
          Add argument
        </Button>
      </div>
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
