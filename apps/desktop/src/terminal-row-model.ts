import type { DetectedTerminalDto } from "./bindings";
import type { TerminalPreference } from "./external-terminal";

/** What the Settings row renders. `custom` is deliberately read-only: the
 * dropdown only ever writes the registry-id form (spec YAGNI cut), and a
 * hand-authored argv must never be clobbered by a Settings save. */
export type TerminalRowModel =
  | {
      mode: "select";
      current: string | null;
      options: DetectedTerminalDto[];
      hint: string;
    }
  | { mode: "custom"; display: string };

export function terminalRowModel(
  pref: TerminalPreference,
  detected: DetectedTerminalDto[],
): TerminalRowModel {
  if (Array.isArray(pref)) {
    return {
      mode: "custom",
      display: `Custom (config file): ${pref.join(" ")}`,
    };
  }
  const hint =
    detected.length === 0
      ? "none detected — install one or set a custom command in the config file"
      : `${detected.length} terminal${detected.length === 1 ? "" : "s"} detected`;
  return { mode: "select", current: pref, options: detected, hint };
}
