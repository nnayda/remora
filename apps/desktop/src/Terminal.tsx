import { forwardRef, useEffect, useImperativeHandle, useRef } from "react";
import type { SessionConnection } from "./connection";
import { TerminalController } from "./terminal-controller";

/** Imperative handle exposed to parents so they can move keyboard focus into
 * the emulator (e.g. when this pane becomes the active session). */
export interface TerminalHandle {
  focus(): void;
}

/** Thin React shell: own a div, construct/dispose a TerminalController on it,
 * and expose a `focus()` handle so the parent can focus the emulator once this
 * pane is the visible one. */
export const Terminal = forwardRef<
  TerminalHandle,
  { connection: SessionConnection; sessionKey?: string }
>(function Terminal({ connection, sessionKey }, ref) {
  const elRef = useRef<HTMLDivElement>(null);
  const controllerRef = useRef<TerminalController | null>(null);
  useEffect(() => {
    const el = elRef.current;
    if (!el) return;
    const controller = new TerminalController(el, connection, undefined, {
      sessionKey,
    });
    controllerRef.current = controller;
    return () => {
      controller.dispose();
      controllerRef.current = null;
    };
  }, [connection, sessionKey]);
  useImperativeHandle(
    ref,
    () => ({
      focus: () => controllerRef.current?.focus(),
    }),
    [],
  );
  return <div ref={elRef} className="rk-term__host" />;
});
