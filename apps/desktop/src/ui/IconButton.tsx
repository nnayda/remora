import "./IconButton.css";

/**
 * Square, icon-only button for toolbars, tab actions, and panel headers.
 * Always pass a `label` for accessibility (also used as the tooltip title).
 */
export interface IconButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  /** Control size. @default "md" */
  size?: "sm" | "md" | "lg";
  /** Toggled / selected state — paints with the accent tint. @default false */
  active?: boolean;
  /** Accessible name + tooltip title. */
  label?: string;
  disabled?: boolean;
  /** Icon node (a single <svg>). */
  children?: React.ReactNode;
  /** Forwarded to the underlying <button> (React 19 ref-as-prop). */
  ref?: React.Ref<HTMLButtonElement>;
}

export function IconButton({
  children,
  size = "md",
  active = false,
  disabled = false,
  label,
  className = "",
  ...props
}: IconButtonProps) {
  const cls = [
    "rmra-iconbtn",
    `rmra-iconbtn--${size}`,
    active ? "rmra-iconbtn--active" : "",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <button
      className={cls}
      aria-label={label}
      title={label}
      disabled={disabled}
      {...props}
    >
      {children}
    </button>
  );
}
