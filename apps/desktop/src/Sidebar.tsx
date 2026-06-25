import { useEffect, useRef, useState } from "react";
import type { ActivityState } from "./activity-store";
import wordmark from "./assets/remora-wordmark.svg";
import type { HostNode, ProjectNode, SessionNode } from "./session-tree";
import { sessionIndicatorState } from "./status-state";
import { Avatar, IconButton, SessionRow, Tag, useTheme } from "./ui";
import {
  AlertTriangle,
  ChevronRight,
  Folder,
  Moon,
  More,
  Plus,
  RotateCw,
  Search,
  Settings,
  Sun,
  Trash,
  Unplug,
} from "./ui/icons";

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
 * it owns only collapse state, an inline search filter, and click routing. Live
 * sessions open as tabs; stopped sessions are clickable and route through App's
 * openFromSidebar which branches on node.state to trigger respawn. Component-
 * render behaviour is covered by manual QA + a deferred e2e (vitest runs in node
 * with no DOM); the tree model and store are unit-tested.
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
        <div className="rk-sidebar__label">Hosts</div>
        {filtered.length === 0 ? (
          <p className="rk-sidebar__empty">No hosts configured.</p>
        ) : (
          filtered.map((host) => (
            <HostGroup
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

/** Filter the rendered tree client-side by case-insensitive substring over host
 * label, project label, and sessionId. A host or project that matches keeps all
 * of its descendants; otherwise only matching sessions survive. Empty query →
 * the tree unchanged. */
function filterTree(tree: HostNode[], query: string): HostNode[] {
  const q = query.trim().toLowerCase();
  if (!q) return tree;
  const out: HostNode[] = [];
  for (const host of tree) {
    const hostMatch = host.label.toLowerCase().includes(q);
    const projects: ProjectNode[] = [];
    for (const project of host.projects) {
      const projMatch = project.label.toLowerCase().includes(q);
      const sessions =
        hostMatch || projMatch
          ? project.sessions
          : project.sessions.filter((s) =>
              s.sessionId.toLowerCase().includes(q),
            );
      if (hostMatch || projMatch || sessions.length > 0) {
        projects.push(
          sessions === project.sessions ? project : { ...project, sessions },
        );
      }
    }
    if (hostMatch || projects.length > 0) {
      out.push({ ...host, projects });
    }
  }
  return out;
}

/** One host group: a collapse header (chevron + avatar + name + count + the
 * transport tag) plus its project groups. */
function HostGroup({
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
  const open = !collapsed.has(host.id);
  const count = host.projects.reduce((n, p) => n + p.sessions.length, 0);
  return (
    <div className="rk-host">
      <button
        type="button"
        className="rk-host__hdr"
        onClick={() => toggle(host.id)}
      >
        <span
          className="rk-host__chev"
          style={{ transform: open ? "rotate(90deg)" : "none" }}
        >
          <ChevronRight size={12} />
        </span>
        <Avatar host name={host.label} size="sm" />
        <span className="rk-host__name">{host.label}</span>
        <span className="rk-host__count">{count}</span>
        {host.transport && <Tag>{host.transport}</Tag>}
      </button>
      {open && (
        <div className="rk-host__rows">
          {host.projects.map((project) => (
            <ProjectGroup
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
          ))}
        </div>
      )}
    </div>
  );
}

/** One project group: a collapse header (chevron + folder + name + count) with a
 * hover-revealed per-project "+" to start a session pre-scoped to it, plus its
 * session leaves. */
function ProjectGroup({
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
  const open = !collapsed.has(rowId);
  // Only configured projects can be pre-scoped — a synthetic "Unconfigured"
  // project (agent === null) isn't in config, so the dialog couldn't resolve it.
  const canStartSession = project.agent !== null;
  return (
    <div className="rk-proj">
      <div className="rk-proj__hdr">
        <button
          type="button"
          className="rk-proj__toggle"
          onClick={() => toggle(rowId)}
        >
          <span
            className="rk-proj__chev"
            style={{ transform: open ? "rotate(90deg)" : "none" }}
          >
            <ChevronRight size={11} />
          </span>
          <Folder size={13} className="rk-proj__icon" />
          <span className="rk-proj__name">{project.label}</span>
          <span className="rk-proj__count">{project.sessions.length}</span>
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
                name={session.sessionId}
                agent={session.agent ?? undefined}
                branch={null}
                state={sessionIndicatorState(
                  session.state,
                  activity.get(session.key),
                )}
                active={session.key === activeKey}
                aria-current={session.key === activeKey ? "true" : undefined}
                title={stopped ? "Stopped — click to respawn" : undefined}
                onClick={() => onOpenSession(session)}
                actions={
                  <span className="rk-srow-trail">
                    {openKeys.has(session.key) && (
                      <span
                        className="rk-srow__open"
                        role="img"
                        aria-label="Open tab"
                        title="Open in a tab"
                      />
                    )}
                    <SessionMenu
                      session={session}
                      onStop={onStop}
                      onRemove={onRemove}
                    />
                  </span>
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
