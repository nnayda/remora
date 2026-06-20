import type {
  AgentInputDto,
  EditableConfigDto,
  EditorAgentDto,
  EditorHostDto,
  EditorProjectDto,
  HostInputDto,
  PresentEntitiesDto,
  ProjectInputDto,
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
  };
}

export function hostFormFromDto(dto: EditorHostDto): HostFormState {
  const form = { ...emptyHostForm(), id: dto.id, name: dto.name ?? "" };
  const t = dto.transport;
  if (t.kind === "ssh") {
    form.kind = "ssh";
    form.sshHost = t.host;
    form.user = t.user ?? "";
    form.port = t.port === null ? "" : String(t.port);
  } else {
    form.kind = "kubectl";
    form.pod = t.pod;
    form.namespace = t.namespace ?? "";
    form.context = t.context ?? "";
    form.container = t.container ?? "";
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
          pod: form.pod.trim(),
          namespace: blankToNull(form.namespace),
          context: blankToNull(form.context),
          container: blankToNull(form.container),
        };
  return { name: blankToNull(form.name), transport };
}

// ---- Project form ----

export interface ProjectFormState {
  id: string;
  name: string;
  hostId: string;
  path: string;
  workspace: WorkspaceModeDto;
  agent: string;
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
  };
}

// ---- Agent form ----

export interface AgentFormState {
  id: string;
  command: string[];
}

export function emptyAgentForm(): AgentFormState {
  return { id: "", command: [""] };
}

export function agentFormFromDto(dto: EditorAgentDto): AgentFormState {
  return { id: dto.id, command: [...dto.command] };
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

export function validateAgentForm(
  form: AgentFormState,
  mode: FormMode,
): string | null {
  const idError = validateId(form.id, mode);
  if (idError) return idError;
  if (form.command.every((arg) => arg.trim() === "")) {
    return "Command cannot be empty.";
  }
  return null;
}

export function toAgentInput(form: AgentFormState): AgentInputDto {
  return {
    command: form.command.map((arg) => arg.trim()).filter((arg) => arg !== ""),
  };
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
