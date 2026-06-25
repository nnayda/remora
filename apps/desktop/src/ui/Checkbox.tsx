import { useEffect, useRef } from "react";
import "./Checkbox.css";

/**
 * Checkbox with optional label + description and indeterminate state.
 * `onChange` receives the boolean directly.
 */
export interface CheckboxProps
  extends Omit<
    React.InputHTMLAttributes<HTMLInputElement>,
    "onChange" | "checked"
  > {
  /** @default false */
  checked?: boolean;
  /** Tri-state dash (overrides the checkmark visually). @default false */
  indeterminate?: boolean;
  /** (next: boolean, e) => void */
  onChange?: (checked: boolean, e: React.ChangeEvent<HTMLInputElement>) => void;
  /** Primary label text. */
  label?: string;
  /** Secondary description under the label. */
  description?: string;
  disabled?: boolean;
}

export function Checkbox({
  checked = false,
  indeterminate = false,
  onChange,
  label,
  description,
  disabled = false,
  className = "",
  ...props
}: CheckboxProps) {
  // The DOM `indeterminate` state is a property, not an attribute — set it on the
  // node so assistive tech announces "mixed", matching the dash visual.
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = indeterminate;
  }, [indeterminate]);
  const cls = [
    "rmra-check",
    checked || indeterminate ? "rmra-check--on" : "",
    disabled ? "rmra-check--disabled" : "",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <label className={cls}>
      <input
        ref={ref}
        type="checkbox"
        checked={checked}
        disabled={disabled}
        aria-checked={indeterminate ? "mixed" : checked}
        onChange={(e) => onChange?.(e.target.checked, e)}
        {...props}
      />
      <span className="rmra-check__box">
        {indeterminate ? (
          <svg viewBox="0 0 12 12" aria-hidden="true">
            <line x1="2.5" y1="6" x2="9.5" y2="6" />
          </svg>
        ) : (
          <svg viewBox="0 0 12 12" aria-hidden="true">
            <polyline points="2,6.5 5,9 10,3" />
          </svg>
        )}
      </span>
      {label && (
        <span className="rmra-check__label">
          {label}
          {description && (
            <span className="rmra-check__sub">{description}</span>
          )}
        </span>
      )}
    </label>
  );
}
