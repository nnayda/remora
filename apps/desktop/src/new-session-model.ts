import type { ConfigDto, WorkspaceModeDto } from "./bindings";

/** One selectable project in the new-session dialog, with its host and default
 * agent resolved from config so the dialog never has to re-join. */
export interface ProjectOption {
  id: string;
  /** Display label: the project's name, or its id when unnamed. */
  label: string;
  /** Host label to show for the project: host name, host id, or — if the host
   * is missing from config — the raw host id. */
  hostLabel: string;
  /** The project's default agent id; preselected in the agent picker. */
  defaultAgent: string;
  /** The project's workspace mode; determines whether sessions can be respawned. */
  defaultWorkspace: WorkspaceModeDto;
}

/** Everything the new-session dialog needs to render its pickers, derived from
 * the per-device config. Pure (no React, no I/O) so it is node-testable. */
export interface NewSessionModel {
  projects: ProjectOption[];
  /** Configured agent ids, in config order, for the agent picker. */
  agents: string[];
}

/** Derive the dialog's pickers from config: join each project to its host
 * label and default agent, and list the configured agents in config order. */
export function buildNewSessionModel(config: ConfigDto): NewSessionModel {
  const hostLabels = new Map(config.hosts.map((h) => [h.id, h.name ?? h.id]));
  return {
    projects: config.projects.map((p) => ({
      id: p.id,
      label: p.name ?? p.id,
      hostLabel: hostLabels.get(p.hostId) ?? p.hostId,
      defaultAgent: p.agent,
      defaultWorkspace: p.workspace,
    })),
    agents: config.agents.map((a) => a.id),
  };
}

/** A resolved, internally-consistent dialog selection. */
export interface Selection {
  projectId: string;
  agent: string;
  workspace: WorkspaceModeDto;
}

/**
 * Clamp a (possibly stale) `projectId` to a project that actually exists in the
 * model, returning that project's default agent. Falls back to the first
 * project — or empty strings when there are none. Keeps the dialog's selection
 * consistent with config even if config changes while the dialog is open (a
 * manual refresh, or the first config load landing after the dialog opened).
 */
export function resolveSelection(
  model: NewSessionModel,
  projectId: string,
): Selection {
  const project =
    model.projects.find((p) => p.id === projectId) ?? model.projects[0] ?? null;
  return {
    projectId: project?.id ?? "",
    agent: project?.defaultAgent ?? "",
    workspace: project?.defaultWorkspace ?? "worktree",
  };
}
