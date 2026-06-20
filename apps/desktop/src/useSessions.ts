import { useSyncExternalStore } from "react";
import { attachConnection, openSession, respawnConnection } from "./connection";
import { stopSession, removeSession } from "./bridge";
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
  stop: (p, s) => stopSession(p, s),
  remove: (p, s, force) => removeSession(p, s, force),
});

/** Subscribe a component to the session-store singleton: returns the live tabs
 * and active key plus the open/close/focus/respawn actions. */
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
    stopSession: sessionStore.stop,
    removeSession: sessionStore.remove,
  };
}
