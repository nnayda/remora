import { useCallback, useEffect, useState } from "react";

export type Theme = "system" | "light" | "dark";

const STORAGE_KEY = "remora-theme";
/** system(undefined) → light → dark → system → ... */
const ORDER: Theme[] = ["system", "light", "dark"];

function readStored(): Theme {
  if (typeof window === "undefined") return "system";
  const v = window.localStorage.getItem(STORAGE_KEY);
  return v === "light" || v === "dark" ? v : "system";
}

/** Reflect a theme onto `documentElement.dataset.theme` + localStorage.
 * "system" removes both (so `prefers-color-scheme` drives the tokens). */
function applyTheme(theme: Theme): void {
  if (typeof document === "undefined") return;
  const root = document.documentElement;
  if (theme === "system") {
    delete root.dataset.theme;
    if (typeof window !== "undefined") {
      window.localStorage.removeItem(STORAGE_KEY);
    }
  } else {
    root.dataset.theme = theme;
    if (typeof window !== "undefined") {
      window.localStorage.setItem(STORAGE_KEY, theme);
    }
  }
}

/** Self-contained theme toggle: cycles system → light → dark, persisting the
 * choice and applying the saved value on mount. */
export function useTheme(): { theme: Theme; cycle: () => void } {
  const [theme, setTheme] = useState<Theme>(readStored);

  // Apply the saved value on mount.
  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  const cycle = useCallback(() => {
    setTheme((prev) => ORDER[(ORDER.indexOf(prev) + 1) % ORDER.length]);
  }, []);

  return { theme, cycle };
}
