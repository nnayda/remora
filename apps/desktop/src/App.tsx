import { useMemo, useRef, useState } from "react";
import "./App.css";
import { NewSessionDialog } from "./NewSessionDialog";
import { Sidebar } from "./Sidebar";
import { buildTree, type SessionNode } from "./session-tree";
import { TabBar } from "./TabBar";
import { Terminal } from "./Terminal";
import { discoveryStore, useDiscovery } from "./useDiscovery";
import { useSessions } from "./useSessions";

export const APP_NAME = "Remora";

function App() {
  const { tabs, activeKey, openSession, closeTab, focusTab } = useSessions();
  const { config, sessions, configError, discoveryUnavailable, refresh } =
    useDiscovery();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const newButtonRef = useRef<HTMLButtonElement>(null);

  // Recompute the tree only when config or the polled session list changes.
  const tree = useMemo(() => buildTree(config, sessions), [config, sessions]);
  const openKeys = useMemo(() => new Set(tabs.map((t) => t.key)), [tabs]);

  // Open (attach/focus) a live session clicked in the sidebar. Reuses the same
  // deduping path as the dialog — clicking an already-open session just focuses
  // it. No discovery refresh here: attaching an existing session changes no
  // server state (Codex #9); only the spawn path (handleOpened) refreshes.
  function openFromSidebar(node: SessionNode) {
    setNotice(null);
    void openSession({
      projectId: node.projectId,
      sessionId: node.sessionId,
      agent: node.agent,
    });
  }

  // No teardown on React unmount: the store is a process-scoped singleton, and
  // a StrictMode/HMR remount must NOT dispose it (decision 1). Process/window
  // exit closes the OS-level PTY + bridge channels; a future window-close hook
  // can call sessionStore.dispose() if explicit teardown is ever needed.

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
            focusTab(key);
          }}
          onClose={(key) => {
            setNotice(null);
            closeTab(key);
          }}
          onNew={() => {
            setNotice(null);
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
                <Terminal connection={t.connection} />
              </div>
            ))
          )}
        </div>
      </div>
      {dialogOpen && (
        <NewSessionDialog
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
