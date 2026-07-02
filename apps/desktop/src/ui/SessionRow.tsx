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
  /** An open for this session is in flight (clicked, awaiting spawn/attach). A
   * spinner fills the status slot until it resolves, so a slow connect reads as
   * "working" rather than a dead click (#170). @default false */
  connecting?: boolean;
  /** A remove for this session is running in the background (the confirm
   * dialog closed immediately). A spinner fills the status slot until the
   * teardown settles — the row then vanishes on success or returns to normal
   * on failure. Takes precedence over `connecting` and the status dot.
   * @default false */
  removing?: boolean;
  /** Selected session. @default false */
  active?: boolean;
  /** Unread / queued count chip. */
  count?: number | null;
  /** Trailing controls revealed on hover/focus — typically a row menu trigger. */
  actions?: React.ReactNode;
  /** Host is unavailable; render dimmed with a reconnecting cue. @default false */
  reconnecting?: boolean;
}

export function SessionRow({
  name,
  agent,
  branch = null,
  state = "idle",
  connected = true,
  connecting = false,
  removing = false,
  active = false,
  count = null,
  actions = null,
  reconnecting = false,
  className = "",
  ...props
}: SessionRowProps) {
  const cls = [
    "rmra-srow",
    active ? "rmra-srow--active" : "",
    connected ? "" : "rmra-srow--disconnected",
    reconnecting ? "rmra-srow--reconnecting" : "",
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
          {removing ? (
            // Remove in flight: same spinner as `connecting`, different label —
            // the row is being torn down in the background, not opened.
            <span
              className="rmra-srow__spinner"
              role="status"
              aria-label="Removing…"
            />
          ) : connecting ? (
            // Open in flight: spin in the dot's footprint so a slow connect
            // reads as working, not a dead click (#170). Takes precedence over
            // the status dot — there is no live status to show yet.
            <span
              className="rmra-srow__spinner"
              role="status"
              aria-label="Connecting…"
            />
          ) : connected ? (
            <StatusIndicator state={state} />
          ) : (
            // Not connected — we have no terminal, so no status to show. Reserve
            // the dot's footprint so names stay aligned with connected rows.
            <span className="rmra-srow__pulse-empty" aria-hidden="true" />
          )}
        </span>
        <span className="rmra-srow__body">
          <span className="rmra-srow__name">{name}</span>
          {reconnecting && (
            <span className="rmra-srow__reconnecting">reconnecting…</span>
          )}
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
