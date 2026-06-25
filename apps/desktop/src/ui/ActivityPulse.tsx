import type { IndicatorState } from "../status-state";
import "./ActivityPulse.css";

/**
 * Remora's signature motion. The single place the system gets expressive:
 * a calm breathing accent for "agent working", a faster high-contrast beat
 * for "needs you", and static dots for idle / done / error. This is the
 * brand's hero moment — use it for session activity, nowhere decorative.
 */
export interface ActivityPulseProps
  extends React.HTMLAttributes<HTMLSpanElement> {
  /** Agent session state. @default "idle" */
  state?: IndicatorState;
  /** @default "md" */
  size?: "sm" | "md" | "lg";
}

const HAS_RING: Partial<Record<IndicatorState, boolean>> = {
  working: true,
  needs: true,
};

export function ActivityPulse({
  state = "idle",
  size = "md",
  className = "",
  ...props
}: ActivityPulseProps) {
  const cls = [
    "rmra-pulse",
    `rmra-pulse--${state}`,
    `rmra-pulse--${size}`,
    className,
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <span className={cls} role="status" {...props}>
      {HAS_RING[state] && <span className="rmra-pulse__ring" />}
      <span className="rmra-pulse__core" />
    </span>
  );
}
