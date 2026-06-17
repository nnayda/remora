import { useSyncExternalStore } from "react";
import { openSession } from "./connection";
import { SessionStore } from "./session-store";

/**
 * App-scoped singleton: lives at module scope so a React StrictMode/HMR remount
 * cannot tear down live connections. Teardown is explicit via `dispose()`
 * (called from App's unmount effect).
 */
export const sessionStore = new SessionStore(openSession);

export function useSessions() {
  const snapshot = useSyncExternalStore(
    sessionStore.subscribe,
    sessionStore.getSnapshot,
  );
  return {
    tabs: snapshot.tabs,
    activeKey: snapshot.activeKey,
    openSession: sessionStore.openSession,
    closeTab: sessionStore.closeTab,
    focusTab: sessionStore.focusTab,
  };
}
