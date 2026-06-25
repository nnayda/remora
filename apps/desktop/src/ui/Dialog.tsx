import { type ReactNode, useId } from "react";
import { X } from "./icons";
import "./Dialog.css";

/**
 * Centered modal for focused decisions — new session, confirm destructive
 * action, settings. Soft-rise entrance, scrim dismiss. Pass Buttons in `footer`.
 */
export interface DialogProps extends React.HTMLAttributes<HTMLDivElement> {
  /** Mounted only when true. @default true */
  open?: boolean;
  /** Heading. */
  title: string;
  /** Supporting description under the title. */
  description?: string;
  /** Leading header icon node. */
  icon?: ReactNode;
  /** Body content. */
  children?: ReactNode;
  /** Footer node — typically a row of Buttons. */
  footer?: ReactNode;
  /** Close handler (scrim click, ×, escape wiring is up to caller). */
  onClose?: () => void;
}

export function Dialog({
  open = true,
  title,
  description,
  icon,
  children,
  footer,
  onClose,
  className = "",
  ...props
}: DialogProps) {
  const titleId = useId();
  const descId = useId();
  if (!open) return null;
  return (
    <div className="rmra-dialog__scrim">
      {onClose && (
        // A real button as the backdrop: click-outside-to-close, keyboard-safe.
        // Kept out of the tab order (tabIndex -1) so the owner's focus trap runs
        // inside the panel; Esc is handled by the dialog owner.
        <button
          type="button"
          className="rmra-dialog__backdrop"
          aria-label="Close dialog"
          tabIndex={-1}
          onClick={onClose}
        />
      )}
      <div
        className={["rmra-dialog", className].filter(Boolean).join(" ")}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={description ? descId : undefined}
        {...props}
      >
        <div className="rmra-dialog__head">
          {icon && <span className="rmra-dialog__head-icon">{icon}</span>}
          <div className="rmra-dialog__head-text">
            <span className="rmra-dialog__title" id={titleId}>
              {title}
            </span>
            {description && (
              <span className="rmra-dialog__desc" id={descId}>
                {description}
              </span>
            )}
          </div>
          {onClose && (
            <button
              type="button"
              className="rmra-dialog__x"
              aria-label="Close"
              onClick={onClose}
            >
              <X size={18} />
            </button>
          )}
        </div>
        {children && <div className="rmra-dialog__body">{children}</div>}
        {footer && <div className="rmra-dialog__foot">{footer}</div>}
      </div>
    </div>
  );
}
