import { useEffect, useRef, useState } from "react";
import type { ActivityState } from "./activity-store";
import wordmark from "./assets/remora-wordmark.svg";
import { filterTree } from "./filter-tree";
import type { HostTransport, ProjectNode, SessionNode } from "./session-tree";
import { SIDEBAR_COLLAPSE_LABEL } from "./sidebar-labels";
import { sessionIndicatorState } from "./status-state";
import { Avatar, IconButton, SessionRow, useTheme } from "./ui";
import {
  AlertTriangle,
  ChevronRight,
  Folder,
  Kubectl,
  Moon,
  More,
  Plus,
  RotateCw,
  Search,
  Settings,
  Sidebar as SidebarIcon,
  Ssh,
  Sun,
  Trash,
  Unplug,
} from "./ui/icons";

interface SidebarProps {
  tree: ProjectNode[];
  /** Tab key of the active tab, highlighted in the tree. */
  activeKey: string | null;
  /** Tab keys currently open — the sessions we have a live terminal for. Only
   * these show an activity dot; the rest read as disconnected (muted, no dot). */
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
  /** Open Settings deep-linked to the new-project form (section-header "+"). */
  onAddProject: () => void;
  activity: ReadonlyMap<string, ActivityState>;
  /** Collapse the sidebar to the narrow icon rail. */
  onCollapse?: () => void;
}

/** The transport glyph folded into the host label. null/unknown → no glyph. */
function transportGlyph(transport: HostTransport) {
  switch (transport) {
    case "ssh":
      return <Ssh size={11} className="rk-proj__hostglyph" />;
    case "kubectl":
      return <Kubectl size={11} className="rk-proj__hostglyph" />;
    default:
      return null;
  }
}

/**
 * Project → Session list. A thin renderer over the `buildTree` model: it owns
 * only collapse state, an inline search filter, and click routing. Host is a
 * bare label on each project row, not a tree level; projects are pre-grouped by
 * host in the model. Live sessions open as tabs; stopped sessions are clickable
 * and route through App's openFromSidebar which branches on node.state to trigger
 * respawn. Component-render behaviour is covered by manual QA + a deferred e2e
 * (vitest runs in node with no DOM); the tree model, filter, and store are
 * unit-tested.
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
  onAddProject,
  activity,
  onCollapse,
}: SidebarProps) {
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const toggle = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const [searchOpen, setSearchOpen] = useState(false);
  const [query, setQuery] = useState("");
  const { theme, cycle } = useTheme();

  const filtered = filterTree(tree, query);
  const ThemeIcon = theme === "dark" ? Moon : Sun;

  return (
    <nav className="rk-sidebar" aria-label="Sessions">
      <div className="rk-sidebar__top">
        <div className="rk-brand">
          <img
            src={wordmark}
            height={20}
            alt="Remora"
            className="rk-brand__img"
          />
        </div>
        <div className="rk-sidebar__actions">
          <IconButton
            label="Search"
            size="sm"
            active={searchOpen}
            onClick={() => setSearchOpen((v) => !v)}
          >
            <Search size={15} />
          </IconButton>
          <IconButton label="Refresh" size="sm" onClick={onRefresh}>
            <RotateCw size={15} />
          </IconButton>
          {onCollapse && (
            <IconButton
              label={SIDEBAR_COLLAPSE_LABEL}
              size="sm"
              onClick={onCollapse}
            >
              <SidebarIcon size={15} />
            </IconButton>
          )}
        </div>
      </div>

      {searchOpen && (
        <div className="rk-sidebar__searchwrap">
          <input
            className="rk-sidebar__search"
            type="text"
            placeholder="Filter sessions…"
            aria-label="Filter sessions"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>
      )}

      {discoveryUnavailable && (
        <div
          className="rk-sidebar__banner rk-sidebar__banner--warning"
          role="status"
        >
          <AlertTriangle size={14} />
          <span>Discovery unavailable — showing last known state.</span>
        </div>
      )}
      {configError && (
        <div
          className="rk-sidebar__banner rk-sidebar__banner--danger"
          role="alert"
        >
          <AlertTriangle size={14} />
          <span>Could not read config: {configError}</span>
        </div>
      )}

      <div className="rk-sidebar__scroll">
        <div className="rk-sidebar__label">
          <span className="rk-sidebar__label-text">Projects</span>
          <IconButton label="New project" size="sm" onClick={onAddProject}>
            <Plus size={14} />
          </IconButton>
        </div>
        {filtered.length === 0 ? (
          <p className="rk-sidebar__empty">No projects yet.</p>
        ) : (
          filtered.map((project) => (
            <ProjectGroup
              key={project.id}
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
      </div>

      <div className="rk-sidebar__foot">
        <Avatar shape="circle" size="sm" name="Remora" />
        <span className="rk-sidebar__brand">Remora</span>
        <div className="rk-sidebar__foot-actions">
          <IconButton label={`Theme: ${theme}`} size="sm" onClick={cycle}>
            <ThemeIcon size={15} />
          </IconButton>
          <IconButton label="Settings" size="sm" onClick={onOpenSettings}>
            <Settings size={15} />
          </IconButton>
        </div>
      </div>
    </nav>
  );
}

/** One project group: a collapse header (chevron + folder + name + a bare muted
 * host label with its transport glyph) plus its session leaves. A hover-revealed
 * per-project "+" starts a session pre-scoped to it — shown only for fully
 * configured projects (a synthetic or dangling-host project can't be resolved by
 * the New Session dialog). */
