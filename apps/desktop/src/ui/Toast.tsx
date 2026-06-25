import type { ReactNode } from "react";
import { X } from "./icons";
import "./Toast.css";

/**
 * Transient notification card — session finished, PR opened, agent needs you.
 * A left accent rail carries the tone. Keep copy short.
 */
export interface ToastProps extends React.HTMLAttributes<HTMLDivElement> {
  /** Bold first line. */
  title: string;
  /** Secondary detail line. */
  message?: string;
  /** Tone (left rail + icon color). @default "accent" */
  tone?: "accent" | "success" | "warning" | "danger";
  /** Leading icon node. */
  icon?: ReactNode;
  /** Inline action label (e.g. "View PR"). */
  actionLabel?: string;
  onAction?: () => void;
  /** Dismiss handler — renders the × when provided. */
  onClose?: () => void;
}

export function Toast({
  title,
  message,
  tone = "accent",
  icon,
  actionLabel,
  onAction,
  onClose,
  className = "",
  ...props
}: ToastProps) {
  const cls = ["rmra-toast", `rmra-toast--${tone}`, className]
    .filter(Boolean)
    .join(" ");
  return (
    <div className={cls} role="status" {...props}>
      {icon && <span className="rmra-toast__icon">{icon}</span>}
      <div className="rmra-toast__body">
        <span className="rmra-toast__title">{title}</span>
        {message && <span className="rmra-toast__msg">{message}</span>}
        {actionLabel && (
          <button
            type="button"
            className="rmra-toast__action"
            onClick={onAction}
          >
            {actionLabel}
          </button>
        )}
      </div>
      {onClose && (
        <button
          type="button"
          className="rmra-toast__close"
          aria-label="Dismiss"
          onClick={onClose}
        >
          <X size={16} />
        </button>
      )}
    </div>
  );
}
