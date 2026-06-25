import type { ReactNode } from "react";
import "./Tooltip.css";

/**
 * Lightweight CSS hover/focus tooltip. Wraps a trigger; shows on hover or
 * keyboard focus. Optionally renders a keyboard-shortcut hint.
 */
export interface TooltipProps {
  /** Trigger element. */
  children: ReactNode;
  /** Tooltip text. */
  content: ReactNode;
  /** Placement. @default "top" */
  side?: "top" | "bottom" | "left" | "right";
  /** Optional shortcut hint rendered as a kbd chip (e.g. "⌘K"). */
  kbd?: string;
}

export function Tooltip({
  children,
  content,
  side = "top",
  kbd,
}: TooltipProps) {
  return (
    <span className="rmra-tip">
      {children}
      <span className={`rmra-tip__pop rmra-tip__pop--${side}`} role="tooltip">
        {content}
        {kbd && <span className="rmra-tip__kbd">{kbd}</span>}
      </span>
    </span>
  );
}
