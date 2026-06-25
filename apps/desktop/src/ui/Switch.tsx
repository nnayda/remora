import "./Switch.css";

/**
 * Binary toggle for settings and panel options. On = accent track.
 * `onChange` receives the boolean directly.
 */
export interface SwitchProps
  extends Omit<
    React.InputHTMLAttributes<HTMLInputElement>,
    "onChange" | "checked" | "size"
  > {
  /** Controlled checked state. @default false */
  checked?: boolean;
  /** (next: boolean, e) => void */
  onChange?: (checked: boolean, e: React.ChangeEvent<HTMLInputElement>) => void;
  /** Inline trailing label. */
  label?: string;
  /** @default "md" */
  size?: "sm" | "md";
  disabled?: boolean;
}

export function Switch({
  checked = false,
  onChange,
  label,
  size = "md",
  disabled = false,
  className = "",
  ...props
}: SwitchProps) {
  const cls = [
    "rmra-switch",
    `rmra-switch--${size}`,
    checked ? "rmra-switch--on" : "",
    disabled ? "rmra-switch--disabled" : "",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <label className={cls}>
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange?.(e.target.checked, e)}
        {...props}
      />
      <span className="rmra-switch__track">
        <span className="rmra-switch__thumb" />
      </span>
      {label && <span className="rmra-switch__label">{label}</span>}
    </label>
  );
}
