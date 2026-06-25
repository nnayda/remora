import { useState } from "react";
import type { ActivityState } from "./activity-store";
import type { HostNode, ProjectNode, SessionNode } from "./session-tree";

interface SidebarProps {
  tree: HostNode[];
  /** Tab key of the active tab, highlighted in the tree. */
  activeKey: string | null;
  /** Tab keys currently open, marked so the user sees what's already a tab. */
  openKeys: Set<string>;
  /** Open (attach/focus) a session. App routes by node.state: live→attach, stopped→respawn. */
  onOpenSession: (node: SessionNode) => void;
  /** Non-fatal config read error; shown as a banner above the tree. */
  configError: string | null;
  /** True when the last discovery poll failed (last good tree still shown). */
  discoveryUnavailable: boolean;
  onRefresh: () => void;
  /** Stop a live worktree session (kills tmux, keeps the worktree). */
  onStop: (node: SessionNode) => void;
  /** Open the remove confirm dialog for any session. */
  onRemove: (node: SessionNode) => void;
  /** Open the New Session dialog pre-scoped to a project (per-project "+"). */
  onNewSession: (projectId: string) => void;
  /** Open the config-management (Settings) modal. */
  onOpenSettings: () => void;
  activity: ReadonlyMap<string, ActivityState>;
}

/**
 * Host → Project → Session tree. A thin renderer over the `buildTree` model:
 * it owns only collapse state and click routing. Live sessions open as tabs;
 * stopped sessions are clickable and route through App's openFromSidebar which
 * branches on node.state to trigger respawn. Component-render behaviour is
 * covered by manual QA + a deferred e2e (vitest runs in node with no DOM); the
 * tree model and store are unit-tested.
 */
export function Sidebar({
  tree,
  activeKey,
  openKeys,
  onOpenSession,
  configError,
  discoveryUnavailable,
  onRefresh,
  onStop,
  onRemove,
  onNewSession,
  onOpenSettings,
  activity,
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
        <div className="sidebar-header-actions">
          <button
            type="button"
            className="sidebar-settings"
            onClick={onOpenSettings}
            aria-label="Settings"
            title="Settings"
          >
            ⚙
          </button>
          <button type="button" className="sidebar-refresh" onClick={onRefresh}>
            Refresh
          </button>
        </div>
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
              onStop={onStop}
              onRemove={onRemove}
              onNewSession={onNewSession}
              activity={activity}
            />
          ))}
        </ul>
      )}
    </nav>
  );
}

/** One host row: a collapse toggle plus its project rows (or an empty hint). */
function HostRow({
  host,
  collapsed,
  toggle,
  activeKey,
  openKeys,
  onOpenSession,
  onStop,
  onRemove,
  onNewSession,
  activity,
}: {
  host: HostNode;
  collapsed: Set<string>;
  toggle: (id: string) => void;
  activeKey: string | null;
  openKeys: Set<string>;
  onOpenSession: (node: SessionNode) => void;
  onStop: (node: SessionNode) => void;
  onRemove: (node: SessionNode) => void;
  onNewSession: (projectId: string) => void;
  activity: ReadonlyMap<string, ActivityState>;
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
                onStop={onStop}
                onRemove={onRemove}
                onNewSession={onNewSession}
                activity={activity}
              />
            ))
          )}
        </ul>
      )}
    </li>
  );
}

/** One project row: a collapse toggle, a per-project "+" to start a session
 * pre-scoped to it, plus its session rows (or an empty hint). */
function ProjectRow({
  rowId,
  project,
  collapsed,
  toggle,
  activeKey,
  openKeys,
  onOpenSession,
  onStop,
  onRemove,
  onNewSession,
  activity,
}: {
  rowId: string;
  project: ProjectNode;
  collapsed: Set<string>;
  toggle: (id: string) => void;
  activeKey: string | null;
  openKeys: Set<string>;
  onOpenSession: (node: SessionNode) => void;
  onStop: (node: SessionNode) => void;
  onRemove: (node: SessionNode) => void;
  onNewSession: (projectId: string) => void;
  activity: ReadonlyMap<string, ActivityState>;
}) {
  const isCollapsed = collapsed.has(rowId);
  // Only configured projects can be pre-scoped — a synthetic "Unconfigured"
  // project (agent === null) isn't in config, so the dialog couldn't resolve it.
  const canStartSession = project.agent !== null;
  return (
    <li className="tree-project">
      <div className="tree-project-row">
        <button
          type="button"
          className="tree-toggle"
          onClick={() => toggle(rowId)}
        >
          <span className="tree-caret">{isCollapsed ? "▸" : "▾"}</span>
          <span className="tree-label">{project.label}</span>
        </button>
        {canStartSession && (
          <button
            type="button"
            className="tree-project-new"
            aria-label={`New session in ${project.label}`}
            title="New session in this project"
            onClick={() => onNewSession(project.id)}
          >
            +
          </button>
        )}
      </div>
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
                onStop={onStop}
                onRemove={onRemove}
                activity={activity.get(session.key)}
              />
            ))
          )}
        </ul>
      )}
    </li>
  );
}

/** One session leaf: a state dot, the session id, an open-tab marker, and a
 * ⋮ menu with Stop (worktree live only) and Remove… actions. */
function SessionRow({
  session,
  active,
  open,
  onOpenSession,
  onStop,
  onRemove,
  activity,
}: {
  session: SessionNode;
  active: boolean;
  open: boolean;
  onOpenSession: (node: SessionNode) => void;
  onStop: (node: SessionNode) => void;
  onRemove: (node: SessionNode) => void;
  activity?: ActivityState;
}) {
  const [menuOpen, setMenuOpen] = useState(false);
  const stopped = session.state === "stopped";
  const canStop = session.state === "live" && session.workspace === "worktree";

  return (
    <li className="tree-session">
      <div className="tree-session-row">
        <button
          type="button"
          className={`tree-session-button${active ? " tree-session-button--active" : ""}`}
          // Live → attach/focus; stopped → respawn (App routes by session.state).
          aria-current={active ? "true" : undefined}
          title={stopped ? "Stopped — click to respawn" : undefined}
          onClick={() => {
            setMenuOpen(false);
            onOpenSession(session);
          }}
        >
          <span
            className={
              open && activity && activity !== "unknown"
                ? `tree-dot tree-dot--${session.state} tree-dot--act-${activity}`
                : `tree-dot tree-dot--${session.state}`
            }
            aria-hidden="true"
          />
          <span className="tree-label">{session.sessionId}</span>
          {open && (
            <span className="tree-open" role="img" aria-label="open tab">
              ●
            </span>
          )}
        </button>
        <button
          type="button"
          className="tree-session-menu-toggle"
          aria-label={`Session actions for ${session.sessionId}`}
          aria-expanded={menuOpen}
          onClick={(e) => {
            e.stopPropagation();
            setMenuOpen((v) => !v);
          }}
        >
          ⋮
        </button>
      </div>
      {menuOpen && (
        <div className="tree-session-menu">
          {canStop && (
            <button
              type="button"
              onClick={() => {
                setMenuOpen(false);
                onStop(session);
              }}
            >
              Stop
            </button>
          )}
          <button
            type="button"
            onClick={() => {
              setMenuOpen(false);
              onRemove(session);
            }}
          >
            Remove…
          </button>
        </div>
      )}
    </li>
  );
}
