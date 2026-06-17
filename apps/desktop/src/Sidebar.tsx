import { useState } from "react";
import type { HostNode, ProjectNode, SessionNode } from "./session-tree";

interface SidebarProps {
  tree: HostNode[];
  /** Tab key of the active tab, highlighted in the tree. */
  activeKey: string | null;
  /** Tab keys currently open, marked so the user sees what's already a tab. */
  openKeys: Set<string>;
  /** Open (attach/focus) a live session. Stopped sessions never call this. */
  onOpenSession: (node: SessionNode) => void;
  /** Non-fatal config read error; shown as a banner above the tree. */
  configError: string | null;
  /** True when the last discovery poll failed (last good tree still shown). */
  discoveryUnavailable: boolean;
  onRefresh: () => void;
}

/**
 * Host → Project → Session tree (stage 10). A thin renderer over the
 * `buildTree` model: it owns only collapse state and click routing. Live
 * sessions open as tabs; stopped sessions are display-only (respawn is a later
 * stage). Component-render behaviour is covered by manual QA + a deferred e2e
 * (vitest runs in node with no DOM); the tree model and store are unit-tested.
 */
export function Sidebar({
  tree,
  activeKey,
  openKeys,
  onOpenSession,
  configError,
  discoveryUnavailable,
  onRefresh,
}: SidebarProps) {
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const toggle = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  return (
    <nav className="sidebar" aria-label="Sessions">
      <div className="sidebar-header">
        <span>Sessions</span>
        <button type="button" className="sidebar-refresh" onClick={onRefresh}>
          Refresh
        </button>
      </div>
      {discoveryUnavailable && (
        <div className="sidebar-warning" role="status">
          Discovery unavailable — showing last known state.
        </div>
      )}
      {configError && (
        <div className="sidebar-error" role="alert">
          Could not read config: {configError}
        </div>
      )}
      {tree.length === 0 ? (
        <p className="sidebar-empty">No hosts configured.</p>
      ) : (
        <ul className="tree">
          {tree.map((host) => (
            <HostRow
              key={host.id}
              host={host}
              collapsed={collapsed}
              toggle={toggle}
              activeKey={activeKey}
              openKeys={openKeys}
              onOpenSession={onOpenSession}
            />
          ))}
        </ul>
      )}
    </nav>
  );
}

function HostRow({
  host,
  collapsed,
  toggle,
  activeKey,
  openKeys,
  onOpenSession,
}: {
  host: HostNode;
  collapsed: Set<string>;
  toggle: (id: string) => void;
  activeKey: string | null;
  openKeys: Set<string>;
  onOpenSession: (node: SessionNode) => void;
}) {
  const isCollapsed = collapsed.has(host.id);
  return (
    <li className="tree-host">
      <button
        type="button"
        className="tree-toggle"
        onClick={() => toggle(host.id)}
      >
        <span className="tree-caret">{isCollapsed ? "▸" : "▾"}</span>
        <span className="tree-label">{host.label}</span>
        {host.transport && <span className="tree-badge">{host.transport}</span>}
      </button>
      {!isCollapsed && (
        <ul className="tree-projects">
          {host.projects.length === 0 ? (
            <li className="tree-empty">no projects</li>
          ) : (
            host.projects.map((project) => (
              <ProjectRow
                key={`${host.id}/${project.id}`}
                rowId={`${host.id}/${project.id}`}
                project={project}
                collapsed={collapsed}
                toggle={toggle}
                activeKey={activeKey}
                openKeys={openKeys}
                onOpenSession={onOpenSession}
              />
            ))
          )}
        </ul>
      )}
    </li>
  );
}

function ProjectRow({
  rowId,
  project,
  collapsed,
  toggle,
  activeKey,
  openKeys,
  onOpenSession,
}: {
  rowId: string;
  project: ProjectNode;
  collapsed: Set<string>;
  toggle: (id: string) => void;
  activeKey: string | null;
  openKeys: Set<string>;
  onOpenSession: (node: SessionNode) => void;
}) {
  const isCollapsed = collapsed.has(rowId);
  return (
    <li className="tree-project">
      <button
        type="button"
        className="tree-toggle"
        onClick={() => toggle(rowId)}
      >
        <span className="tree-caret">{isCollapsed ? "▸" : "▾"}</span>
        <span className="tree-label">{project.label}</span>
      </button>
      {!isCollapsed && (
        <ul className="tree-sessions">
          {project.sessions.length === 0 ? (
            <li className="tree-empty">no sessions</li>
          ) : (
            project.sessions.map((session) => (
              <SessionRow
                key={session.key}
                session={session}
                active={session.key === activeKey}
                open={openKeys.has(session.key)}
                onOpenSession={onOpenSession}
              />
            ))
          )}
        </ul>
      )}
    </li>
  );
}

function SessionRow({
  session,
  active,
  open,
  onOpenSession,
}: {
  session: SessionNode;
  active: boolean;
  open: boolean;
  onOpenSession: (node: SessionNode) => void;
}) {
  const stopped = session.state === "stopped";
  return (
    <li className="tree-session">
      <button
        type="button"
        className={`tree-session-button${active ? " tree-session-button--active" : ""}`}
        // Stopped sessions are display-only until respawn lands (later stage).
        disabled={stopped}
        aria-current={active ? "true" : undefined}
        title={stopped ? "Stopped — respawn comes in a later stage" : undefined}
        onClick={() => onOpenSession(session)}
      >
        <span
          className={`tree-dot tree-dot--${session.state}`}
          aria-hidden="true"
        />
        <span className="tree-label">{session.sessionId}</span>
        {open && (
          <span className="tree-open" role="img" aria-label="open tab">
            ●
          </span>
        )}
      </button>
    </li>
  );
}
