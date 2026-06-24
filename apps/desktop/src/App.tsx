import { useEffect, useMemo, useRef, useState } from "react";
import "./App.css";
import type { WorkspaceModeDto } from "./bindings";
import { ConfirmRemoveDialog } from "./ConfirmRemoveDialog";
import { NewSessionDialog } from "./NewSessionDialog";
import { SettingsDialog } from "./SettingsDialog";
import { Sidebar } from "./Sidebar";
import { OPEN_CANCELLED } from "./session-store";
import { buildTree, type SessionNode } from "./session-tree";
import { TabBar } from "./TabBar";
import { Terminal, type TerminalHandle } from "./Terminal";
import { discoveryStore, useDiscovery } from "./useDiscovery";
import { useReconnect } from "./useReconnect";
import { sessionStore, useSessions } from "./useSessions";

export const APP_NAME = "Remora";

/** Root component: wires the discovery and session stores to the sidebar, tab
 * bar, terminal panes, and the new-session dialog. */
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
  const { config, sessions, configError, discoveryUnavailable, refresh } =
    useDiscovery();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
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
  const openKeys = useMemo(() => new Set(tabs.map((t) => t.key)), [tabs]);

  // Status of the active tab, so the focus effect re-fires when a freshly opened
  // or respawned session goes live (a stopped/reconnecting tab renders a
  // placeholder or a not-yet-ready terminal until it does).
  const activeStatus = tabs.find((t) => t.key === activeKey)?.status ?? null;

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
    focusOnSelect.current = true;
    if (node.state === "stopped") {
      void openViaRespawn({
        projectId: node.projectId,
        sessionId: node.sessionId,
        agent: node.agent,
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

  /** Dialog success callback: close it, note an attach-vs-spawn outcome, restore
   * focus, and re-list sessions now (a fresh spawn changed server state). */
  function handleOpened(attached: boolean) {
    setDialogOpen(false);
    setNotice(attached ? "Attached to an existing session." : null);
    newButtonRef.current?.focus();
    // A fresh spawn changed server state, so re-list now rather than waiting a
    // poll tick; an attach didn't, but a redundant list is cheap and keeps the
    // call site simple.
    void discoveryStore.refreshAfterOpen();
  }

  return (
    <main className="app">
      <Sidebar
        tree={tree}
        activeKey={activeKey}
        openKeys={openKeys}
        onOpenSession={openFromSidebar}
        configError={configError}
        discoveryUnavailable={discoveryUnavailable}
        onRefresh={() => void refresh()}
        onStop={onStop}
        onRemove={onRemove}
        onOpenSettings={() => {
          setNotice(null);
          setSettingsOpen(true);
        }}
      />
      <div className="main-col">
        <TabBar
          tabs={tabs}
          activeKey={activeKey}
          onFocus={(key) => {
            setNotice(null);
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
          onNew={() => {
            setNotice(null);
            // Drop any unconsumed selection intent so spawning from the dialog
            // doesn't later yank focus off newButtonRef onto the new terminal.
            focusOnSelect.current = false;
            setDialogOpen(true);
          }}
          newButtonRef={newButtonRef}
        />
        {notice && (
          <div className="notice" role="status">
            {notice}
          </div>
        )}
        <div className="panes">
          {tabs.length === 0 ? (
            <p className="status">
              No sessions. Click "+ New session" to start one.
            </p>
          ) : (
            tabs.map((t) => (
              <div
                key={t.key}
                className="pane"
                style={t.key === activeKey ? undefined : { display: "none" }}
              >
                {t.status === "stopped" ? (
                  <div className="pane-status" role="status">
                    <p>Session stopped{t.error ? `: ${t.error}` : "."}</p>
                    <div className="pane-status-actions">
                      <button
                        type="button"
                        onClick={() => void respawnTab(t.key)}
                      >
                        Respawn
                      </button>
                      <button
                        type="button"
                        onClick={() => onRemoveTab(t.projectId, t.sessionId)}
                      >
                        Remove…
                      </button>
                    </div>
                  </div>
                ) : t.status === "disconnected" ? (
                  <div className="pane-status pane-status--error" role="alert">
                    <p>Disconnected: {t.error}</p>
                    <div className="pane-status-actions">
                      <button
                        type="button"
                        onClick={() => void respawnTab(t.key)}
                      >
                        Respawn
                      </button>
                      <button
                        type="button"
                        onClick={() => onRemoveTab(t.projectId, t.sessionId)}
                      >
                        Remove…
                      </button>
                    </div>
                  </div>
                ) : (
                  <Terminal
                    connection={t.connection}
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
      {dialogOpen && (
        <NewSessionDialog
          config={config}
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
