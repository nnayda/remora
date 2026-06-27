import { clampWidth } from "./use-rail-width";

export type RailEdge = "left" | "right";

/**
 * Pointer X → rail width. edge names which edge of the rail the handle sits on:
 *   edge:"right" (left sidebar, handle on its right edge) → clientX - rect.left
 *   edge:"left"  (right panel, handle on its left edge)   → rect.right - clientX
 */
export function nextWidthFromPointer(
  clientX: number,
  rect: { left: number; right: number },
  edge: RailEdge,
  min: number,
  max: number,
): number {
  const raw = edge === "right" ? clientX - rect.left : rect.right - clientX;
  return clampWidth(raw, min, max);
}

/**
 * Keyboard → rail width. Arrows map to SPATIAL movement of the handle, so the
 * numeric width direction depends on the edge. Shift = 4× step. Home/End jump
 * to the bounds. Unhandled keys return current unchanged.
 */
export function nextWidthFromKey(
  e: { key: string; shiftKey?: boolean },
  current: number,
  step: number,
  min: number,
  max: number,
  edge: RailEdge,
): number {
  const delta = (e.shiftKey ? step * 4 : step) * (edge === "right" ? 1 : -1);
  switch (e.key) {
    case "ArrowRight":
      return clampWidth(current + delta, min, max);
    case "ArrowLeft":
      return clampWidth(current - delta, min, max);
    case "Home":
      return min;
    case "End":
      return max;
    default:
      return current;
  }
}
