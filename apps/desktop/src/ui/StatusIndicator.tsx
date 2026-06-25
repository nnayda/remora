import type { IndicatorState } from "../status-state";
import { ActivityPulse } from "./ActivityPulse";

/* ============================================================
   StatusIndicator — the canonical inline agent-state dot.
   A thin wrapper around ActivityPulse locked to the small
   inline scale. This is the SINGLE SOURCE OF TRUTH for how a
   session's state reads inside rows, tabs and lists: tune the
   scale here and every consumer updates.
   ============================================================ */

export interface StatusIndicatorProps
  extends Omit<React.HTMLAttributes<HTMLSpanElement>, "color"> {
  /** Agent state — sets color token and animation. @default "idle" */
  state?: IndicatorState;
  /** Indicator scale. @default "sm" */
  size?: "sm" | "md" | "lg";
}

export function StatusIndicator({
  state = "idle",
  size = "sm",
  ...props
}: StatusIndicatorProps) {
  return <ActivityPulse state={state} size={size} {...props} />;
}
