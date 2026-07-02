import type {
  AgentInputDto,
  EditableConfigDto,
  EditorAgentDto,
  EditorHostDto,
  EditorProjectDto,
  HostInputDto,
  PresentEntitiesDto,
  ProjectInputDto,
  ProvisionFileDto,
  WorkspaceModeDto,
} from "./bindings";
import { isValidSlug } from "./spawn-input";

/** Whether a form is creating a new entry (id editable, slug-checked) or
 * editing an existing one (id locked — it is an immutable join key, ADR-0004). */
export type FormMode = "create" | "edit";

/**
 * Everything the Settings modal needs, normalized from the bridge's
 * `EditableConfigDto`. Pure (no React, no I/O) so it is node-testable.
 *
 * When the base file is valid, `degraded` is false and `hosts`/`projects`/
 * `agents` carry the editable entities. When it is semantically invalid,
 * `degraded` is true, those lists are empty, `issues` says what's broken, and
 * `present` lists the ids the user can delete to recover (ADR-0006).
 */
export interface SettingsModel {
  degraded: boolean;
  issues: string[];
  hosts: EditorHostDto[];
  projects: EditorProjectDto[];
  agents: EditorAgentDto[];
  present: PresentEntitiesDto;
}

export function buildSettingsModel(dto: EditableConfigDto): SettingsModel {
  return {
    degraded: dto.config === null,
    issues: dto.issues,
    hosts: dto.config?.hosts ?? [],
    projects: dto.config?.projects ?? [],
    agents: dto.config?.agents ?? [],
    present: dto.present,
  };
}

// ---- Host form ----

export type TransportKind = "ssh" | "kubectl";

/** Flat, all-string host form state (ssh + kubectl fields coexist; `kind`
 * selects which the form shows and which `toHostInput` reads). */
export interface HostFormState {
  id: string;
  name: string;
  kind: TransportKind;
  sshHost: string;
  user: string;
  port: string;
  pod: string;
  namespace: string;
  context: string;
  container: string;
  podIsCommand: boolean;
  namespaceIsCommand: boolean;
  contextIsCommand: boolean;
  containerIsCommand: boolean;
  worktreeRoot: string;
}

export function emptyHostForm(): HostFormState {
  return {
    id: "",
    name: "",
    kind: "ssh",
    sshHost: "",
    user: "",
    port: "",
    pod: "",
    namespace: "",
    context: "",
    container: "",
    podIsCommand: false,
    namespaceIsCommand: false,
    contextIsCommand: false,
    containerIsCommand: false,
    worktreeRoot: "",
  };
}

export function hostFormFromDto(dto: EditorHostDto): HostFormState {
  const form = {
    ...emptyHostForm(),
    id: dto.id,
    name: dto.name ?? "",
    worktreeRoot: dto.worktreeRoot ?? "",
  };
  const t = dto.transport;
  if (t.kind === "ssh") {
    form.kind = "ssh";
    form.sshHost = t.host;
    form.user = t.user ?? "";
    form.port = t.port === null ? "" : String(t.port);
  } else {
    form.kind = "kubectl";
    form.pod = t.pod.value;
    form.podIsCommand = t.pod.command;
    if (t.namespace) {
      form.namespace = t.namespace.value;
      form.namespaceIsCommand = t.namespace.command;
    }
    if (t.context) {
      form.context = t.context.value;
      form.contextIsCommand = t.context.command;
    }
    if (t.container) {
      form.container = t.container.value;
      form.containerIsCommand = t.container.command;
    }
  }
  return form;
}

/** First-line validation (non-empty, slug shape, sane port); core re-validation
 * is the source of truth. Returns an error message, or null when ok. */
export function validateHostForm(
  form: HostFormState,
  mode: FormMode,
): string | null {
  const idError = validateId(form.id, mode);
  if (idError) return idError;
  if (form.kind === "ssh") {
    if (form.sshHost.trim() === "") return "Host is required.";
    const port = form.port.trim();
    if (port !== "" && !isValidPort(port)) {
      return "Port must be a number between 1 and 65535.";
    }
  } else if (form.pod.trim() === "") {
    return "Pod is required.";
  }
  return null;
}

function fieldDto(value: string, isCommand: boolean) {
  return { command: isCommand, value: value.trim() };
}

function optionalFieldDto(value: string, isCommand: boolean) {
  return value.trim() === "" ? null : fieldDto(value, isCommand);
}

export function toHostInput(form: HostFormState): HostInputDto {
  const transport =
    form.kind === "ssh"
      ? {
          kind: "ssh" as const,
          host: form.sshHost.trim(),
          user: blankToNull(form.user),
          port: form.port.trim() === "" ? null : Number(form.port.trim()),
        }
      : {
          kind: "kubectl" as const,
          pod: fieldDto(form.pod, form.podIsCommand),
          namespace: optionalFieldDto(form.namespace, form.namespaceIsCommand),
          context: optionalFieldDto(form.context, form.contextIsCommand),
          container: optionalFieldDto(form.container, form.containerIsCommand),
        };
  return {
    name: blankToNull(form.name),
    transport,
    worktreeRoot: blankToNull(form.worktreeRoot),
  };
}

