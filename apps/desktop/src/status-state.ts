import type { ActivityState } from "./activity-store"; // "working"|"idle"|"awaiting"|"unknown"
import type { Tab } from "./session-store"; // tab.status: "live"|"reconnecting"|"stopped"|"disconnected"

export type IndicatorState = "idle" | "working" | "needs" | "done" | "error";

/** Sidebar session leaf: lifecycle ("live"|"stopped") + optional activity. */
export function sessionIndicatorState(
  lifecycle: "live" | "stopped",
  activity?: ActivityState,
): IndicatorState {
  if (lifecycle === "stopped") return "idle"; // muted; stopped != success
  switch (activity) {
    case "working":
      return "working";
    case "awaiting":
      return "needs"; // "needs you" — the emotional center
    default:
      return "idle"; // "idle" | "unknown" | undefined
  }
}

/** Workspace tab: transport status takes precedence, else live → activity. */
export function tabIndicatorState(
  status: Tab["status"],
  activity?: ActivityState,
): IndicatorState {
  switch (status) {
    case "disconnected":
      return "error";
    case "reconnecting":
      return "working"; // in-motion transport
    case "stopped":
      return "idle";
    default:
      return sessionIndicatorState("live", activity); // "live"
  }
}
