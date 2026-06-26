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
