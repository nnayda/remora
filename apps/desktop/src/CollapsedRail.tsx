import type { ActivityState } from "./activity-store";
import { hostIndicatorState } from "./host-activity";
import type { ProjectNode } from "./session-tree";
import { SIDEBAR_EXPAND_LABEL } from "./sidebar-labels";
import type { IndicatorState } from "./status-state";
import { Avatar, IconButton, StatusIndicator, Tooltip, useTheme } from "./ui";
import { ChevronRight, Moon, Settings, Sun } from "./ui/icons";

interface CollapsedRailProps {
  tree: ProjectNode[];
  activeKey: string | null;
  openKeys: Set<string>;
  activity: ReadonlyMap<string, ActivityState>;
  /** Expand back to the full sidebar. */
  onExpand: () => void;
  onOpenSettings: () => void;
}

interface HostEntry {
  label: string;
  sessions: ProjectNode["sessions"];
  hasActive: boolean;
}

/** Map an IndicatorState to a human-readable phrase for aria-labels. */
function activityPhrase(state: IndicatorState): string {
  switch (state) {
    case "needs":
      return "needs attention";
    case "working":
      return "working";
    case "error":
      return "error";
    case "done":
      return "done";
    case "idle":
      return "idle";
  }
}

/** Group projects by host label (deduped, tree order) for the narrow rail. */
function hostsFromTree(
  tree: ProjectNode[],
  activeKey: string | null,
): HostEntry[] {
  const order: string[] = [];
  const byHost = new Map<string, HostEntry>();
  for (const project of tree) {
    let entry = byHost.get(project.hostLabel);
    if (!entry) {
      entry = { label: project.hostLabel, sessions: [], hasActive: false };
      byHost.set(project.hostLabel, entry);
      order.push(project.hostLabel);
    }
    entry.sessions = entry.sessions.concat(project.sessions);
    if (project.sessions.some((s) => s.key === activeKey))
      entry.hasActive = true;
  }
  return order.map((label) => byHost.get(label) as HostEntry);
}

/**
 * The 56px consolidated rail shown when the sidebar is collapsed: an expand
 * toggle, one avatar per host with an aggregate activity dot, and the theme +
 * settings foot. Errors/banners are intentionally not shown here (no room) —
 * they surface when expanded. Purely navigational.
 */
export function CollapsedRail({
  tree,
  activeKey,
  openKeys,
  activity,
  onExpand,
  onOpenSettings,
}: CollapsedRailProps) {
  const { theme, cycle } = useTheme();
  const ThemeIcon = theme === "dark" ? Moon : Sun;
  const hosts = hostsFromTree(tree, activeKey);

  return (
    <nav className="rk-railmini" aria-label="Sessions (collapsed)">
      <div className="rk-railmini__top">
        <IconButton label={SIDEBAR_EXPAND_LABEL} size="sm" onClick={onExpand}>
          <ChevronRight size={15} />
        </IconButton>
      </div>

      <div className="rk-railmini__hosts">
        {hosts.map((host) => {
          const state = hostIndicatorState(host.sessions, openKeys, activity);
          const count = host.sessions.length;
          return (
            <Tooltip key={host.label} content={host.label} side="right">
              <button
                type="button"
                className={
                  host.hasActive
                    ? "rk-railmini__host is-active"
                    : "rk-railmini__host"
                }
                aria-current={host.hasActive ? "page" : undefined}
                aria-label={`${host.label}, ${count} session${count === 1 ? "" : "s"}, ${activityPhrase(state)}`}
                onClick={onExpand}
              >
                <Avatar shape="circle" size="sm" name={host.label} />
                <StatusIndicator state={state} />
              </button>
            </Tooltip>
          );
        })}
      </div>

      <div className="rk-railmini__foot">
        <IconButton label={`Theme: ${theme}`} size="sm" onClick={cycle}>
          <ThemeIcon size={15} />
        </IconButton>
        <IconButton label="Settings" size="sm" onClick={onOpenSettings}>
          <Settings size={15} />
        </IconButton>
      </div>
    </nav>
  );
}
