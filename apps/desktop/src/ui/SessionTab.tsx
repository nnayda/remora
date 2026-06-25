import type { IndicatorState } from "../status-state";
import { StatusIndicator } from "./StatusIndicator";
import "./SessionTab.css";

/**
 * One tab per open session in the tab bar above the terminal. Carries the
 * activity pulse so a backgrounded session can still signal "needs you".
 */
export interface SessionTabProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  /** Tab title. */
  label: string;
  /** Agent activity state — drives the pulse. @default "idle" */
  state?: IndicatorState;
  /** Current/focused tab. @default false */
  active?: boolean;
  /** Unsaved/dirty marker — shows a dot that swaps to the × close affordance on hover/focus. @default false */
  dirty?: boolean;
  /** Close handler — renders the × affordance when provided. */
  onClose?: (e: React.MouseEvent) => void;
}

export function SessionTab({
  label,
  state = "idle",
  active = false,
  dirty = false,
  onClose,
  className = "",
  ...props
}: SessionTabProps) {
  const cls = [
    "rmra-tab",
    active ? "rmra-tab--active" : "",
    onClose ? "rmra-tab--closable" : "",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  // The tab and its close affordance are sibling buttons (not nested — a button
  // inside a button is invalid). The outer element is presentational; the
  // interactive props (onClick, role="tab", aria-*) ride on the main button.
  return (
    <div className={cls}>
      <button type="button" className="rmra-tab__main" {...props}>
        <StatusIndicator state={state} />
        <span className="rmra-tab__label">{label}</span>
      </button>
      {(dirty || onClose) && (
        <span className="rmra-tab__trail">
          {dirty && <span className="rmra-tab__dirty" />}
          {onClose && (
            <button
              type="button"
              className="rmra-tab__close"
              aria-label={`Close ${label}`}
              onClick={(e) => {
                e.stopPropagation();
                onClose(e);
              }}
            >
              ×
            </button>
          )}
        </span>
      )}
    </div>
  );
}
