import type { IndicatorState } from "../status-state";
import { DotmLoader } from "./DotmLoader";

/* ============================================================
   StatusIndicator — the canonical inline agent-state dot.
   A thin wrapper around DotmLoader locked to the small
   "indicator" scale. This is the SINGLE SOURCE OF TRUTH for
   how a session's state reads inside rows, tabs and lists:
   tune size / dotSize here and every consumer updates.
   For the hero/booting loader, use DotmLoader directly.
   ============================================================ */

// Canonical inline-indicator scale. Change these to restyle
// the dot everywhere it appears inline.
export const STATUS_INDICATOR_SIZE = 12;
export const STATUS_INDICATOR_DOT_SIZE = 2;

export interface StatusIndicatorProps
  extends Omit<React.HTMLAttributes<HTMLDivElement>, "color"> {
  /** Agent state — sets color token and animation. @default "idle" */
  state?: IndicatorState;
  /** Outer box size in px (grid is always 5×5). @default 12 */
  size?: number;
  /** Dot diameter in px. @default 2 */
  dotSize?: number;
}

export function StatusIndicator({
  state = "idle",
  size = STATUS_INDICATOR_SIZE,
  dotSize = STATUS_INDICATOR_DOT_SIZE,
  ...props
}: StatusIndicatorProps) {
  return <DotmLoader state={state} size={size} dotSize={dotSize} {...props} />;
}
