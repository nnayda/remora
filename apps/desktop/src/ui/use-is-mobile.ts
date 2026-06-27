import { useEffect, useState } from "react";

/** Matches the app.css single-pane breakpoint. Keep in sync with app.css. */
const MOBILE_QUERY = "(max-width: 680px)";

/** True when the viewport is at/below the mobile single-pane breakpoint. */
export function useIsMobile(): boolean {
  const [isMobile, setIsMobile] = useState(
    () =>
      typeof window !== "undefined" && window.matchMedia(MOBILE_QUERY).matches,
  );
  useEffect(() => {
    const mql = window.matchMedia(MOBILE_QUERY);
    const onChange = (e: MediaQueryListEvent) => setIsMobile(e.matches);
    mql.addEventListener("change", onChange);
    setIsMobile(mql.matches);
    return () => mql.removeEventListener("change", onChange);
  }, []);
  return isMobile;
}
