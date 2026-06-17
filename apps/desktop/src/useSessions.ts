import { useSyncExternalStore } from "react";
import { attachConnection, openSession, respawnConnection } from "./connection";
import { SessionStore } from "./session-store";

/**
 * App-scoped singleton: lives at module scope so a React StrictMode/HMR remount
 * cannot tear down live connections. Teardown is explicit via `dispose()`
 * (called from App's unmount effect).
 */
export const sessionStore = new SessionStore({
  spawn: (p, s, a) => openSession(p, s, a),
  attach: (p, s) => attachConnection(p, s),
  respawn: (p, s, a) => respawnConnection(p, s, a),
  schedule: (fn, ms) => setTimeout(fn, ms),
});

export function useSessions() {
  const snapshot = useSyncExternalStore(
    sessionStore.subscribe,
    sessionStore.getSnapshot,
  );
  return {
    tabs: snapshot.tabs,
    activeKey: snapshot.activeKey,
    openSession: sessionStore.openSession,
    openViaRespawn: sessionStore.openViaRespawn,
    closeTab: sessionStore.closeTab,
    focusTab: sessionStore.focusTab,
    respawnTab: sessionStore.respawnTab,
  };
}
