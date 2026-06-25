import "./Tag.css";

/**
 * Monospace metadata chip — branch names, model labels, paths, tokens.
 * Mono face signals "machine value" and pairs with the terminal.
 */
export interface TagProps extends React.HTMLAttributes<HTMLSpanElement> {
  /** Leading icon node. */
  icon?: React.ReactNode;
  /** "branch" tints the tag with the accent for git refs. @default "default" */
  variant?: "default" | "branch";
  /** When provided, renders a removable × affordance. */
  onRemove?: (e: React.MouseEvent) => void;
  children?: React.ReactNode;
}

export function Tag({
  children,
  icon = null,
  variant = "default",
  onRemove,
  className = "",
  ...props
}: TagProps) {
  const cls = ["rmra-tag", `rmra-tag--${variant}`, className]
    .filter(Boolean)
    .join(" ");
  return (
    <span className={cls} {...props}>
      {icon && <span className="rmra-tag__icon">{icon}</span>}
      {children}
      {onRemove && (
        <button
          type="button"
          className="rmra-tag__close"
          onClick={onRemove}
          aria-label="Remove"
        >
          ×
        </button>
      )}
    </span>
  );
}
