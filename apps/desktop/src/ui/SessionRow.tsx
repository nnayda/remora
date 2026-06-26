import type { IndicatorState } from "../status-state";
import { StatusIndicator } from "./StatusIndicator";
import "./SessionRow.css";

/**
 * A live agent session in the sidebar — the densest, most-repeated row in the
 * product. Carries the activity pulse, name, agent + branch meta, an optional
 * count chip, and a hover-revealed actions slot (e.g. a row menu). Active rows
 * get the accent rail and tint.
 */
export interface SessionRowProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  /** Session title. */
  name: string;
  /** Agent CLI label. */
  agent?: string;
  /** Git branch / worktree ref. */
  branch?: string | null;
  /** Agent activity state — drives the pulse. @default "idle" */
  state?: IndicatorState;
  /** Whether we have a live terminal for this session and therefore know its
   * status. When false the row reads as disconnected: no status dot (we can't
   * know the state) and muted text. @default true */
  connected?: boolean;
  /** Selected session. @default false */
  active?: boolean;
  /** Unread / queued count chip. */
  count?: number | null;
  /** Trailing controls revealed on hover/focus — typically a row menu trigger. */
  actions?: React.ReactNode;
}

export function SessionRow({
  name,
  agent,
  branch = null,
  state = "idle",
  connected = true,
  active = false,
  count = null,
  actions = null,
  className = "",
  ...props
}: SessionRowProps) {
  const cls = [
    "rmra-srow",
    active ? "rmra-srow--active" : "",
    connected ? "" : "rmra-srow--disconnected",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  const hasTrail = count != null || actions;
  // Adaptation: with no real agent label and no branch, render no meta row
  // (do not fall back to a fabricated default agent name).
  const hasMeta = Boolean(agent) || Boolean(branch);
  return (
    <div className={cls}>
      <button className="rmra-srow__main" {...props}>
        <span className="rmra-srow__pulse">
          {connected ? (
            <StatusIndicator state={state} />
          ) : (
            // Not connected — we have no terminal, so no status to show. Reserve
            // the dot's footprint so names stay aligned with connected rows.
            <span className="rmra-srow__pulse-empty" aria-hidden="true" />
          )}
        </span>
        <span className="rmra-srow__body">
          <span className="rmra-srow__name">{name}</span>
          {hasMeta && (
            <span className="rmra-srow__meta">
              {agent && <span className="rmra-srow__agent">{agent}</span>}
              {branch && <span className="rmra-srow__sep">/</span>}
              {branch && <span>{branch}</span>}
            </span>
          )}
        </span>
      </button>
      {hasTrail && (
        <span className="rmra-srow__trail">
          {count != null && <span className="rmra-srow__count">{count}</span>}
          {actions && <span className="rmra-srow__actions">{actions}</span>}
        </span>
      )}
    </div>
  );
}
