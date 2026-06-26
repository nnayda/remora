import { useEffect, useMemo, useRef, useState } from "react";
import type { WorkspaceModeDto } from "./bindings";
import { ConfirmRemoveDialog } from "./ConfirmRemoveDialog";
import { DiffPanel } from "./DiffPanel";
import { NewSessionDialog } from "./NewSessionDialog";
import { SettingsDialog } from "./SettingsDialog";
import { Sidebar } from "./Sidebar";
import { canRespawn, OPEN_CANCELLED } from "./session-store";
import { buildTree, type SessionNode } from "./session-tree";
import { TabBar } from "./TabBar";
import { Terminal, type TerminalHandle } from "./Terminal";
import { Button, IconButton, Tag } from "./ui";
import { ChevronRight } from "./ui/icons";
import { activityStore, useActivity } from "./useActivity";
import { discoveryStore, useDiscovery } from "./useDiscovery";
import { useReconnect } from "./useReconnect";
import { sessionStore, useSessions } from "./useSessions";

export const APP_NAME = "Remora";

/** Root component: wires the discovery and session stores to the sidebar, tab
 * bar, terminal panes, the diff peek panel, and the new-session dialog. */
function App() {
  const {
    tabs,
    activeKey,
    openSession,
    openViaRespawn,
    closeTab,
    focusTab,
    respawnTab,
    stopSession,
    removeSession,
  } = useSessions();
  useReconnect(sessionStore);
  const activity = useActivity();
  useEffect(() => {
    activityStore.start();
    return () => activityStore.stop();
  }, []);
  const { config, sessions, configError, discoveryUnavailable, refresh } =
    useDiscovery();
  const [dialogOpen, setDialogOpen] = useState(false);
  // Project the New Session dialog is pre-scoped to (per-project sidebar "+"),
  // or null for the global "+ New session" entry point.
  const [dialogProjectId, setDialogProjectId] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
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
  } | null>(null);
  const newButtonRef = useRef<HTMLButtonElement>(null);
  // Live focus handles for the mounted terminal panes, keyed by tab key.
  const terminals = useRef(new Map<string, TerminalHandle>());
  // Intent flag: set by tab/sidebar selection so the activeKey effect knows the
  // change was a user pick (focus its terminal) versus a dialog spawn or
  // background reconnect (leave focus alone). Consumed once per selection.
  const focusOnSelect = useRef(false);

  // Recompute the tree only when config or the polled session list changes.
  const tree = useMemo(() => buildTree(config, sessions), [config, sessions]);

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
  // just-opened xterm has its input ready to accept it.
  useEffect(() => {
    if (!focusOnSelect.current) return;
    if (activeKey === null || activeStatus !== "live") return; // wait for live
    const handle = terminals.current.get(activeKey);
    if (!handle) return; // terminal not mounted yet; stay armed for when it is
    focusOnSelect.current = false;
    const raf = requestAnimationFrame(() => handle.focus());
    return () => cancelAnimationFrame(raf);
  }, [activeKey, activeStatus]);

  /** Open a session clicked in the sidebar, routing by its discovered state:
   * live → attach/focus, stopped → respawn. Reuses the dialog's deduping path
   * (an already-open session just focuses). No discovery refresh here: attaching
   * an existing session changes no server state (Codex #9); only the spawn path
   * (handleOpened) refreshes. */
  function openFromSidebar(node: SessionNode) {
    setNotice(null);
    setMobilePane("session");
    focusOnSelect.current = true;
    if (node.state === "stopped") {
      void openViaRespawn({
        projectId: node.projectId,
        sessionId: node.sessionId,
        agent: node.agent,
        base: null,
        workspace: node.workspace ?? "worktree",
      })
        .then((r) => {
          if (!r.ok && r.error !== OPEN_CANCELLED) {
            // A failed open never changes activeKey, so the intent flag would
            // stay armed and steal focus on the next unrelated change. Disarm.
            focusOnSelect.current = false;
            setNotice("Could not respawn the session.");
          }
        })
        .catch(() => {
          focusOnSelect.current = false;
          setNotice("Could not respawn the session.");
        });
      return;
    }
    // openSession resolves (never rejects) with {ok:false} on failure — e.g. a
    // session that died between the poll and the click. Surface that instead of
    // dropping it silently; the .catch is a belt-and-braces guard.
    openSession({
      projectId: node.projectId,
      sessionId: node.sessionId,
      agent: node.agent,
      base: null,
      workspace: node.workspace ?? "worktree",
    })
      .then((result) => {
        if (!result.ok && result.error !== OPEN_CANCELLED) {
          // See the respawn path: disarm so a no-op open can't leak focus.
          focusOnSelect.current = false;
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

  /** Open the remove confirm dialog for any session. */
  function onRemove(node: SessionNode) {
    setNotice(null);
    setRemoveTarget({
      projectId: node.projectId,
      sessionId: node.sessionId,
      workspace: node.workspace,
    });
  }

  /** Open the remove confirm dialog from a tab (stopped/disconnected pane). */
  function onRemoveTab(projectId: string, sessionId: string) {
    setNotice(null);
    // Find the workspace from the tree if available.
    const node = tree
      .flatMap((h) => h.projects)
      .flatMap((p) => p.sessions)
      .find((s) => s.projectId === projectId && s.sessionId === sessionId);
    setRemoveTarget({
      projectId,
      sessionId,
      workspace: node?.workspace ?? null,
    });
  }

  /** Dialog success callback: close it, note an attach-vs-spawn outcome, route
   * focus, and re-list sessions now (a fresh spawn changed server state). */
  function handleOpened(attached: boolean) {
    setDialogOpen(false);
    setNotice(attached ? "Attached to an existing session." : null);
    setMobilePane("session");
    if (attached) {
      // Attaching an already-running session opens no fresh terminal to type
      // into, so keep focus on the + button (matching the cancel/fail paths).
      newButtonRef.current?.focus();
    } else {
      // Fresh spawn: route into the same focus path a tab/sidebar pick uses so
      // the new terminal grabs focus once it's live — typing the first prompt
      // is the most common next action (#78). openSession already flipped
      // activeKey to the new tab; arming the intent flag (a ref, set before any
      // passive effect runs) lets the focus effect claim it post-paint. The
      // dialog's unmount restores focus to the + button during commit, but the
      // effect re-focuses the terminal via a deferred rAF that runs after that
      // commit — so the terminal wins. Cancel/fail never reach here (the dialog
      // only calls back on success), so they keep button focus.
      focusOnSelect.current = true;
    }
    // A fresh spawn changed server state, so re-list now rather than waiting a
    // poll tick; an attach didn't, but a redundant list is cheap and keeps the
    // call site simple.
    void discoveryStore.refreshAfterOpen();
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

  return (
    <main className={`rk-app rk-app--${mobilePane}`}>
      <div className="rk-app__sidebar">
        <Sidebar
          tree={tree}
          activeKey={activeKey}
          onOpenSession={openFromSidebar}
          configError={configError}
          discoveryUnavailable={discoveryUnavailable}
          onRefresh={() => void refresh()}
          onStop={onStop}
          onRemove={onRemove}
          onNewSession={openNewSession}
          onOpenSettings={() => {
            setNotice(null);
            setSettingsOpen(true);
          }}
          activity={activity}
        />
      </div>
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
          projectId={removeTarget.projectId}
          sessionId={removeTarget.sessionId}
          workspace={removeTarget.workspace}
          onConfirm={(force) =>
            removeSession(removeTarget.projectId, removeTarget.sessionId, force)
          }
          onClose={() => {
            setRemoveTarget(null);
            void discoveryStore.refreshAfterOpen();
          }}
        />
      )}
      {settingsOpen && (
        <SettingsDialog
          // A config edit changes the sidebar's (redacted) view; re-read it.
          onConfigChanged={() => void refresh()}
          onClose={() => setSettingsOpen(false)}
        />
      )}
    </main>
  );
}

export default App;