// ---- Project form ----

export interface ProjectFormState {
  id: string;
  name: string;
  hostId: string;
  path: string;
  workspace: WorkspaceModeDto;
  agent: string;
  base: string;
  worktreeRoot: string;
}

/** A blank project form, preselecting the first existing host and agent so the
 * dropdowns never start on a dangling reference. */
export function emptyProjectForm(
  hostIds: string[],
  agentIds: string[],
): ProjectFormState {
  return {
    id: "",
    name: "",
    hostId: hostIds[0] ?? "",
    path: "",
    workspace: "worktree",
    agent: agentIds[0] ?? "",
    base: "",
    worktreeRoot: "",
  };
}

export function projectFormFromDto(dto: EditorProjectDto): ProjectFormState {
  return {
    id: dto.id,
    name: dto.name ?? "",
    hostId: dto.hostId,
    path: dto.path,
    workspace: dto.workspace,
    agent: dto.agent,
    base: dto.base ?? "",
    worktreeRoot: dto.worktreeRoot ?? "",
  };
}

/** Validates against the *existing* host/agent ids so a stale selection can't
 * submit a dangling reference (the dropdowns are membership-guarded too). */
export function validateProjectForm(
  form: ProjectFormState,
  mode: FormMode,
  hostIds: string[],
  agentIds: string[],
): string | null {
  const idError = validateId(form.id, mode);
  if (idError) return idError;
  if (!hostIds.includes(form.hostId)) return "Select a host.";
  if (!agentIds.includes(form.agent)) return "Select an agent.";
  if (form.path.trim() === "") return "Path is required.";
  return null;
}

export function toProjectInput(form: ProjectFormState): ProjectInputDto {
  return {
    name: blankToNull(form.name),
    hostId: form.hostId,
    path: form.path.trim(),
    workspace: form.workspace,
    agent: form.agent,
    base: blankToNull(form.base),
    worktreeRoot: blankToNull(form.worktreeRoot),
  };
}

// ---- Agent form ----

export interface AgentFormState {
  id: string;
  command: string[];
  /** When true, this agent has no launch command — a plain shell (#35). The
   * argv editor is disabled but its rows are preserved so unchecking restores
   * them; only `[]` is sent on save. */
  plainShell: boolean;
  /** Optional file to write and chmod before the agent's first launch (#196),
   * e.g. the Claude notification-hook script. Round-tripped untouched through
   * edit — the form has no UI for it yet. */
  provision: ProvisionFileDto | null;
}

export function emptyAgentForm(): AgentFormState {
  return { id: "", command: [""], plainShell: false, provision: null };
}

export function agentFormFromDto(dto: EditorAgentDto): AgentFormState {
  return {
    id: dto.id,
    command: dto.command.length > 0 ? [...dto.command] : [""],
    plainShell: dto.command.length === 0,
    provision: dto.provision ?? null, // round-trip: preserve on edit (D5)
  };
}

/** Append a new blank argv row. */
export function addArg(command: string[]): string[] {
  return [...command, ""];
}

/** Set the argv row at `index`. */
export function setArg(
  command: string[],
  index: number,
  value: string,
): string[] {
  return command.map((arg, i) => (i === index ? value : arg));
}

/** Remove the argv row at `index`. */
export function removeArg(command: string[], index: number): string[] {
  return command.filter((_, i) => i !== index);
}

/** Move the argv row at `index` by `dir` (-1 up, +1 down). Out-of-range is a
 * no-op so the reorder buttons never throw at the ends. */
export function moveArg(
  command: string[],
  index: number,
  dir: -1 | 1,
): string[] {
  const target = index + dir;
  if (target < 0 || target >= command.length) return command;
  const next = [...command];
  [next[index], next[target]] = [next[target], next[index]];
  return next;
}

/** Unicode `Dash_Punctuation` (Pd) code points minus ASCII hyphen-minus — the
 * dashes autocorrect/paste plausibly substitutes for `-`. Mirrors the Rust
 * `starts_with_unicode_dash` guard in remora-core's config validation. */
const UNICODE_DASH_PREFIX =
  /^[\u058A\u05BE\u1400\u1806\u2010-\u2015\u2E17\u2E1A\u2E3A\u2E3B\u2E40\u301C\u3030\u30A0\uFE31\uFE32\uFE58\uFE63\uFF0D]/u;

export function validateAgentForm(
  form: AgentFormState,
  mode: FormMode,
): string | null {
  const idError = validateId(form.id, mode);
  if (idError) return idError;
  if (!form.plainShell && form.command.every((arg) => arg.trim() === "")) {
    return "Command cannot be empty.";
  }
  // A leading Unicode dash (e.g. `—flag` from autocorrect) is read as a prompt,
  // not a flag, by the agent CLI — reject it instead of letting it confuse the
  // launch silently. Skipped in plain-shell mode, where the command rows are
  // ignored (saved as `[]`) so a leftover dash row must not block the save.
  if (
    !form.plainShell &&
    form.command.some((arg) => UNICODE_DASH_PREFIX.test(arg.trim()))
  ) {
    return "An argument uses a Unicode dash (e.g. — or –). Use ASCII `-`/`--` for flags.";
  }
  return null;
}

