import { useCallback, useEffect, useRef } from "react";
import { clampWidth } from "./use-rail-width";
import "./ResizeHandle.css";

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

export interface ResizeHandleProps {
  /** Which edge of the rail the handle occupies. */
  edge: RailEdge;
  /** The rail element, measured for its bounding rect during drag. */
  railRef: React.RefObject<HTMLElement | null>;
  min: number;
  /** Effective (already viewport-capped) max. */
  max: number;
  ariaLabel: string;
  /** Current width, for aria-valuenow + keyboard math. */
  value: number;
  /** Keyboard nudge step in px (default 8). */
  step?: number;
  /** Live, during drag/keyboard — state only. */
  onResize: (width: number) => void;
  /** Persist — called on pointerup / after a keyboard nudge. */
  onCommit: (width?: number) => void;
  /** Double-click — reset to default. */
  onReset: () => void;
  /** Notifies the parent so it can toggle the no-transition class during drag. */
  onResizingChange?: (active: boolean) => void;
}

/** Restore body styles set during a drag. Idempotent. */
function releaseBody(): void {
  document.body.style.removeProperty("user-select");
  document.body.style.removeProperty("cursor");
}

export function ResizeHandle({
  edge,
  railRef,
  min,
  max,
  ariaLabel,
  value,
  step = 8,
  onResize,
  onCommit,
  onReset,
  onResizingChange,
}: ResizeHandleProps) {
  const draggingRef = useRef(false);

  const endDrag = useCallback(() => {
    if (!draggingRef.current) return;
    draggingRef.current = false;
    releaseBody();
    onResizingChange?.(false);
    onCommit();
  }, [onCommit, onResizingChange]);

  // Safety net: if the handle unmounts mid-drag (e.g. the parent stops
  // rendering it because the window crossed the mobile breakpoint), run the
  // full endDrag — not just releaseBody — so the parent's resizing flag is
  // cleared and the in-progress width is committed. Idempotent via the
  // draggingRef guard, so a no-op when not dragging.
  const endDragRef = useRef(endDrag);
  endDragRef.current = endDrag;
  useEffect(() => () => endDragRef.current(), []);

  const onPointerDown = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      if (e.button !== 0) return;
      e.preventDefault();
      e.currentTarget.setPointerCapture(e.pointerId);
      draggingRef.current = true;
      document.body.style.userSelect = "none";
      document.body.style.cursor = "col-resize";
      onResizingChange?.(true);
    },
    [onResizingChange],
  );

  const onPointerMove = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      if (!draggingRef.current) return;
      const rail = railRef.current;
      if (!rail) return;
      const rect = rail.getBoundingClientRect();
      onResize(nextWidthFromPointer(e.clientX, rect, edge, min, max));
    },
    [railRef, edge, min, max, onResize],
  );

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLDivElement>) => {
      const next = nextWidthFromKey(e, value, step, min, max, edge);
      if (next === value) return;
      e.preventDefault();
      onResize(next);
      onCommit(next);
    },
    [value, step, min, max, edge, onResize, onCommit],
  );

  return (
    // biome-ignore lint/a11y/useSemanticElements: <hr> maps to the non-interactive separator role; an interactive (focusable, tabIndex) separator/splitter must be a <div role="separator"> per the ARIA APG
    <div
      className="rk-resize-handle"
      role="separator"
      aria-orientation="vertical"
      aria-label={ariaLabel}
      aria-valuenow={Math.round(value)}
      aria-valuemin={min}
      aria-valuemax={max}
      tabIndex={0}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      onLostPointerCapture={endDrag}
      onDoubleClick={onReset}
      onKeyDown={onKeyDown}
    />
  );
}
