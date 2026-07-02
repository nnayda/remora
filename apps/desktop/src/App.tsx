import { useEffect, useMemo, useRef, useState } from "react";
import {
  commands,
  type DetectedTerminalDto,
  type DirtyReasonDto,
  type WorkspaceModeDto,
} from "./bindings";
import { CollapsedRail } from "./CollapsedRail";
import { ConfirmRemoveDialog } from "./ConfirmRemoveDialog";
import { subscribeConfigChanged } from "./config-watch-listener";
import { DiffPanel } from "./DiffPanel";
import {
  externalTerminalLabel,
  runCopyAttach,
  runOpenExternal,
  runOpenInVscode,
} from "./external-terminal";
import { NewSessionDialog } from "./NewSessionDialog";
import { SettingsDialog, type View as SettingsView } from "./SettingsDialog";
import { Sidebar } from "./Sidebar";
import {
  canRespawn,
  OPEN_CANCELLED,
  routeRemoveResult,
  tabKey,
} from "./session-store";
import { buildTree, type SessionNode } from "./session-tree";
import {
  shouldDisarmAfterSidebarOpen,
  shouldFocusActiveTabInPlace,
} from "./sidebar-focus";
import { SIDEBAR_COLLAPSE_LABEL, SIDEBAR_EXPAND_LABEL } from "./sidebar-labels";
import { TabBar } from "./TabBar";
import { Terminal, type TerminalHandle } from "./Terminal";
import {
  Button,
  effectiveMax,
  IconButton,
  ResizeHandle,
  shouldRenderCollapsed,
  Tag,
  useIsMobile,
  useRailWidth,
} from "./ui";
import { ChevronRight } from "./ui/icons";
import { useActivity, useMarkerSeen, usePreviews } from "./useActivity";
import { discoveryStore, useDiscovery } from "./useDiscovery";
import { useReconnect } from "./useReconnect";
import { sessionStore, useSessions } from "./useSessions";

export const APP_NAME = "Remora";

/** Re-fetch the PATH-detected terminal list (Task 9's externalTerminalLabel
 * input). Module-scope so effects can call it without listing a per-render
 * closure as a dependency. */
function refreshDetectedTerminals(
  setDetectedTerminals: (terminals: DetectedTerminalDto[]) => void,
) {
  void commands
    .externalTerminals()
    .then((r) => {
      if (r.status === "ok") setDetectedTerminals(r.data);
    })
    // Module-scope helper: no notice setter in scope here, and this is a
    // background refresh (same guard-anyway rationale as the
    // subscribeConfigChanged listen() failure below) — a failed IPC call
    // should not become an unhandled rejection, but it's not worth
    // interrupting the user over either.
    .catch(() => {});
}

/** Single source of truth for the left sidebar's persisted width bounds. The
 * hook clamps to [min, effectiveMax(max)] and the ResizeHandle advertises the
 * same bounds — keep both reading from here so they can't drift. */
const SIDEBAR_RAIL = {
  key: "remora.rail.sidebar",
  defaultWidth: 240,
  min: 180,
  max: 480,
} as const;

/** Root component: wires the discovery and session stores to the sidebar, tab
 * bar, terminal panes, the diff peek panel, and the new-session dialog. */