export function toAgentInput(form: AgentFormState): AgentInputDto {
  const provision = form.provision ?? undefined;
  if (form.plainShell) {
    return { command: [], provision };
  }
  return {
    command: form.command.map((arg) => arg.trim()).filter((arg) => arg !== ""),
    provision,
  };
}

// ---- Claude notification-hook template ----

const CLAUDE_NOTIFY_PATH = "~/.remora/hooks/claude-notify.sh";

// Byte-identical to contrib/agent-hooks/claude-code/remora-notify.sh — a
// drift-guard test (claude-template.drift.test.ts) enforces this, so this
// string must never be hand-edited independently of that file.
const CLAUDE_NOTIFY_SCRIPT = `#!/usr/bin/env bash
# Remora activity marker (OSC-7366, ADR-0010). Claude Code "Notification" hook:
# emit awaiting_input + the agent's message as a preview so Remora can show what
# the agent is waiting for.
#
# Two non-obvious, load-bearing requirements:
#   1. The marker MUST be tmux-passthrough WRAPPED. Remora runs the agent in its
#      own tmux session with allow-passthrough; tmux silently consumes a bare OSC.
#   2. It MUST go to the terminal, not stdout. Claude Code captures a hook's
#      stdout (shown only in Ctrl-R), so stdout never reaches the PTY byte stream
#      Remora reads. Default to /dev/tty; REMORA_MARKER_OUT overrides (tests).
#
# The payload is UNTRUSTED by design; Remora core sanitizes + length-caps it.
# This printf MUST stay byte-for-byte in sync with the wire contract asserted by
# remora_notify_recipe_round_trip in crates/remora-core/src/activity/marker.rs.
set -euo pipefail

out="\${REMORA_MARKER_OUT:-/dev/tty}"
msg="$(jq -r '.message // empty' 2>/dev/null || true)"
[ -n "$msg" ] || exit 0

enc="$(printf '%s' "$msg" | base64 | tr -d '\\n')"
state="YXdhaXRpbmdfaW5wdXQ="   # base64("awaiting_input")

# on-wire (tmux passthrough envelope, inner ESC doubled):
#   ESC P tmux ; ESC ESC ] 7366 ; remora ; 1 ; state ; <state> ; <msg> BEL ESC \\
printf '\\033Ptmux;\\033\\033]7366;remora;1;state;%s;%s\\007\\033\\\\' "$state" "$enc" > "$out" 2>/dev/null || exit 0
`;

// Inline settings: a Notification hook running the script via $HOME (claude
// runs hook commands through sh -c, which expands $HOME — tilde is not
// guaranteed to expand there).
const CLAUDE_SETTINGS_JSON = JSON.stringify({
  hooks: {
    Notification: [
      {
        hooks: [
          {
            type: "command",
            command: "$HOME/.remora/hooks/claude-notify.sh",
          },
        ],
      },
    ],
  },
});

/** The stock "wire Claude's Notification hook to Remora's activity marker"
 * template (#196): a `--settings` flag carrying the hook config, plus the
 * provision file the hook script needs on disk before first launch. */
export function claudeMarkerTemplate(): {
  command: string[];
  provision: ProvisionFileDto;
} {
  return {
    command: ["claude", "--settings", CLAUDE_SETTINGS_JSON],
    provision: {
      path: CLAUDE_NOTIFY_PATH,
      content: CLAUDE_NOTIFY_SCRIPT,
      mode: 0o755,
    },
  };
}

/** Apply the Claude activity-markers template (#196) to a form: set the
 * provision file and ensure the launch command carries `--settings`, WITHOUT
 * clobbering the user's existing flags (e.g. `--continue`). A blank command
 * uses the template command outright. Shaped as `(prev) => next` so callers
 * can pass it straight to `setForm`. */
export function applyClaudeTemplate(form: AgentFormState): AgentFormState {
  const t = claudeMarkerTemplate();
  const base = form.command.filter((arg) => arg.trim() !== "");
  const settingsJson = t.command[t.command.indexOf("--settings") + 1];
  const command =
    base.length === 0
      ? t.command
      : base.includes("--settings")
        ? base
        : [...base, "--settings", settingsJson];
  return { ...form, command, provision: t.provision, plainShell: false };
}

// ---- shared helpers ----

/** Create checks the slug shape; edit locks the id (an immutable join key). */
function validateId(id: string, mode: FormMode): string | null {
  if (mode === "edit") return null;
  if (!isValidSlug(id)) {
    return "Id must be a lowercase slug (a–z, 0–9, hyphen), 1–64 characters.";
  }
  return null;
}

function blankToNull(value: string): string | null {
  const trimmed = value.trim();
  return trimmed === "" ? null : trimmed;
}

function isValidPort(value: string): boolean {
  if (!/^[0-9]+$/.test(value)) return false;
  const n = Number(value);
  return n >= 1 && n <= 65535;
}
