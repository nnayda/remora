import { useEffect } from "react";

const LONG_GAP_MS = 15_000;

export interface ReconnectTarget {
  reconnectAll(): Promise<void> | void;
  reconnectStale(): void;
}

export function decideReconnect(gapMs: number): "all" | "stale" {
  return gapMs > LONG_GAP_MS ? "all" : "stale";
}

/** Mirrors DiscoveryStore's visibility handling: on regaining focus after a
 * long hidden gap (slept laptop), reconnect every open tab; after a short gap
 * (alt-tab), just kick the tabs already reconnecting. `now` is injectable for
 * tests. */
export function useReconnect(
  store: ReconnectTarget,
  now: () => number = Date.now,
): void {
  useEffect(() => {
    let hiddenAt: number | null = null;
    const onHidden = () => {
      hiddenAt = now();
    };
    const onVisible = () => {
      const gap = hiddenAt === null ? 0 : now() - hiddenAt;
      hiddenAt = null;
      if (decideReconnect(gap) === "all") void store.reconnectAll();
      else store.reconnectStale();
    };
    const onVisibility = () => {
      if (document.visibilityState === "hidden") onHidden();
      else onVisible();
    };
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("focus", onVisible);
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("focus", onVisible);
    };
  }, [store, now]);
}