function ProjectGroup({
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
  const open = !collapsed.has(project.id);
  // Only fully configured projects can be pre-scoped: synthetic projects have
  // agent === null, and a dangling-host configured project (unconfigured) points
  // at a host the New Session dialog can't resolve, so it would spawn into
  // nowhere. Guard on both.
  const canStartSession = project.agent !== null && !project.unconfigured;
  return (
    <div className="rk-proj">
      <div className="rk-proj__hdr">
        <button
          type="button"
          className="rk-proj__toggle"
          aria-label={`${project.label} on ${project.hostLabel}`}
          aria-expanded={open}
          onClick={() => toggle(project.id)}
        >
          <span
            className="rk-proj__chev"
            style={{ transform: open ? "rotate(90deg)" : "none" }}
          >
            <ChevronRight size={11} />
          </span>
          <Folder size={13} className="rk-proj__icon" />
          <span className="rk-proj__name">{project.label}</span>
          <span
            className={
              project.unconfigured
                ? "rk-proj__host rk-proj__host--unconfigured"
                : "rk-proj__host"
            }
            title={project.hostLabel}
          >
            {transportGlyph(project.transport)}
            {project.hostLabel}
          </span>
        </button>
        {canStartSession && (
          <span className="rk-proj__actions">
            <IconButton
              label={`New session in ${project.label}`}
              size="sm"
              onClick={() => onNewSession(project.id)}
            >
              <Plus size={14} />
            </IconButton>
          </span>
        )}
      </div>
      {open && (
        <div className="rk-proj__rows">
          {project.sessions.map((session) => {
            const stopped = session.state === "stopped";
            return (
              <SessionRow
                key={session.key}
                name={session.branch ?? session.sessionId}
                agent={session.agent ?? undefined}
                branch={null}
                state={sessionIndicatorState(
                  session.state,
                  activity.get(session.key),
                )}
                connected={openKeys.has(session.key)}
                active={session.key === activeKey}
                aria-current={session.key === activeKey ? "true" : undefined}
                title={stopped ? "Stopped — click to respawn" : undefined}
                onClick={() => onOpenSession(session)}
                reconnecting={session.reconnecting}
                actions={
                  <SessionMenu
                    session={session}
                    onStop={onStop}
                    onRemove={onRemove}
                  />
                }
              />
            );
          })}
        </div>
      )}
    </div>
  );
}

/** Hover-revealed per-session menu: Stop (worktree live only) / Remove session.
 * Outside-click closes it (mousedown listener); the trigger stops propagation so
 * opening the menu never also opens the session. */
function SessionMenu({
  session,
  onStop,
  onRemove,
}: {
  session: SessionNode;
  onStop: (node: SessionNode) => void;
  onRemove: (node: SessionNode) => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (ev: MouseEvent) => {
      if (ref.current && !ref.current.contains(ev.target as Node))
        setOpen(false);
    };
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const canStop = session.state === "live" && session.workspace === "worktree";
  const pick = (fn: (node: SessionNode) => void) => (ev: React.MouseEvent) => {
    ev.stopPropagation();
    setOpen(false);
    fn(session);
  };

  return (
    <span className="rk-smenu" ref={ref}>
      <IconButton
        label={`Session actions for ${session.sessionId}`}
        size="sm"
        active={open}
        aria-expanded={open}
        onClick={(ev) => {
          ev.stopPropagation();
          setOpen((o) => !o);
        }}
      >
        <More size={15} />
      </IconButton>
      {open && (
        <div className="rk-smenu__pop" role="menu">
          {canStop && (
            <button
              type="button"
              className="rk-smenu__item"
              role="menuitem"
              onClick={pick(onStop)}
            >
              <Unplug size={14} />
              Stop
            </button>
          )}
          <button
            type="button"
            className="rk-smenu__item rk-smenu__item--danger"
            role="menuitem"
            onClick={pick(onRemove)}
          >
            <Trash size={14} />
            Remove session
          </button>
        </div>
      )}
    </span>
  );
}
