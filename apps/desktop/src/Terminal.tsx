import { useEffect, useRef } from "react";
import type { SessionConnection } from "./connection";
import { TerminalController } from "./terminal-controller";

/** Thin React shell: own a div, construct/dispose a TerminalController on it. */
export function Terminal({ connection }: { connection: SessionConnection }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const controller = new TerminalController(el, connection);
    return () => controller.dispose();
  }, [connection]);
  return <div ref={ref} className="terminal" />;
}