function App() {
  const {
    tabs,
    activeKey,
    connecting,
    removing,
    openSession,
    openViaRespawn,
    openViaAttach,
    closeTab,
    focusTab,
    reorderTab,
    respawnTab,
    stopSession,
    removeSession,
  } = useSessions();
  useReconnect(sessionStore);
  const activity = useActivity();
  const previews = usePreviews();
  const markerSeen = useMarkerSeen();
  const {
    config,
    sessions,
    configError,
    discoveryUnavailable,
    reconnectingKeys,
    refresh,
  } = useDiscovery();

  // Terminals detected on PATH for the "Open in <Name>" menu label/action
  // (Task 9's externalTerminalLabel). Not part of the discovery store's poll
  // loop — fetched once on mount and again whenever config changes, since
  // installing/configuring a terminal doesn't change session state.
  const [detectedTerminals, setDetectedTerminals] = useState<
    DetectedTerminalDto[]
  >([]);
  useEffect(() => {
    refreshDetectedTerminals(setDetectedTerminals);
  }, []);

  // Live-reload the sidebar when the config file changes on disk (backend
  // watcher emits ConfigChanged). Mirrors the manual refresh button.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    subscribeConfigChanged(() => {
      void refresh();
      refreshDetectedTerminals(setDetectedTerminals);
    })
      .then((fn) => {
        // If the effect already cleaned up before listen() resolved, unlisten
        // immediately; otherwise hand the cleanup the handle.
        if (cancelled) fn();
        else unlisten = fn;
      })
      // A failed listen() leaves manual refresh working; never throw an
      // unhandled rejection that would surface as a console error.
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [refresh]);
  const [dialogOpen, setDialogOpen] = useState(false);
  // Project the New Session dialog is pre-scoped to (per-project sidebar "+"),
  // or null for the global "+ New session" entry point.
  const [dialogProjectId, setDialogProjectId] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // Body the Settings dialog opens on: the list (footer gear) or a deep-linked
  // form (the sidebar's new-project "+").
  const [settingsView, setSettingsView] = useState<SettingsView>({
    kind: "list",
  });
  const [notice, setNotice] = useState<string | null>(null);
  // Files & diff peek panel (⌘\). Closed by default — the terminal is the hero;
  // the panel is an intentional reveal (and its data surface is empty for now).
  const [panelOpen, setPanelOpen] = useState(false);
  // Desktop→mobile single-pane fold: which pane the narrow layout shows.
  const [mobilePane, setMobilePane] = useState<"list" | "session">("session");
  const [removeTarget, setRemoveTarget] = useState<{
    projectId: string;
    sessionId: string;
    workspace: WorkspaceModeDto | null;
    /** Set when re-opened at the force stage after a background dirty result. */
    forceReason?: DirtyReasonDto;
  } | null>(null);
  // Render-time mirror of removeTarget for the backgrounded remove's settle
  // callback, whose closure captured the value from a long-gone render.
  const removeTargetRef = useRef(removeTarget);
  removeTargetRef.current = removeTarget;
  const newButtonRef = useRef<HTMLButtonElement>(null);
  // Live focus handles for the mounted terminal panes, keyed by tab key.
  const terminals = useRef(new Map<string, TerminalHandle>());
  // Intent flag: set by tab/sidebar selection so the activeKey effect knows the
  // change was a user pick (focus its terminal) versus a dialog spawn or
  // background reconnect (leave focus alone). Consumed once per selection.
  const focusOnSelect = useRef(false);
  // Bumped to re-run the focus effect when intent is armed *after* activeKey has
  // already settled. The tab/sidebar paths arm focusOnSelect before they flip
  // activeKey, so the activeKey change itself re-runs the effect. The dialog
  // spawn path can't: it only learns it opened a fresh terminal once openSession
  // resolves (handleOpened), by which point activeKey/activeStatus have stopped
  // changing — so without this nudge the effect would never re-fire to consume
  // the flag, and a new session's terminal would never take focus (#126).
  const [focusRequest, setFocusRequest] = useState(0);

  // Sidebar resize + collapse state, persisted per-device in localStorage.
  const sidebarRef = useRef<HTMLDivElement>(null);
  const isMobile = useIsMobile();
  const { width, collapsed, setWidth, commitWidth, toggleCollapsed, reset } =
    useRailWidth(SIDEBAR_RAIL);
  const prevCollapsedRef = useRef(collapsed);
  const [resizing, setResizing] = useState(false);
  // On mobile the layout owns full-width; collapsed rail only renders off-mobile.
  const showCollapsed = shouldRenderCollapsed(collapsed, isMobile);

  // Recompute the tree only when config or the polled session list changes.
  const reconnectingSet = useMemo(
    () => new Set(reconnectingKeys),
    [reconnectingKeys],
  );
  const tree = useMemo(
    () => buildTree(config, sessions, reconnectingSet),
    [config, sessions, reconnectingSet],
  );
  // Which sessions are open as tabs — the only ones we have a live terminal for
  // and can therefore report an activity status. Drives the sidebar's "show the
  // status dot only when connected" rule.
  const openKeys = useMemo(() => new Set(tabs.map((t) => t.key)), [tabs]);
  // Sessions whose open is in flight — the sidebar spins their rows until the
  // open resolves to a live tab, fails, or is cancelled (#170).
  const connectingKeys = useMemo(() => new Set(connecting), [connecting]);
  // Sessions with a background remove in flight — their sidebar row spins
  // "Removing…" until the teardown settles and the row vanishes or recovers.
  const removingKeys = useMemo(() => new Set(removing), [removing]);

  // Status of the active tab, so the focus effect re-fires when a freshly opened
  // or respawned session goes live (a stopped/reconnecting tab renders a
  // placeholder or a not-yet-ready terminal until it does).
  const activeTab = tabs.find((t) => t.key === activeKey) ?? null;
  const activeStatus = activeTab?.status ?? null;

  // ⌘\ / Ctrl+\ toggles the files & diff peek panel.
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "\\" && (e.metaKey || e.ctrlKey)) {
        // Don't hijack Ctrl+\ from a focused terminal — it's SIGQUIT to the
        // agent. Only toggle when focus is outside the terminal hero.
        if ((e.target as HTMLElement | null)?.closest(".rk-term")) return;
        e.preventDefault();
        setPanelOpen((open) => !open);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Focus the now-active terminal once it's live and its pane has mounted.
  // Gated by focusOnSelect so only an explicit tab/sidebar selection grabs focus
  // — the dialog spawn path keeps focus on newButtonRef. The intent flag is held
  // (not consumed) until the terminal exists, so opening a new session or
  // reopening a stopped one — which mount the terminal a tick or two after
  // activeKey flips — still lands focus. Focus is deferred to the next frame so a
  // just-opened xterm has its input ready to accept it. focusRequest lets the
  // dialog-spawn path re-trigger this effect after it arms the flag late.
  // biome-ignore lint/correctness/useExhaustiveDependencies: focusRequest is a re-arm nudge, not read in the body
  useEffect(() => {
    if (!focusOnSelect.current) return;
    if (activeKey === null) return;
    // A revive we armed for (reconnect/respawn) terminally failed: the active
    // tab settled into a non-live, non-recovering state, so the "wait for live"
    // guard below would never consume the flag. Clear it here so it can't steal
    // focus on a later activeKey change — e.g. closing this tab refocuses a live
    // neighbour (#189; also closes the latent respawn-path steal).
    //
    // Load-bearing invariant: openViaAttach/openViaRespawn flip a revived tab to
    // "reconnecting" synchronously (reconnectTab/respawnTab setStatus before any
    // await), in the same batched click handler as the activeKey commit. React
    // coalesces both store notifications into one render that reads the final
    // "reconnecting" status, so this branch never observes the pre-revive
    // stopped/disconnected as a transient — only as a terminal revive failure.
    // If an opener ever yields between the activeKey commit and the status flip,
    // that transient would leak here and prematurely disarm the arm (see #189).
    if (activeStatus === "stopped" || activeStatus === "disconnected") {
      focusOnSelect.current = false;
      return;
    }
    if (activeStatus !== "live") return; // reconnecting: stay armed, wait for live
    const handle = terminals.current.get(activeKey);
    if (!handle) return; // terminal not mounted yet; stay armed for when it is
    focusOnSelect.current = false;
    const raf = requestAnimationFrame(() => handle.focus());
    return () => cancelAnimationFrame(raf);
  }, [activeKey, activeStatus, focusRequest]);

  // After the collapsed/expanded swap, move focus to the counterpart toggle so
  // keyboard users aren't dropped to <body>. :focus-visible keeps mouse users
  // ring-free. Keyed on both collapsed and showCollapsed so it runs when the
  // view changes, but skips mount and isMobile-only changes (viewport resize).
  useEffect(() => {
    if (prevCollapsedRef.current === collapsed) return; // isMobile-only change or mount → skip
    prevCollapsedRef.current = collapsed;
    const root = sidebarRef.current;
    if (!root) return;
    const label = showCollapsed ? SIDEBAR_EXPAND_LABEL : SIDEBAR_COLLAPSE_LABEL;
    root
      .querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`)
      ?.focus();
  }, [collapsed, showCollapsed]);

  /** Open a session clicked in the sidebar, routing by its discovered state:
   * live → attach/focus, stopped → respawn. Reuses the dialog's deduping path
   * (an already-open session just focuses). No discovery refresh here: attaching
   * an existing session changes no server state (Codex #9); only the spawn path
   * (handleOpened) refreshes. */
  function openFromSidebar(node: SessionNode) {
    setNotice(null);
    setMobilePane("session");
    focusOnSelect.current = true;
    const key = tabKey(node.projectId, node.sessionId);
    // Sample local status fresh from the store — a status flip can land between
    // render and this click — BEFORE any opener runs. The openers flip status
    // synchronously (reconnectTab/respawnTab setStatus "reconnecting"), so a
    // post-call read would misjudge a tab that is legitimately reviving.
    const snap = sessionStore.getSnapshot();
    const preStatus = snap.tabs.find((t) => t.key === key)?.status ?? null;

    // Re-clicking the active, locally-live tab: no opener would change activeKey/
    // status, so the activeKey-gated focus effect never fires to consume
    // focusOnSelect. Focus the terminal directly and disarm. A non-live active
    // tab falls through to a revive path instead (#189 Bug A).
    if (shouldFocusActiveTabInPlace(snap.activeKey, key, preStatus)) {
      focusOnSelect.current = false;
      terminals.current.get(key)?.focus();
      return;
    }

    if (node.state === "stopped") {
      // Discovery reports this session stopped (server reaped it). openViaRespawn
      // respawns a non-live existing tab in place, or spawns a fresh one — a
      // controlled path to live the focus effect will consume.
      void openViaRespawn({
        projectId: node.projectId,
        sessionId: node.sessionId,
        agent: node.agent,
        base: null,
        workspace: node.workspace ?? "worktree",
      })
        .then((r) => {
          if (shouldDisarmAfterSidebarOpen(r, preStatus)) {
            focusOnSelect.current = false;
          }
          if (!r.ok && r.error !== OPEN_CANCELLED) {
            setNotice("Could not respawn the session.");
          }
        })
        .catch(() => {
          focusOnSelect.current = false;
          setNotice("Could not respawn the session.");
        });
      return;
    }

    // Live-attach path: attach to the live tmux (never spawn-first, which would
    // leak a duplicate worktree). openViaAttach resolves (never rejects) with
    // {ok:false} on failure, falls back to respawn internally if the session died
    // between poll and click, and now reconnects a non-live existing tab in place
    // (#189 Bug A).
    openViaAttach({
      projectId: node.projectId,
      sessionId: node.sessionId,
      agent: node.agent,
      base: null,
      workspace: node.workspace ?? "worktree",
    })
      .then((result) => {
        if (shouldDisarmAfterSidebarOpen(result, preStatus)) {
          focusOnSelect.current = false;
        }
        if (!result.ok && result.error !== OPEN_CANCELLED) {
          setNotice("Could not open the session. It may have stopped.");
        }
      })
      .catch(() => {
        focusOnSelect.current = false;
        setNotice("Could not open the session. It may have stopped.");
      });
  }

  // No teardown on React unmount: the store is a process-scoped singleton, and
  // a StrictMode/HMR remount must NOT dispose it (decision 1). Process/window
  // exit closes the OS-level PTY + bridge channels; a future window-close hook
  // can call sessionStore.dispose() if explicit teardown is ever needed.

  /** Stop a live worktree session (kills tmux, keeps the worktree). */
  function onStop(node: SessionNode) {
    setNotice(null);
    void stopSession(node.projectId, node.sessionId)
      .then((r) => {
        if (r.ok) {
          void discoveryStore.refreshAfterOpen();
        } else if (r.error) {
          // A bare {ok:false} with no error is the in-flight guard (double-click)
          // or a disposed store — neither is a real failure, so stay quiet.
          setNotice("Could not stop the session.");
        }
      })
      // The store action resolves rather than rejects, but guard anyway so an
      // unexpected throw can't become an unhandled rejection (matches the
      // .then().catch() pattern openFromSidebar uses).
      .catch(() => setNotice("Could not stop the session."));
  }

  /** Launch the configured external terminal attached to this session. */
  function onOpenExternal(node: SessionNode) {
    setNotice(null);
    void runOpenExternal(
      {
        open: (p, s, t) => commands.openExternalTerminal(p, s, t),
        onNotConfigured: () => {
          // No terminal configured (or ambiguous): the Settings list view
          // hosts the External terminal row (initialView pattern, #161).
          setSettingsView({ kind: "list" });
          setSettingsOpen(true);
        },
        onError: (message) =>
          setNotice(`Could not open the terminal: ${message}`),
      },
      node.projectId,
      node.sessionId,
    )
      // The runner routes failures through onError, but guard anyway so an
      // unexpected IPC throw can't become an unhandled rejection (see onStop).
      .catch(() => setNotice("Could not open the terminal."));
  }

  /** Put the exact attach command on the clipboard (universal fallback). */
  function onCopyAttach(node: SessionNode) {
    setNotice(null);
    void runCopyAttach(
      {
        copy: (p, s) => commands.copyAttachCommand(p, s),
        onError: (message) =>
          setNotice(`Could not copy the command: ${message}`),
      },
      node.projectId,
      node.sessionId,
    )
      // Same guard-anyway rationale as onStop/onOpenExternal.
      .catch(() => setNotice("Could not copy the command."));
  }

  /** Open this SSH session's remote workspace in local VS Code (Remote-SSH). */
  function onOpenVscode(node: SessionNode) {
    setNotice(null);
    void runOpenInVscode(
      {
        open: (p, s) => commands.openInVscode(p, s),
        onError: (message) => setNotice(`Could not open VS Code: ${message}`),
      },
      node.projectId,
      node.sessionId,
    ).catch(() => setNotice("Could not open VS Code."));
  }

  /** Open the remove confirm dialog for any session. A session whose removal
   * is already running in the background gets no dialog — confirming it would
   * only hit the store's busy-guard, and the row's spinner already says the
   * removal is underway. */
  function onRemove(node: SessionNode) {
    setNotice(null);
    const key = tabKey(node.projectId, node.sessionId);
    if (removingKeys.has(key)) return;
    setRemoveTarget({
      projectId: node.projectId,
      sessionId: node.sessionId,
      workspace: node.workspace,
    });
  }

  /** Open the remove confirm dialog from a tab (stopped/disconnected pane). */
  function onRemoveTab(projectId: string, sessionId: string) {
    setNotice(null);
    if (removingKeys.has(tabKey(projectId, sessionId))) return;
    // Find the workspace from the tree if available.
    const node = tree
      .flatMap((p) => p.sessions)
      .find((s) => s.projectId === projectId && s.sessionId === sessionId);
    setRemoveTarget({
      projectId,
      sessionId,
      workspace: node?.workspace ?? null,
    });
  }

  /** Confirm callback for the remove dialog. Removal is backgrounded: the tab
   * and dialog close immediately (the user moves on to other sessions) and the
   * settled result is routed async — success needs nothing (the row vanishes
   * on the discovery refresh), WorkspaceDirty re-opens the dialog at the force
   * stage, and a real failure lands in the notice bar. The sidebar row spins
   * via snapshot.removing for the whole teardown. */
  function handleRemoveConfirm(force: boolean) {
    if (!removeTarget) return;
    const target = removeTarget;
    setRemoveTarget(null);
    const key = tabKey(target.projectId, target.sessionId);
    const promise = removeSession(target.projectId, target.sessionId, force);
    // Optimistic close: intent to remove was just confirmed, and mid-teardown
    // the terminal would only die visibly. On failure the sidebar row remains
    // and the session can be reopened. Gated on the store having ACCEPTED the
    // remove — remove() publishes snapshot.removing synchronously before its
    // first await, so a busy-guard refusal (a stop/respawn/open already in
    // flight for this key) is visible here. Closing unconditionally would
    // destroy the tab for a remove that never ran.
    if (sessionStore.getSnapshot().removing.includes(key)) {
      closeTab(key);
    }
    void promise
      .then((result) => {
        // Even a failed remove may have torn down partial server state (e.g.
        // tmux killed, worktree removal failed), so re-list on every outcome.
        void discoveryStore.refreshAfterOpen();
        const followUp = routeRemoveResult(result, force);
        if (followUp.kind === "confirm-force") {
          // If the user has meanwhile opened a remove dialog for a DIFFERENT
          // session, don't clobber it with this session's force re-prompt —
          // park the dirty result in the notice bar instead, so neither the
          // open dialog nor this result is silently lost. Removing the dirty
          // session again re-prompts. (The ref, not the closure, carries the
          // current dialog state — this callback's render is long gone.)
          const open = removeTargetRef.current;
          if (open && tabKey(open.projectId, open.sessionId) !== key) {
            setNotice(
              `${target.projectId}/${target.sessionId} has uncommitted work — choose Remove again to confirm.`,
            );
          } else {
            setRemoveTarget({ ...target, forceReason: followUp.reason });
          }
        } else if (followUp.kind === "error") {
          setNotice(
            `Could not remove ${target.projectId}/${target.sessionId}: ${followUp.message}`,
          );
        }
      })
      // The store action resolves rather than rejects, but guard anyway so an
      // unexpected throw can't become an unhandled rejection.
      .catch(() => {
        setNotice(`Could not remove ${target.projectId}/${target.sessionId}.`);
      });
  }

  /** Dialog success callback: close it, note an attach-vs-spawn outcome, route
   * focus, and re-list sessions now (a fresh spawn changed server state). */
  function handleOpened({
    attached,
    opened,
  }: {
    attached: boolean;
    opened: boolean;
  }) {
    setDialogOpen(false);
    setNotice(attached ? "Attached to an existing session." : null);
    setMobilePane("session");
    if (attached || !opened) {
      // No fresh terminal was opened — either we attached to an already-running
      // session, or the dialog deduped to an already-open tab (live or not).
      // Keep focus on the + button and do NOT arm focusOnSelect: the dialog
      // opened nothing new to type into, and arming for a non-live tab would
      // leave the flag set, stealing focus when the pane later goes live (#133).
      // Matches the cancel/fail paths. (Re-submitting the dialog for an already-
      // open live session thus keeps button focus, by design — the sidebar, not
      // the dialog, is the path for focusing an existing session.)
      newButtonRef.current?.focus();
    } else {
      // Fresh spawn: route into the same focus path a tab/sidebar pick uses so
      // the new terminal grabs focus once it's live — typing the first prompt
      // is the most common next action (#78). openSession already flipped
      // activeKey to the new tab, so arming the intent flag (a ref) alone won't
      // re-run the activeKey-gated focus effect; bumping focusRequest does. The
      // dialog's unmount restores focus to the + button during commit, but the
      // effect re-focuses the terminal via a deferred rAF that runs after that
      // commit — so the terminal wins. Cancel/fail never reach here (the dialog
      // only calls back on success), so they keep button focus.
      focusOnSelect.current = true;
      setFocusRequest((n) => n + 1);
    }
    // A fresh spawn changed server state, so re-list now rather than waiting a
    // poll tick; an attach didn't, but a redundant list is cheap and keeps the
    // call site simple.
    void discoveryStore.refreshAfterOpen();
  }

  /** Open the Settings dialog on its list view — shared by the full sidebar and
   * the collapsed rail. Resets the deep-linked view so the footer gear always
   * lands on the list, not a stale new-project form. */
  function openSettings() {
    setNotice(null);
    setSettingsView({ kind: "list" });
    setSettingsOpen(true);
  }

  /** Open Settings deep-linked to the new-project form (the Projects header "+", #161). */
  function openAddProject() {
    setNotice(null);
    setSettingsView({ kind: "project", mode: "create" });
    setSettingsOpen(true);
  }

  /** Open the New Session dialog. `projectId` pre-scopes it to a project (the
   * sidebar per-project "+"); null is the global, no-context entry point. */
  function openNewSession(projectId: string | null) {
    setNotice(null);
    // Drop any unconsumed selection intent so spawning from the dialog doesn't
    // later yank focus off the opener onto the new terminal.
    focusOnSelect.current = false;
    setDialogProjectId(projectId);
    setDialogOpen(true);
  }

  // Menu label for the primary "Open in <Name>" action (Task 9's
  // externalTerminalLabel): the configured registry terminal's display name,
  // the sole PATH-detected terminal when nothing is configured, or a generic
  // fallback.
  const externalLabel = externalTerminalLabel(
    config?.terminal ?? null,
    detectedTerminals,
  );

  return (
    <main className={`rk-app rk-app--${mobilePane}`}>
      <div
        ref={sidebarRef}
        className={[
          "rk-app__sidebar",
          showCollapsed ? "rk-app__sidebar--collapsed" : "",
          resizing ? "rk-app__sidebar--resizing" : "",
        ]
          .filter(Boolean)
          .join(" ")}
        style={
          isMobile ? undefined : { width: showCollapsed ? undefined : width }
        }
      >
        {showCollapsed ? (
          <CollapsedRail
            tree={tree}
            activeKey={activeKey}
            openKeys={openKeys}
            connectingKeys={connectingKeys}
            activity={activity}
            onOpenSession={openFromSidebar}
            onExpand={toggleCollapsed}
            onOpenSettings={openSettings}
          />
        ) : (
          <Sidebar
            tree={tree}
            activeKey={activeKey}
            openKeys={openKeys}
            connectingKeys={connectingKeys}
            removingKeys={removingKeys}
            onOpenSession={openFromSidebar}
            configError={configError}
            discoveryUnavailable={discoveryUnavailable}
            onRefresh={() => void refresh()}
            externalLabel={externalLabel}
            onOpenExternal={onOpenExternal}
            onCopyAttach={onCopyAttach}
            onOpenVscode={onOpenVscode}
            onStop={onStop}
            onRemove={onRemove}
            onNewSession={openNewSession}
            onOpenSettings={openSettings}
            onAddProject={openAddProject}
            activity={activity}
            previews={previews}
            markerSeen={markerSeen}
            onCollapse={isMobile ? undefined : toggleCollapsed}
          />
        )}
      </div>
      {!showCollapsed && !isMobile && (
        <ResizeHandle
          edge="right"
          railRef={sidebarRef}
          min={SIDEBAR_RAIL.min}
          max={effectiveMax(SIDEBAR_RAIL.max)}
          ariaLabel="Resize sidebar"
          value={width}
          onResize={setWidth}
          onCommit={commitWidth}
          onReset={reset}
          onResizingChange={setResizing}
        />
      )}
      <div className="rk-app__main">
        <div className="rk-mobilebar">
          <IconButton
            label="Back to sessions"
            size="sm"
            onClick={() => setMobilePane("list")}
          >
            <ChevronRight size={16} style={{ transform: "rotate(180deg)" }} />
          </IconButton>
          <span className="rk-mobilebar__title">
            {activeTab ? activeTab.sessionId : APP_NAME}
          </span>
        </div>
        <TabBar
          tabs={tabs}
          activeKey={activeKey}
          activity={activity}
          panelOpen={panelOpen}
          onTogglePanel={() => setPanelOpen((open) => !open)}
          onFocus={(key) => {
            setNotice(null);
            setMobilePane("session");
            // Re-selecting the active tab leaves activeKey unchanged, so the
            // effect won't fire — focus its (already-visible) terminal directly
            // and leave the intent flag disarmed.
            if (key === activeKey) {
              focusOnSelect.current = false;
              terminals.current.get(key)?.focus();
              return;
            }
            focusOnSelect.current = true;
            focusTab(key);
          }}
          onClose={(key) => {
            setNotice(null);
            closeTab(key);
          }}
          onReorder={reorderTab}
          onNew={() => openNewSession(null)}
          newButtonRef={newButtonRef}
        />
        {notice && (
          <div className="rk-notice" role="status">
            {notice}
          </div>
        )}
        <div className="rk-session-bar">
          {activeTab ? (
            <>
              <span className="rk-session-bar__id">{activeTab.sessionId}</span>
              {activeTab.agent && <Tag>{activeTab.agent}</Tag>}
            </>
          ) : (
            <span className="rk-session-bar__empty">No active session</span>
          )}
          <span className="rk-session-bar__spacer" />
        </div>
        <div className="rk-panes">
          {tabs.length === 0 ? (
            <div className="rk-term">
              <div className="rk-term__boot">
                <div className="rk-term__boot-txt">
                  <div className="rk-term__boot-title">No sessions open</div>
                  <div className="rk-term__boot-sub">
                    Pick a session from the sidebar, or start a new one.
                  </div>
                </div>
              </div>
            </div>
          ) : (
            tabs.map((t) => (
              <div
                key={t.key}
                className="rk-term"
                style={t.key === activeKey ? undefined : { display: "none" }}
              >
                {t.status === "stopped" ? (
                  <div className="rk-pane-status" role="status">
                    <p>Session stopped{t.error ? `: ${t.error}` : "."}</p>
                    <div className="rk-pane-status__actions">
                      {canRespawn(t.workspace) && (
                        <Button
                          variant="secondary"
                          size="sm"
                          onClick={() => void respawnTab(t.key)}
                        >
                          Respawn
                        </Button>
                      )}
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => onRemoveTab(t.projectId, t.sessionId)}
                      >
                        Remove…
                      </Button>
                    </div>
                  </div>
                ) : t.status === "disconnected" ? (
                  <div
                    className="rk-pane-status rk-pane-status--error"
                    role="alert"
                  >
                    <p>Disconnected: {t.error}</p>
                    <div className="rk-pane-status__actions">
                      {canRespawn(t.workspace) && (
                        <Button
                          variant="secondary"
                          size="sm"
                          onClick={() => void respawnTab(t.key)}
                        >
                          Respawn
                        </Button>
                      )}
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => onRemoveTab(t.projectId, t.sessionId)}
                      >
                        Remove…
                      </Button>
                    </div>
                  </div>
                ) : (
                  <Terminal
                    connection={t.connection}
                    sessionKey={t.key}
                    ref={(h) => {
                      if (h) terminals.current.set(t.key, h);
                      else terminals.current.delete(t.key);
                    }}
                  />
                )}
              </div>
            ))
          )}
        </div>
      </div>
      {panelOpen && (
        <div className="rk-app__panel">
          <DiffPanel onClose={() => setPanelOpen(false)} />
        </div>
      )}
      {dialogOpen && (
        <NewSessionDialog
          config={config}
          initialProjectId={dialogProjectId ?? undefined}
          openSession={openSession}
          onOpened={handleOpened}
          onClose={() => {
            setDialogOpen(false);
            newButtonRef.current?.focus();
          }}
        />
      )}
      {removeTarget && (
        <ConfirmRemoveDialog
          // Remount (fresh focus + state) if the dialog ever retargets to a
          // different session while open, rather than morphing in place.
          key={tabKey(removeTarget.projectId, removeTarget.sessionId)}
          projectId={removeTarget.projectId}
          sessionId={removeTarget.sessionId}
          workspace={removeTarget.workspace}
          forceReason={removeTarget.forceReason ?? null}
          onConfirm={handleRemoveConfirm}
          onClose={() => setRemoveTarget(null)}
        />
      )}
      {settingsOpen && (
        <SettingsDialog
          // A config edit changes the sidebar's (redacted) view; re-read it.
          onConfigChanged={() => void refresh()}
          onClose={() => setSettingsOpen(false)}
          initialView={settingsView}
        />
      )}
    </main>
  );
}

export default App;
