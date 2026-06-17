import { useRef, useState } from "react";
import "./App.css";
import { NewSessionDialog } from "./NewSessionDialog";
import { TabBar } from "./TabBar";
import { Terminal } from "./Terminal";
import { useSessions } from "./useSessions";

export const APP_NAME = "Remora";

function App() {
  const { tabs, activeKey, openSession, closeTab, focusTab } = useSessions();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const newButtonRef = useRef<HTMLButtonElement>(null);

  // No teardown on React unmount: the store is a process-scoped singleton, and
  // a StrictMode/HMR remount must NOT dispose it (decision 1). Process/window
  // exit closes the OS-level PTY + bridge channels; a future window-close hook
  // can call sessionStore.dispose() if explicit teardown is ever needed.

  function handleOpened(attached: boolean) {
    setDialogOpen(false);
    setNotice(attached ? "Attached to an existing session." : null);
    newButtonRef.current?.focus();
  }

  return (
    <main className="app">
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
