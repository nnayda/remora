import { useEffect, useSyncExternalStore } from "react";
import { getConfig, listSessions } from "./bridge";
import { DiscoveryStore } from "./discovery-store";

/**
 * App-scoped singleton (module scope, like `sessionStore`) so live config +
 * discovery state survives React StrictMode/HMR remounts. The store holds all
 * poll/guard/error logic; this hook is the thin `useSyncExternalStore` wrapper
 * plus the DOM glue that pauses polling while the window is hidden and
 * refreshes on focus (D7).
 */
export const discoveryStore = new DiscoveryStore({
  loadConfig: getConfig,
  listSessions,
});

/** Subscribe a component to the discovery singleton and own its DOM glue
 * (start once, pause while hidden, refresh on focus). Returns the live config,
 * sessions, error flags, and the manual `refresh`/`refreshAfterOpen` actions. */
export function useDiscovery() {
  const snapshot = useSyncExternalStore(
    discoveryStore.subscribe,
    discoveryStore.getSnapshot,
  );

  useEffect(() => {
    // start() is idempotent, so a StrictMode double-mount is safe.
    void discoveryStore.start();

    // Pause polling when the window is hidden; refresh the moment it's visible
    // or focused again. These standard DOM events fire in the Tauri webview; if
    // a platform proves unreliable, swap to `@tauri-apps/api/window`
    // `onFocusChanged` here (see stage-10 spec D7) — the store API is the same.
    const onVisibility = () => {
      discoveryStore.setActive(document.visibilityState === "visible");
    };
    const onFocus = () => discoveryStore.setActive(true);
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("focus", onFocus);
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("focus", onFocus);
      // Intentionally NOT stopping the singleton on unmount: it is
      // process-scoped and a remount must not tear down live polling.
    };
  }, []);

  return {
    config: snapshot.config,
    sessions: snapshot.sessions,
    configError: snapshot.configError,
    discoveryUnavailable: snapshot.discoveryUnavailable,
    refresh: discoveryStore.refresh,
    refreshAfterOpen: discoveryStore.refreshAfterOpen,
  };
}
