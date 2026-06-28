import { useCallback, useEffect, useRef, useState } from "react";

export interface RailWidthOptions {
  /** localStorage key, e.g. "remora.rail.sidebar". */
  key: string;
  /** Default expanded width (px). */
  defaultWidth: number;
  /** Hard min/max bounds (px). */
  min: number;
  max: number;
}

interface RailState {
  width: number;
  collapsed: boolean;
}

/** Round + clamp to [min, max]; non-finite collapses to min (defends the live drag path). */
export function clampWidth(w: number, min: number, max: number): number {
  if (!Number.isFinite(w)) return min;
  const r = Math.round(w);
  if (r < min) return min;
  if (r > max) return max;
  return r;
}

/**
 * Parse the persisted blob. An out-of-range *numeric* width is clamped; a
 * NaN / missing / non-number / unparseable value falls back to defaultWidth.
 * Never throws.
 */
export function parseRailState(
  raw: string | null,
  opts: RailWidthOptions,
): RailState {
  const fallback: RailState = { width: opts.defaultWidth, collapsed: false };
  if (raw == null) return fallback;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return fallback;
  }
  if (typeof parsed !== "object" || parsed === null) return fallback;
  const obj = parsed as Record<string, unknown>;
  const rawWidth = obj.width;
  const width =
    typeof rawWidth === "number" && Number.isFinite(rawWidth)
      ? clampWidth(rawWidth, opts.min, opts.max)
      : opts.defaultWidth;
  return { width, collapsed: obj.collapsed === true };
}

/** Render the collapsed rail only off-mobile — the mobile layout owns full width. */
export function shouldRenderCollapsed(
  collapsed: boolean,
  isMobile: boolean,
): boolean {
  return collapsed && !isMobile;
}

function readRaw(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null; // storage disabled / private mode
  }
}

function writeState(key: string, state: RailState): void {
  try {
    window.localStorage.setItem(key, JSON.stringify(state));
  } catch {
    /* storage unavailable — keep the in-memory state, skip the write */
  }
}

/**
 * Configured max capped so the rail never eats more than ~40% of the window.
 * Exported so the single rail-bounds source (App's SIDEBAR_RAIL + the
 * ResizeHandle `max` prop) and the hook agree — see App.tsx.
 */
export function effectiveMax(max: number): number {
  if (typeof window === "undefined") return max;
  return Math.min(max, Math.floor(window.innerWidth * 0.4));
}

/**
 * Per-device rail width + collapsed state, persisted in localStorage. Read in
 * the useState initializer (mirrors use-theme.ts) so first paint is correct —
 * no 240→saved flash. setWidth is the HOT path (state only, no write);
 * commitWidth persists once on pointerup.
 */
export function useRailWidth(opts: RailWidthOptions) {
  // One localStorage read on mount; clamp the restored width through
  // effectiveMax so a wide width saved on a large screen can't paint
  // oversized when restored on a narrow window.
  const [initial] = useState((): RailState => {
    const parsed = parseRailState(readRaw(opts.key), opts);
    return {
      width: clampWidth(parsed.width, opts.min, effectiveMax(opts.max)),
      collapsed: parsed.collapsed,
    };
  });
  const [width, setWidthState] = useState(initial.width);
  const [collapsed, setCollapsed] = useState(initial.collapsed);

  const widthRef = useRef(width);
  widthRef.current = width;
  const collapsedRef = useRef(collapsed);
  collapsedRef.current = collapsed;

  const setWidth = useCallback(
    (w: number) => {
      const clamped = clampWidth(w, opts.min, effectiveMax(opts.max));
      // Sync the ref synchronously so a commit fired in the same tick (before a
      // re-render) persists the value just set, not the previous render's.
      widthRef.current = clamped;
      setWidthState(clamped);
    },
    [opts.min, opts.max],
  );

  // Re-clamp the live width when the window shrinks: effectiveMax is viewport-
  // relative, so a width set on a wide window must shrink with the window or it
  // would exceed the ~40% cap until the next drag. clampWidth returns the same
  // value when already in bounds, so React bails on the no-op (no churn).
  useEffect(() => {
    function onResize() {
      setWidthState((w) => {
        const clamped = clampWidth(w, opts.min, effectiveMax(opts.max));
        widthRef.current = clamped;
        return clamped;
      });
    }
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [opts.min, opts.max]);

  const commitWidth = useCallback(
    (next?: number) => {
      const w = next ?? widthRef.current;
      writeState(opts.key, { width: w, collapsed: collapsedRef.current });
    },
    [opts.key],
  );

  const toggleCollapsed = useCallback(() => {
    const next = !collapsedRef.current;
    setCollapsed(next);
    writeState(opts.key, { width: widthRef.current, collapsed: next });
  }, [opts.key]);

  const reset = useCallback(() => {
    const clampedDefault = clampWidth(
      opts.defaultWidth,
      opts.min,
      effectiveMax(opts.max),
    );
    setWidthState(clampedDefault);
    writeState(opts.key, {
      width: clampedDefault,
      collapsed: collapsedRef.current,
    });
  }, [opts.key, opts.defaultWidth, opts.min, opts.max]);

  return { width, collapsed, setWidth, commitWidth, toggleCollapsed, reset };
}
