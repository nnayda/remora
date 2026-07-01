import { useSyncExternalStore } from "react";
import { ActivityStore } from "./activity-store";

/** App-scoped singleton (module scope, like sessionStore/discoveryStore) so the
 * activity map survives React StrictMode/HMR remounts. Each TerminalController
 * calls setStatus/setPreview/clear directly — no sweep needed. */
export const activityStore = new ActivityStore();

/** Subscribe a component to the activity singleton; returns the live
 * `key → ActivityState` snapshot. */
export function useActivity() {
  return useSyncExternalStore(
    activityStore.subscribe,
    activityStore.getSnapshot,
  );
}

/** Subscribe to the live `key → preview` snapshot (agent-claimed text). Separate
 * from useActivity so a preview update never re-renders status-only consumers. */
export function usePreviews() {
  return useSyncExternalStore(
    activityStore.subscribe,
    activityStore.getPreviewSnapshot,
  );
}

/** Subscribe to the live set of session keys whose activity hook is confirmed
 * (a marker was seen this attach). Separate snapshot so it never re-renders
 * status-only consumers. */
export function useMarkerSeen() {
  return useSyncExternalStore(
    activityStore.subscribe,
    activityStore.getMarkerSeenSnapshot,
  );
}
