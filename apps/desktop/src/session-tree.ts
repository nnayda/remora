import type { ConfigDto, SessionMetaDto, SessionStateDto } from "./bindings";
import { tabKey } from "./session-store";

/**
 * Pure config-and-discovery join (stage 10). Folds the per-device config
 * (hosts/projects) and the live discovery list (sessions) into the
 * Host → Project → Session tree the sidebar renders.
 *
 *   config.hosts ─┐
 *   config.projects ─┼─▶ buildTree ─▶ HostNode[] (config order, Unconfigured last)
 *   sessions ────────┘
 *
 * No React, no I/O — node-testable. Determinism comes from the inputs:
 * `config.hosts`/`config.projects` arrive in BTreeMap order from the bridge and
 * `sessions` arrive sorted by (projectId, sessionId), so the tree order is
 * stable without re-sorting here.
 */

/**
 * Synthetic host id for discovered sessions whose project is not in config.
 * Underscores are deliberate: real host ids are `[a-z0-9-]+` (no underscores),
 * so this can never collide with a configured host.
 */
export const UNCONFIGURED_HOST_ID = "__unconfigured__";

export interface SessionNode {
  projectId: string;
  sessionId: string;
  state: SessionStateDto;
  /** Agent the session advertises (discovery), display-only; may be null. */
  agent: string | null;
  /** Identity shared with the tab store, so the sidebar can match open/active tabs. */
  key: string;
}

export interface ProjectNode {
  id: string;
  label: string;
  /** Default agent from config; null for a synthetic (unconfigured) project. */
  agent: string | null;
  sessions: SessionNode[];
}

export interface HostNode {
  id: string;
  label: string;
  /** Transport discriminant from config; null for the synthetic Unconfigured host. */
  transport: HostTransport;
  /** True only for the synthetic Unconfigured group. */
  unconfigured: boolean;
  projects: ProjectNode[];
}

type HostTransport = ConfigDto["hosts"][number]["transport"] | null;

function sessionNode(s: SessionMetaDto): SessionNode {
  return {
    projectId: s.projectId,
    sessionId: s.sessionId,
    state: s.state,
    agent: s.agent,
    key: tabKey(s.projectId, s.sessionId),
  };
}

export function buildTree(
  config: ConfigDto,
  sessions: SessionMetaDto[],
): HostNode[] {
  // 1. Seed one ProjectNode per configured project, indexed by id. Sessions
  //    append into these in pass 3; hosts adopt them in pass 4.
  const projectNodes = new Map<string, ProjectNode>();
  for (const p of config.projects) {
    projectNodes.set(p.id, {
      id: p.id,
      label: p.name ?? p.id,
      agent: p.agent,
      sessions: [],
    });
  }

  // 2. Seed host nodes in config order; empty hosts still render (T3).
  const hostNodes = new Map<string, HostNode>();
  const hosts: HostNode[] = config.hosts.map((h) => {
    const node: HostNode = {
      id: h.id,
      label: h.name ?? h.id,
      transport: h.transport,
      unconfigured: false,
      projects: [],
    };
    hostNodes.set(h.id, node);
    return node;
  });

  // 3. Place each session: into its configured project, or into the synthetic
  //    Unconfigured host (grouped into a synthetic project per unknown id).
  const unconfigured: HostNode = {
    id: UNCONFIGURED_HOST_ID,
    label: "Unconfigured",
    transport: null,
    unconfigured: true,
    projects: [],
  };
  const unconfiguredProjects = new Map<string, ProjectNode>();
  // The SessionSource trait does not promise unique (project, session) tuples
  // (multi-host discovery can surface the same one twice), so dedup by key to
  // keep React keys unique. Sessions arrive sorted, so first-wins is stable.
  const seen = new Set<string>();
  for (const s of sessions) {
    const key = tabKey(s.projectId, s.sessionId);
    if (seen.has(key)) continue;
    seen.add(key);
    const project = projectNodes.get(s.projectId);
    if (project) {
      project.sessions.push(sessionNode(s));
      continue;
    }
    let synthetic = unconfiguredProjects.get(s.projectId);
    if (!synthetic) {
      synthetic = {
        id: s.projectId,
        label: s.projectId,
        agent: null,
        sessions: [],
      };
      unconfiguredProjects.set(s.projectId, synthetic);
      unconfigured.projects.push(synthetic);
    }
    synthetic.sessions.push(sessionNode(s));
  }

  // 4. Attach configured projects to their hosts (config order preserved). A
  //    project whose host id is absent from config is itself unconfigured.
  for (const p of config.projects) {
    const node = projectNodes.get(p.id);
    if (!node) continue;
    const hostNode = hostNodes.get(p.hostId);
    if (hostNode) {
      hostNode.projects.push(node);
    } else {
      unconfigured.projects.push(node);
    }
  }

  // 5. Unconfigured group renders last, only when it has content.
  return unconfigured.projects.length > 0 ? [...hosts, unconfigured] : hosts;
}
