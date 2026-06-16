// apps/desktop/src/App.tsx
import { useEffect, useRef, useState } from "react";
import "./App.css";
import { NewSessionDialog } from "./NewSessionDialog";
import { TabBar } from "./TabBar";
import { Terminal } from "./Terminal";
import { sessionStore, useSessions } from "./useSessions";

export const APP_NAME = "Remora";

function App() {
  const { tabs, activeKey, openSession, closeTab, focusTab } = useSessions();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const newButtonRef = useRef<HTMLButtonElement>(null);

  // App teardown closes every live session (window close). The store is a
  // module singleton, so this is the one place that ends connections.
  useEffect(() => () => sessionStore.dispose(), []);

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
        onFocus={focusTab}
        onClose={closeTab}
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
