import { useEffect, useState } from "react";
import type { IndicatorState } from "../status-state";
import "./DotmLoader.css";

/* ============================================================
   DotmLoader — a port of zzzzshawn's Dot Matrix loaders
   (github.com/zzzzshawn/matrix, MIT) into Remora tokens.

   Only the two variants Remora uses are ported:
     - square4 — twin-perimeter ring chase (outer CW + middle CCW,
       hollow center). The expressive "working / booting" hero.
     - square7 — bottom-up fill + double flash. A staged loader.

   The library's gradient colorPresets are intentionally NOT ported
   (the system bans gradients). Color comes from one flat token,
   chosen by `state`; the matrix inherits it via currentColor.
   ============================================================ */

type Variant = "square4" | "pulse" | "square7";

const N = 5;
const rowMajor = (r: number, c: number) => r * N + c;
const coord = (i: number) => ({ row: Math.floor(i / N), col: i % N });

/* --- ring traversal orders (verbatim from core/grid-paths) --- */
const OUTER_COORDS: Array<[number, number]> = [
  [0, 0],
  [0, 1],
  [0, 2],
  [0, 3],
  [0, 4],
  [1, 4],
  [2, 4],
  [3, 4],
  [4, 4],
  [4, 3],
  [4, 2],
  [4, 1],
  [4, 0],
  [3, 0],
  [2, 0],
  [1, 0],
];
const MIDDLE_COORDS: Array<[number, number]> = [
  [1, 1],
  [2, 1],
  [3, 1],
  [3, 2],
  [3, 3],
  [2, 3],
  [1, 3],
  [1, 2],
];
const OUTER_ORDER: number[] = new Array(N * N).fill(-1);
OUTER_COORDS.forEach(([r, c], t) => {
  OUTER_ORDER[rowMajor(r, c)] = t;
});
const MIDDLE_ORDER: number[] = new Array(N * N).fill(-1);
MIDDLE_COORDS.forEach(([r, c], t) => {
  MIDDLE_ORDER[rowMajor(r, c)] = t;
});

/* --- square7 frame masks (verbatim) --- */
const FRAME_MASKS = [
  "....." + "....." + "....." + "....." + "ooooo",
  "....." + "....." + "....." + "ooooo" + "ooooo",
  "....." + "....." + "ooooo" + "ooooo" + "ooooo",
  "....." + "ooooo" + "ooooo" + "ooooo" + "ooooo",
  "ooooo" + "ooooo" + "ooooo" + "ooooo" + "ooooo",
  "ccccc" + "ccccc" + "ccccc" + "ccccc" + "ccccc",
  "....." + "....." + "....." + "....." + ".....",
  "ccccc" + "ccccc" + "ccccc" + "ccccc" + "ccccc",
  "....." + "....." + "....." + "....." + ".....",
  "....." + "....." + "....." + "....." + ".....",
];
const FRAME_SEQUENCE = [0, 1, 2, 3, 4, 4, 5, 6, 7, 8, 9];
const S7 = { BASE: 0.08, SETTLED: 0.42, ACTIVE: 1, CLEAR: 0.88 };
const S7_IDLE_STEP = 10;

/* --- opacity remap (verbatim from core) --- */
const SRC_BASE = 0.08;
const SRC_MID = 0.34;
const SRC_PEAK = 0.94;
const clmp = (v: number) => Math.min(1, Math.max(0, v));
const lerp = (a: number, b: number, p: number) => a + (b - a) * p;
const nprog = (v: number, a: number, b: number) => {
  const s = b - a;
  return Math.abs(s) < 1e-9 ? 0 : clmp((v - a) / s);
};
function remapOpacity(o: number, tb: number, tm: number, tp: number): number {
  if (!Number.isFinite(o)) return o;
  const so = clmp(o);
  if (so <= SRC_BASE) return clmp(lerp(0, tb, nprog(so, 0, SRC_BASE)));
  if (so <= SRC_MID) return clmp(lerp(tb, tm, nprog(so, SRC_BASE, SRC_MID)));
  if (so <= SRC_PEAK) return clmp(lerp(tm, tp, nprog(so, SRC_MID, SRC_PEAK)));
  return clmp(lerp(tp, 1, nprog(so, SRC_PEAK, 1)));
}

