import type {
  ConfigDto,
  SessionMetaDto,
  SessionStateDto,
  WorkspaceModeDto,
} from "./bindings";
import { tabKey } from "./session-store";

/**
 * Pure config-and-discovery join. Folds the per-device config (hosts/projects)
 * and the live discovery list (sessions) into the flat, host-grouped
 * `ProjectNode[]` the sidebar renders.
 *
 *   config.hosts ────┐   (host label + transport stamped onto each project)
 *   config.projects ─┼─▶ buildTree ─▶ ProjectNode[]
 *   sessions ────────┘                  ├─ host A's projects (config order)
 *                                       ├─ host B's projects (config order)
 *                                       ├─ dangling-host projects (host id not in config)
 *                                       └─ synthetic projects (sessions with no config project)
 *
 * The host level is a label on each project row, not a tree level: projects are
 * grouped adjacently by host via a bucket pass (NOT a sort — no reliance on
 * `Array.sort` stability). Order is deterministic from the inputs:
 * `config.hosts`/`config.projects` arrive in BTreeMap order from the bridge and
 * `sessions` arrive sorted by (projectId, sessionId), so within-host order and
 * the trailing buckets are stable without re-sorting.
 */

export interface SessionNode {
  projectId: string;
  sessionId: string;
  state: SessionStateDto;
  /** Agent the session advertises (discovery), display-only; may be null. */
  agent: string | null;
  /** Identity shared with the tab store, so the sidebar can match open/active tabs. */
  key: string;
  /**
   * Workspace mode inherited from the owning configured project, or null for
   * unconfigured-project sessions whose workspace mode is unknown. Used by the
   * UI to gate the Stop action (worktree-mode live sessions only).
   */
  workspace: WorkspaceModeDto | null;
  /** Host of this session is currently unavailable; its row is retained and
   * shown dimmed/"reconnecting" rather than removed (#159). */
  reconnecting: boolean;
  /**
   * Git branch the session advertises (ADR-0015). This is the user-facing
   * identity shown in the sidebar; falls back to sessionId when null
   * (e.g. detached HEAD).
   */
  branch: string | null;
}

export interface ProjectNode {
  id: string;
  label: string;
  /** Default agent from config; null for a synthetic (discovered) project. */
  agent: string | null;
  /**
   * Host display name, rendered as a bare muted label on the project row. For a
   * project whose `hostId` is missing from config (dangling reference) this is
   * the raw `hostId`; for a synthetic discovered project it is "Unconfigured".
   */
  hostLabel: string;
  /** Transport of the owning host; null for dangling/synthetic projects. */
  transport: HostTransport;
  /**
   * True when the project is not fully configured: either a discovered session
   * with no config project (synthetic), or a configured project whose host id is
   * absent from config (dangling). The UI reads this flag to render the dim
   * treatment and suppress the "+" new-session affordance.
   */
  unconfigured: boolean;
  sessions: SessionNode[];
}

export type HostTransport = ConfigDto["hosts"][number]["transport"] | null;

/** Project a discovered `SessionMetaDto` into a tree leaf, stamping the
 * tab-store `key` so the sidebar can match it against open/active tabs.
 * `workspace` is the owning configured project's mode, or null for sessions
 * whose project is not in config. */
function sessionNode(
  s: SessionMetaDto,
  workspace: WorkspaceModeDto | null,
  reconnecting: boolean,
): SessionNode {
  return {
    projectId: s.projectId,
    sessionId: s.sessionId,
    state: s.state,
    agent: s.agent,
    key: tabKey(s.projectId, s.sessionId),
    workspace,
    reconnecting,
    branch: s.branch,
  };
}

/** Fold config and the discovery list into the flat, host-grouped
 * `ProjectNode[]` the sidebar renders. Projects cluster by host (config.hosts
 * order); dangling-host projects then synthetic discovered projects trail last.
 * Duplicate (project, session) tuples are deduped first-wins (see module doc). */
export function buildTree(
  config: ConfigDto,
  sessions: SessionMetaDto[],
  reconnectingKeys: Set<string> = new Set(),
): ProjectNode[] {
  const projectWorkspace = new Map<string, WorkspaceModeDto>(
    config.projects.map((p) => [p.id, p.workspace]),
  );
  // Host lookup for stamping label/transport onto projects — not for grouping.
  const hostLookup = new Map(config.hosts.map((h) => [h.id, h]));

  // 1. Seed one ProjectNode per configured project, indexed by id. A project
  //    whose host id is absent from config is a dangling reference: stamp the
  //    raw hostId and mark it unconfigured.
  const projectNodes = new Map<string, ProjectNode>();
  for (const p of config.projects) {
    const host = hostLookup.get(p.hostId);
    projectNodes.set(p.id, {
      id: p.id,
      label: p.name ?? p.id,
      agent: p.agent,
      hostLabel: host ? (host.name ?? host.id) : p.hostId,
      transport: host ? host.transport : null,
      unconfigured: host === undefined,
      sessions: [],
    });
  }

  // 2. Place each session: into its configured project, or into a synthetic
  //    project (one per unknown projectId, kept in discovery order).
  //    The SessionSource trait does not promise unique (project, session) tuples
  //    (multi-host discovery can surface the same one twice), so dedup by key to
  //    keep React keys unique. Sessions arrive sorted, so first-wins is stable.
  const synthetic = new Map<string, ProjectNode>();
  const syntheticOrder: ProjectNode[] = [];
  const seen = new Set<string>();
  for (const s of sessions) {
    const key = tabKey(s.projectId, s.sessionId);
    if (seen.has(key)) continue;
    seen.add(key);
    const configured = projectNodes.get(s.projectId);
    if (configured) {
      configured.sessions.push(
        sessionNode(
          s,
          s.workspace ?? projectWorkspace.get(s.projectId) ?? null,
          reconnectingKeys.has(key),
        ),
      );
      continue;
    }
    let syn = synthetic.get(s.projectId);
    if (!syn) {
      syn = {
        id: s.projectId,
        label: s.projectId,
        agent: null,
        hostLabel: "Unconfigured",
        transport: null,
        unconfigured: true,
        sessions: [],
      };
      synthetic.set(s.projectId, syn);
      syntheticOrder.push(syn);
    }
    syn.sessions.push(
      sessionNode(s, s.workspace ?? null, reconnectingKeys.has(key)),
    );
  }

  // 3. Bucket projects by host (config.hosts order seeds the buckets so empty
  //    hosts contribute nothing and host order is preserved). Within a host,
  //    config.projects order is preserved. Dangling-host projects collect
  //    separately and trail the configured hosts.
  const byHost = new Map<string, ProjectNode[]>();
  for (const h of config.hosts) byHost.set(h.id, []);
  const dangling: ProjectNode[] = [];
  for (const p of config.projects) {
    const node = projectNodes.get(p.id);
    if (!node) continue;
    const bucket = byHost.get(p.hostId);
    if (bucket) bucket.push(node);
    else dangling.push(node);
  }

  // 4. Flatten: configured hosts in order, then dangling, then synthetic.
  const result: ProjectNode[] = [];
  for (const h of config.hosts) result.push(...(byHost.get(h.id) ?? []));
  result.push(...dangling, ...syntheticOrder);
  return result;
}
