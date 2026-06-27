import type { ActivityState } from "./activity-store";
import type { SessionNode } from "./session-tree";
import { type IndicatorState, sessionIndicatorState } from "./status-state";

/**
 * Aggregate a host's session activity into one dot state for the collapsed rail.
 * Domain is the OPEN (attached) sessions — only those carry a working/awaiting
 * signal; stopped/unattached read as idle. Priority: needs > working > idle, so
 * the rail dot never contradicts the expanded per-session dots.
 */
export function hostIndicatorState(
  sessions: SessionNode[],
  openKeys: Set<string>,
  activity: ReadonlyMap<string, ActivityState>,
): IndicatorState {
  let sawWorking = false;
  for (const s of sessions) {
    if (!openKeys.has(s.key)) continue;
    const state = sessionIndicatorState(s.state, activity.get(s.key));
    if (state === "needs") return "needs";
    if (state === "working") sawWorking = true;
  }
  return sawWorking ? "working" : "idle";
}