/* --- per-state defaults --- */
const STATE_COLOR: Record<IndicatorState, string> = {
  working: "var(--accent-bright)",
  needs: "var(--accent-bright)",
  idle: "var(--text-muted)",
  done: "var(--success)",
  error: "var(--danger)",
};
const STATE_OPACITY: Record<IndicatorState, [number, number, number]> = {
  // [base, mid, peak]
  working: [0.12, 0.42, 1],
  needs: [0.2, 0.42, 0.9],
  idle: [0.12, 0.42, 0.4],
  done: [0.78, 0.42, 1],
  error: [0.78, 0.42, 1],
};
const STATE_VARIANT: Record<IndicatorState, Variant> = {
  working: "square4",
  needs: "pulse",
  idle: "square7",
  done: "square7",
  error: "square7",
};
const STATE_SPEED: Record<IndicatorState, number> = {
  working: 1,
  needs: 0.62,
  idle: 1,
  done: 1,
  error: 1,
};

function usePrefersReducedMotion(): boolean {
  const [r, setR] = useState(false);
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return undefined;
    const q = window.matchMedia("(prefers-reduced-motion: reduce)");
    const u = () => setR(q.matches);
    u();
    q.addEventListener("change", u);
    return () => q.removeEventListener("change", u);
  }, []);
  return r;
}

