import { useEffect, useState } from "react";
import "./App.css";
import { connectSession, type SessionConnection } from "./connection";
import { Terminal } from "./Terminal";

export const APP_NAME = "Remora";

// Stage-8 dev harness: one hardcoded session against the fake source. The first
// dev mount spawns; StrictMode's remount and every reload attach (banner
// replays). Tabs and a real spawn picker are stage 9.
function App() {
  const [connection, setConnection] = useState<SessionConnection | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let conn: SessionConnection | undefined;
    let cancelled = false;
    const logClose = (e: unknown) => console.error("session close failed", e);
    connectSession("demo", "scratch", null)
      .then((c) => {
        if (cancelled) void c.close().catch(logClose);
        else {
          conn = c;
          setConnection(c);
        }
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        setError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
      void conn?.close().catch(logClose);
    };
  }, []);

  return (
    <main className="app">
      {error ? (
        <p className="status">Failed to connect: {error}</p>
      ) : connection ? (
        <Terminal connection={connection} />
      ) : (
        <p className="status">Connecting…</p>
      )}
    </main>
  );
}

export default App;
