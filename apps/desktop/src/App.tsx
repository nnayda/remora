import { useEffect, useMemo, useRef, useState } from "react";
import "./App.css";
import { NewSessionDialog } from "./NewSessionDialog";
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
  } = useSessions();
  useReconnect(sessionStore);
  const { config, sessions, configError, discoveryUnavailable, refresh } =
    useDiscovery();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
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
                    <p>Session stopped.</p>
                    <button
                      type="button"
                      onClick={() => void respawnTab(t.key)}
                    >
                      Respawn
                    </button>
                  </div>
                ) : t.status === "disconnected" ? (
                  <div className="pane-status pane-status--error" role="alert">
                    <p>Disconnected: {t.error}</p>
                    <button
                      type="button"
                      onClick={() => void respawnTab(t.key)}
                    >
                      Respawn
                    </button>
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
    </main>
  );
}

export default App;
