import "./Avatar.css";

/**
 * Identity glyph for hosts, projects, and users. Falls back to initials on a
 * deterministic tint derived from `name`. Use `host` for infra (neutral chip).
 */
export interface AvatarProps extends React.HTMLAttributes<HTMLSpanElement> {
  /** Display name — drives initials and fallback tint. */
  name?: string;
  /** Image URL; when present, overrides initials. */
  src?: string | null;
  /** @default "md" */
  size?: "sm" | "md" | "lg";
  /** @default "rounded" */
  shape?: "rounded" | "circle";
  /** Neutral infra styling for hosts/sandboxes. @default false */
  host?: boolean;
}

const TINTS = [
  "var(--marine-500)",
  "var(--blue-500)",
  "var(--green-500)",
  "var(--amber-500)",
  "#C77DD8",
  "#4FC4C9",
];

function tintFor(seed: string) {
  let h = 0;
  for (let i = 0; i < (seed || "").length; i++)
    h = (h * 31 + seed.charCodeAt(i)) >>> 0;
  return TINTS[h % TINTS.length];
}

export function Avatar({
  name = "",
  src = null,
  size = "md",
  shape = "rounded",
  host = false,
  className = "",
  ...props
}: AvatarProps) {
  const initials = name
    .split(/\s+/)
    .map((w) => w[0])
    .slice(0, 2)
    .join("")
    .toUpperCase();
  const cls = [
    "rmra-avatar",
    `rmra-avatar--${size}`,
    shape === "circle" ? "rmra-avatar--circle" : "",
    host ? "rmra-avatar--host" : "",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  const style = !src && !host ? { background: tintFor(name) } : undefined;
  return (
    <span className={cls} style={style} {...props}>
      {src ? <img src={src} alt={name} /> : initials}
    </span>
  );
}
