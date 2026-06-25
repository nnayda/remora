import "./Badge.css";

/**
 * Compact status / count label. Tones map to chrome semantics — never used
 * inside the terminal, where ANSI colors own status meaning.
 */
export interface BadgeProps extends React.HTMLAttributes<HTMLSpanElement> {
  /** Semantic tone. @default "neutral" */
  tone?: "neutral" | "accent" | "success" | "warning" | "danger" | "info";
  /** Show a leading status dot. @default false */
  dot?: boolean;
  /** Filled accent style (use sparingly). @default false */
  solid?: boolean;
  /** Numeric count style — tabular, not uppercased. @default false */
  count?: boolean;
  children?: React.ReactNode;
}

export function Badge({
  children,
  tone = "neutral",
  dot = false,
  solid = false,
  count = false,
  className = "",
  ...props
}: BadgeProps) {
  const cls = [
    "rmra-badge",
    `rmra-badge--${tone}`,
    solid ? "rmra-badge--solid" : "",
    count ? "rmra-badge--count" : "",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <span className={cls} {...props}>
      {dot && <span className="rmra-badge__dot" />}
      {children}
    </span>
  );
}
