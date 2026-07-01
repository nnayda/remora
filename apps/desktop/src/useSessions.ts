import { useSyncExternalStore } from "react";
import { removeSession, stopSession } from "./bridge";
import { attachConnection, openSession, respawnConnection } from "./connection";
import { SessionStore } from "./session-store";

/**
 * App-scoped singleton: lives at module scope so a React StrictMode/HMR remount
 * cannot tear down live connections. Teardown is explicit via `dispose()`
 * (called from App's unmount effect).
 */
export const sessionStore = new SessionStore({
  spawn: (p, s, a, b, w, branch, worktreeRoot) =>
    openSession(p, s, a, b, w, branch, worktreeRoot),
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
    connecting: snapshot.connecting,
    openSession: sessionStore.openSession,
    openViaRespawn: sessionStore.openViaRespawn,
    openViaAttach: sessionStore.openViaAttach,
    closeTab: sessionStore.closeTab,
    focusTab: sessionStore.focusTab,
    reorderTab: sessionStore.reorderTab,
    respawnTab: sessionStore.respawnTab,
    stopSession: sessionStore.stop,
    removeSession: sessionStore.remove,
  };
}
