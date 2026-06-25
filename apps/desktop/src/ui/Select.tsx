import "./Select.css";

export type SelectOption = string | { value: string; label: string };

/**
 * Native-backed select styled to the system. Use for agent pick, model pick,
 * host/region — short, known option sets.
 */
export interface SelectProps
  extends Omit<
    React.SelectHTMLAttributes<HTMLSelectElement>,
    "onChange" | "value"
  > {
  /** Field label. */
  label?: string;
  /** Options as strings or {value,label}. */
  options?: SelectOption[];
  value?: string;
  /** (value: string, e) => void */
  onChange?: (value: string, e: React.ChangeEvent<HTMLSelectElement>) => void;
  /** Monospace option text (e.g. model IDs). @default false */
  mono?: boolean;
  disabled?: boolean;
}

export function Select({
  label,
  options = [],
  value,
  onChange,
  mono = false,
  disabled = false,
  className = "",
  ...props
}: SelectProps) {
  const cls = ["rmra-select", mono ? "rmra-select--mono" : "", className]
    .filter(Boolean)
    .join(" ");
  return (
    <label className={cls}>
      {label && <span className="rmra-select__label">{label}</span>}
      <span className="rmra-select__control">
        <select
          value={value}
          disabled={disabled}
          onChange={(e) => onChange?.(e.target.value, e)}
          {...props}
        >
          {options.map((o) => {
            const opt = typeof o === "string" ? { value: o, label: o } : o;
            return (
              <option key={opt.value} value={opt.value}>
                {opt.label}
              </option>
            );
          })}
        </select>
        <span className="rmra-select__chev">
          <svg viewBox="0 0 12 12" aria-hidden="true">
            <polyline points="2.5,4.5 6,8 9.5,4.5" />
          </svg>
        </span>
      </span>
    </label>
  );
}
