import { type ReactNode, useId, useState } from "react";
import "./Input.css";

/**
 * Single-line text field with optional label, icons, hint, and error state.
 * Use `mono` for paths, commands, and machine values.
 */
export interface InputProps
  extends Omit<React.InputHTMLAttributes<HTMLInputElement>, "size"> {
  /** Field label above the control. */
  label?: string;
  /** Leading icon node. */
  icon?: ReactNode;
  /** Trailing icon node. */
  iconRight?: ReactNode;
  /** Helper text below the field. */
  hint?: string;
  /** Error message — paints the field danger and replaces the hint. */
  error?: string;
  /** Monospace input text for paths/commands. @default false */
  mono?: boolean;
  disabled?: boolean;
}

export function Input({
  label,
  icon,
  iconRight,
  hint,
  error,
  mono = false,
  disabled = false,
  className = "",
  ...props
}: InputProps) {
  const [focused, setFocused] = useState(false);
  const hintId = useId();
  const wrapCls = [
    "rmra-input-wrap",
    focused ? "rmra-input-wrap--focus" : "",
    mono ? "rmra-input-wrap--mono" : "",
    error ? "rmra-input-wrap--invalid" : "",
    disabled ? "rmra-input-wrap--disabled" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <label className={["rmra-field", className].filter(Boolean).join(" ")}>
      {label && <span className="rmra-field__label">{label}</span>}
      <span className={wrapCls}>
        {icon && <span className="rmra-input__icon">{icon}</span>}
        <input
          disabled={disabled}
          aria-invalid={error ? true : undefined}
          aria-describedby={hint || error ? hintId : undefined}
          onFocus={(e) => {
            setFocused(true);
            props.onFocus?.(e);
          }}
          onBlur={(e) => {
            setFocused(false);
            props.onBlur?.(e);
          }}
          {...props}
        />
        {iconRight && <span className="rmra-input__icon">{iconRight}</span>}
      </span>
      {(hint || error) && (
        <span
          id={hintId}
          className={[
            "rmra-field__hint",
            error ? "rmra-field__hint--error" : "",
          ]
            .filter(Boolean)
            .join(" ")}
        >
          {error || hint}
        </span>
      )}
    </label>
  );
}
