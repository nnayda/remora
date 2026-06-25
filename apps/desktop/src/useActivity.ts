import { useSyncExternalStore } from "react";
import { ActivityStore } from "./activity-store";

/** App-scoped singleton (module scope, like sessionStore/discoveryStore) so the
 * activity map survives React StrictMode/HMR remounts. App starts/stops the
 * sweep in a mount effect. */
export const activityStore = new ActivityStore();

/** Subscribe a component to the activity singleton; returns the live
 * `key → ActivityState` snapshot. */
export function useActivity() {
  return useSyncExternalStore(
    activityStore.subscribe,
    activityStore.getSnapshot,
  );
}
