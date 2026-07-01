import type { JSX } from "react";
import type { ActivityState } from "./activity-store";
import type { ProjectNode, SessionNode } from "./session-tree";
import { type IndicatorState, sessionIndicatorState } from "./status-state";
import {
  Activity,
  Command,
  Cpu,
  FileCode,
  GitBranch,
  type IconProps,
  Play,
  Server,
  Terminal,
} from "./ui/icons";

export type RailIcon = (props: IconProps) => JSX.Element;

/**
 * Curated per-project icon set. Order is a stability contract — reordering
 * changes which icon a project shows. Marine-only (icons inherit currentColor);
 * shape (not color) encodes project identity in the narrow rail (see #184).
 */
export const RAIL_ICONS: RailIcon[] = [
  Terminal,
  FileCode,
  GitBranch,
  Cpu,
  Command,
  Play,
  Server,
  Activity,
];

/** Icon for the project at `projectIndex` (its position among non-empty
 * projects, tree order). Index-based, collision-free for <=8 projects; wraps. */
export function projectIcon(projectIndex: number): RailIcon {
  return RAIL_ICONS[projectIndex % RAIL_ICONS.length];
}

/** Per-session distinguisher: first character (uppercased) of the branch, or
 * sessionId when there's no branch. Grapheme-safe (spreads to code points so a
 * surrogate pair is not split). Empty/whitespace -> a stable middot fallback. */
export function branchInitial(session: SessionNode): string {
  const label = (session.branch ?? session.sessionId).trim();
  const first = [...label][0];
  return first ? first.toUpperCase() : "·";
}

export interface RailEntry {
  key: string;
  session: SessionNode;
  Icon: RailIcon;
  initial: string;
  status: IndicatorState;
  active: boolean;
  connected: boolean;
  connecting: boolean;
  reconnecting: boolean;
  hostLabel: string;
  branchLabel: string;
  firstOfProject: boolean;
}

/**
 * Flatten the host-grouped tree into one decorated entry per session, in tree
 * order. Zero-session projects are skipped AND do not consume an icon index, so
 * two visible projects always get adjacent (distinct) icons. Pure — the whole
 * rail's logic lives here so it can be unit-tested without React.
 */
export function railEntries(
  tree: ProjectNode[],
  activeKey: string | null,
  openKeys: Set<string>,
  activity: ReadonlyMap<string, ActivityState>,
  connectingKeys: Set<string>,
): RailEntry[] {
  const entries: RailEntry[] = [];
  let projectIndex = 0;
  for (const project of tree) {
    if (project.sessions.length === 0) continue;
    const Icon = projectIcon(projectIndex);
    projectIndex++;
    let firstOfProject = true;
    for (const session of project.sessions) {
      entries.push({
        key: session.key,
        session,
        Icon,
        initial: branchInitial(session),
        status: sessionIndicatorState(session.state, activity.get(session.key)),
        active: session.key === activeKey,
        connected: openKeys.has(session.key),
        connecting: connectingKeys.has(session.key),
        reconnecting: session.reconnecting,
        hostLabel: project.hostLabel,
        branchLabel: session.branch ?? session.sessionId,
        firstOfProject,
      });
      firstOfProject = false;
    }
  }
  return entries;
}