function useSteppedCycle(
  active: boolean,
  cycleMsBase: number,
  steps: number,
  speed: number,
  idleStep: number,
): number {
  const [step, setStep] = useState(active ? 0 : idleStep);
  useEffect(() => {
    if (!active) {
      setStep(idleStep);
      return undefined;
    }
    const safeSpeed = speed > 0 ? speed : 1;
    const stepMs = Math.max(1, cycleMsBase / safeSpeed / steps);
    const cycleMs = stepMs * steps;
    const start = performance.now();
    let raf = 0;
    let cur = -1;
    const tick = (now: number) => {
      const elapsed = Math.max(0, now - start);
      const ns = Math.floor((elapsed % cycleMs) / stepMs) % steps;
      if (ns !== cur) {
        cur = ns;
        setStep(ns);
      }
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [active, cycleMsBase, steps, speed, idleStep]);
  return active ? step : idleStep;
}

export interface DotmLoaderProps
  extends Omit<React.HTMLAttributes<HTMLDivElement>, "color"> {
  /** Agent state — sets color token, default variant, animation. @default "working" */
  state?: IndicatorState;
  /** Override the traversal. square4 = ring chase; pulse = heartbeat beat; square7 = bottom-up fill + flash. */
  variant?: Variant;
  /** Outer box size in px (grid is always 5×5). @default 64 */
  size?: number;
  /** Dot diameter in px. Defaults to ~size/8. */
  dotSize?: number;
  /** Animation speed multiplier (>1 faster). Defaults per state. */
  speed?: number;
  /** Force animation on/off. Defaults true for working/needs, false otherwise. */
  animated?: boolean;
  /** Soft glow on active dots. Defaults on for "needs". */
  bloom?: boolean;
  /** Rest / settled / peak opacity overrides (0–1). */
  opacityBase?: number;
  opacityMid?: number;
  opacityPeak?: number;
  /** @default "Loading" */
  ariaLabel?: string;
}

export function DotmLoader({
  state = "working",
  variant,
  size = 64,
  dotSize,
  speed,
  animated,
  bloom,
  opacityBase,
  opacityMid,
  opacityPeak,
  ariaLabel = "Loading",
  className = "",
  ...props
}: DotmLoaderProps) {
  const reduced = usePrefersReducedMotion();

  const v = variant || STATE_VARIANT[state] || "square4";
  const spd = speed != null ? speed : STATE_SPEED[state] || 1;
  const isAnimated =
    (animated != null ? animated : state === "working" || state === "needs") &&
    !reduced;
  const halo = bloom != null ? bloom : false;

  const color = STATE_COLOR[state] || "var(--accent-bright)";
  const dflt = STATE_OPACITY[state] || [0.12, 0.42, 1];
  const ob = opacityBase != null ? opacityBase : dflt[0];
  const om = opacityMid != null ? opacityMid : dflt[1];
  const op = opacityPeak != null ? opacityPeak : dflt[2];

  const ds = dotSize != null ? dotSize : Math.max(2, Math.round(size / 8));
  const gap = Math.max(1, Math.floor((size - ds * N) / (N - 1)));
  const extent = ds * N + gap * (N - 1);

  // square7 stepping (hook called unconditionally)
  const step = useSteppedCycle(
    v === "square7" && isAnimated,
    1900,
    FRAME_SEQUENCE.length,
    spd,
    Math.min(S7_IDLE_STEP, FRAME_SEQUENCE.length - 1),
  );
  const frameIdx = v === "square7" ? (FRAME_SEQUENCE[step] ?? 0) : 0;
  const mask = FRAME_MASKS[frameIdx];

  const dots = [];
  for (let i = 0; i < N * N; i++) {
    const { row, col } = coord(i);
    let cls = "dmx-dot";
    const st: Record<string, string | number> = { width: ds, height: ds };
    let inactive = false;
    let rawOpacity: number | null = null;

    if (v === "square4") {
      if (row === 2 && col === 2) {
        inactive = true;
      } else if (OUTER_ORDER[i] >= 0) {
        if (!isAnimated) {
          rawOpacity = 0.2 + (OUTER_ORDER[i] / 15) * 0.72;
        } else {
          cls += " dmx-outer-snake";
          st["--dmx-outer-order"] = OUTER_ORDER[i];
        }
      } else {
        if (!isAnimated) {
          rawOpacity = 0.2 + (MIDDLE_ORDER[i] / 7) * 0.72;
        } else {
          cls += " dmx-middle-snake";
          st["--dmx-middle-order"] = MIDDLE_ORDER[i];
        }
      }
    } else if (v === "pulse") {
      if (!isAnimated) {
        rawOpacity = op;
      } else {
        cls += " dmx-heartbeat";
        const dist = Math.hypot(row - 2, col - 2) / 2.8284271247;
        st["--dmx-dist-norm"] = dist;
      }
    } else {
      const cell = mask[i];
      rawOpacity =
        cell === "x"
          ? S7.ACTIVE
          : cell === "o"
            ? S7.SETTLED
            : cell === "c"
              ? S7.CLEAR
              : S7.BASE;
    }

    if (inactive) {
      cls += " dmx-inactive";
    } else if (rawOpacity != null) {
      st.opacity = remapOpacity(rawOpacity, ob, om, op);
    }

    dots.push(
      <span
        key={i}
        aria-hidden="true"
        className={cls}
        style={st as React.CSSProperties}
      />,
    );
  }

  const rootStyle: Record<string, string | number> = {
    width: extent,
    height: extent,
    color,
    "--dmx-speed": 1 / (spd > 0 ? spd : 1),
    "--dmx-dot-size": `${ds}px`,
    "--dmx-dot-fill": color,
    "--dmx-opacity-base": ob,
    "--dmx-opacity-mid": om,
    "--dmx-opacity-peak": op,
    "--dmx-halo-level": halo ? 0.55 : 0,
  };

  const rootCls = ["dmx-root", halo && "dmx-bloom", className]
    .filter(Boolean)
    .join(" ");

  return (
    <div
      role="status"
      aria-live="polite"
      aria-label={ariaLabel}
      className={rootCls}
      style={rootStyle as React.CSSProperties}
      {...props}
    >
      <div
        className="dmx-grid"
        style={{
          gap,
          gridTemplateColumns: `repeat(${N}, ${ds}px)`,
          gridTemplateRows: `repeat(${N}, ${ds}px)`,
        }}
      >
        {dots}
      </div>
    </div>
  );
}
