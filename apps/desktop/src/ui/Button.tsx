import "./Button.css";

/**
 * Primary action control for Remora chrome. Quiet by default; the signature
 * accent appears on the primary variant and focus ring only.
 */
export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  /** Visual weight. @default "primary" */
  variant?: "primary" | "secondary" | "ghost" | "danger";
  /** Control height. @default "md" */
  size?: "sm" | "md" | "lg";
  /** Leading icon node (e.g. a Lucide <svg>). */
  icon?: React.ReactNode;
  /** Trailing icon node. */
  iconRight?: React.ReactNode;
  /** Show inline spinner and disable. @default false */
  loading?: boolean;
  /** Stretch to container width. @default false */
  fullWidth?: boolean;
  disabled?: boolean;
  children?: React.ReactNode;
  /** Forwarded to the underlying <button> (React 19 ref-as-prop). */
  ref?: React.Ref<HTMLButtonElement>;
}

export function Button({
  children,
  variant = "primary",
  size = "md",
  icon = null,
  iconRight = null,
  loading = false,
  fullWidth = false,
  disabled = false,
  className = "",
  ...props
}: ButtonProps) {
  const cls = [
    "rmra-btn",
    `rmra-btn--${variant}`,
    `rmra-btn--${size}`,
    fullWidth ? "rmra-btn--full" : "",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <button className={cls} disabled={disabled || loading} {...props}>
      {loading && <span className="rmra-btn__spin" />}
      {!loading && icon}
      {children && <span>{children}</span>}
      {!loading && iconRight}
    </button>
  );
}
