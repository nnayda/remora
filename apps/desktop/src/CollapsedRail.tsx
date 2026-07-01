import type { ActivityState } from "./activity-store";
import { type RailEntry, railEntries } from "./rail-glyph";
import type { ProjectNode, SessionNode } from "./session-tree";
import { SIDEBAR_EXPAND_LABEL } from "./sidebar-labels";
import type { IndicatorState } from "./status-state";
import { IconButton, StatusIndicator, Tooltip, useTheme } from "./ui";
import { ChevronRight, Moon, Settings, Sun } from "./ui/icons";

interface CollapsedRailProps {
  tree: ProjectNode[];
  activeKey: string | null;
  openKeys: Set<string>;
  connectingKeys: Set<string>;
  activity: ReadonlyMap<string, ActivityState>;
  /** Focus/open a session (same handler the expanded sidebar uses). */
  onOpenSession: (node: SessionNode) => void;
  /** Expand back to the full sidebar. */
  onExpand: () => void;
  onOpenSettings: () => void;
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

/**
 * The 56px consolidated rail shown when the sidebar is collapsed: an expand
 * toggle, one glyph PER SESSION (project icon = shape identity, a mono
 * branch-initial to tell same-project sessions apart, a per-session activity
 * dot), and the theme + settings foot. Grouping is by shape + a hairline
 * divider — marine-only, no color (see #184 / DESIGN.md). Purely navigational;
 * errors/banners surface only when expanded.
 */
export function CollapsedRail({
  tree,
  activeKey,
  openKeys,
  connectingKeys,
  activity,
  onOpenSession,
  onExpand,
  onOpenSettings,
}: CollapsedRailProps) {
  const { theme, cycle } = useTheme();
  const ThemeIcon = theme === "dark" ? Moon : Sun;
  const entries = railEntries(
    tree,
    activeKey,
    openKeys,
    activity,
    connectingKeys,
  );

  return (
    <nav className="rk-railmini" aria-label="Sessions (collapsed)">
      <div className="rk-railmini__top">
        <IconButton label={SIDEBAR_EXPAND_LABEL} size="sm" onClick={onExpand}>
          <ChevronRight size={15} />
        </IconButton>
      </div>

      <div className="rk-railmini__sessions">
        {entries.map((entry: RailEntry) => {
          const { Icon } = entry;
          const cls = [
            "rk-railmini__session",
            entry.active ? "is-active" : "",
            entry.reconnecting ? "is-reconnecting" : "",
            entry.firstOfProject ? "is-project-start" : "",
          ]
            .filter(Boolean)
            .join(" ");
          const activity =
            entry.connected || entry.connecting
              ? `, ${entry.connecting ? "connecting" : activityPhrase(entry.status)}`
              : "";
          return (
            <Tooltip
              key={entry.key}
              content={`${entry.branchLabel} · ${entry.hostLabel}`}
              side="right"
            >
              <button
                type="button"
                className={cls}
                aria-current={entry.active ? "page" : undefined}
                aria-label={`${entry.branchLabel}, ${entry.hostLabel}${activity}`}
                onClick={() => onOpenSession(entry.session)}
              >
                <span className="rk-railmini__glyph">
                  <Icon size={18} />
                  <span className="rk-railmini__initial" aria-hidden="true">
                    {entry.initial}
                  </span>
                </span>
                <span className="rk-railmini__dot">
                  {entry.connecting ? (
                    <span
                      className="rk-railmini__spinner"
                      role="status"
                      aria-label="Connecting…"
                    />
                  ) : entry.connected ? (
                    <StatusIndicator state={entry.status} />
                  ) : null}
                </span>
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
