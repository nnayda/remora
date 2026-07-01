import { type RefObject, useEffect, useState } from "react";
import type { ActivityState } from "./activity-store";
import { reorderTabs, type Tab } from "./session-store";
import { tabIndicatorState } from "./status-state";
import { IconButton, SessionTab, Tooltip } from "./ui";
import { PanelRight, Plus } from "./ui/icons";

interface TabBarProps {
  tabs: Tab[];
  activeKey: string | null;
  activity: ReadonlyMap<string, ActivityState>;
  onFocus: (key: string) => void;
  onClose: (key: string) => void;
  onReorder: (key: string, targetKey: string) => void;
  onNew: () => void;
  newButtonRef: RefObject<HTMLButtonElement | null>;
  panelOpen: boolean;
  onTogglePanel: () => void;
}

/** Thin shell: renders tabs from the store snapshot, emits callbacks. */
export function TabBar({
  tabs,
  activeKey,
  activity,
  onFocus,
  onClose,
  onReorder,
  onNew,
  newButtonRef,
  panelOpen,
  onTogglePanel,
}: TabBarProps) {
  // Drag-to-reorder is a transient, view-only interaction, so it lives in local
  // state; the committed order lives in the store (onReorder). `dragKey` is the
  // tab being dragged, `overKey` the tab currently under the cursor.
  const [dragKey, setDragKey] = useState<string | null>(null);
  const [overKey, setOverKey] = useState<string | null>(null);
  const resetDrag = () => {
    setDragKey(null);
    setOverKey(null);
  };
  // Commit the previewed order to the store. `onReorder(dragKey, overKey)` runs
  // the same move (`reorderTabs`) the preview below rendered, so what the user
  // sees during the drag is exactly what lands on drop.
  const commitDrag = () => {
    if (dragKey && overKey && dragKey !== overKey) onReorder(dragKey, overKey);
    resetDrag();
  };
  // preventDefault + dropEffect="move" marks the element as a valid drop target
  // so the native HTML5 DnD cursor reads "move" rather than the "+" copy glyph.
  const allowMove = (e: React.DragEvent) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
  };
  // Safety net: if the dragged tab leaves the bar mid-drag (closed/removed by a
  // user action or a backend event), `dragend` may never fire on its now-gone
  // node, leaving drag state stuck. Clear it once the key falls out of `tabs`.
  useEffect(() => {
    if (dragKey && !tabs.some((t) => t.key === dragKey)) {
      setDragKey(null);
      setOverKey(null);
    }
  }, [dragKey, tabs]);
  // Live reflow: while dragging, render the tabs in their *previewed* order so
  // the other tabs shuffle out of the way and the dragged tab (dimmed) slides
  // into its prospective slot in real time — instead of a static drop indicator.
  const order = dragKey && overKey ? reorderTabs(tabs, dragKey, overKey) : tabs;
  return (
    <div className="rk-tabbar" role="tablist" aria-label="Sessions">
      {/* Strip-level handlers accept the "move" everywhere in the bar — over the
          dragged tab itself and the gaps past the last tab — so the native HTML5
          DnD cursor reads as "move" rather than the stray "+" copy affordance.
          This container `onDrop` is the SOLE commit point: `drop` bubbles up from
          the tab under the cursor, so one drop fires exactly one `commitDrag`.
          Do NOT add a per-tab `onDrop` — it would bubble here and double-apply
          the (non-idempotent) reorder, landing the tab in the wrong slot. */}
      {/* biome-ignore lint/a11y/noStaticElementInteractions: pointer drag-drop surface, not a keyboard target — reorder has no non-drag semantics to expose via role */}
      <div
        className="rk-tabbar__tabs"
        onDragOver={(e) => {
          if (!dragKey) return;
          allowMove(e);
        }}
        onDrop={(e) => {
          if (!dragKey) return;
          e.preventDefault();
          commitDrag();
        }}
      >
        {order.map((t) => {
          const active = t.key === activeKey;
          return (
            <SessionTab
              key={t.key}
              label={t.sessionId}
              state={tabIndicatorState(t.status, activity.get(t.key))}
              active={active}
              role="tab"
              aria-selected={active}
              // Full project/session key on hover — disambiguates two sessions
              // with the same slug under different projects.
              title={t.key}
              // Status is shown as an indicator (aria-hidden); fold it into the
              // accessible name so screen-reader users hear reconnecting/
              // stopped/disconnected, not just the session slug.
              aria-label={
                t.status === "live" ? undefined : `${t.sessionId}, ${t.status}`
              }
              onClick={() => onFocus(t.key)}
              onClose={() => onClose(t.key)}
              dragging={dragKey === t.key}
              drag={{
                draggable: true,
                onDragStart: (e) => {
                  setDragKey(t.key);
                  e.dataTransfer.effectAllowed = "move";
                  // Firefox only starts a drag once data is set.
                  e.dataTransfer.setData("text/plain", t.key);
                },
                onDragOver: (e) => {
                  if (!dragKey || dragKey === t.key) return;
                  allowMove(e); // mark this tab a valid drop target
                  // Drive the live reflow: this tab is now under the cursor, so
                  // the dragged tab previews into its slot (see `order` above).
                  // Drops commit via the container `onDrop` (this event bubbles).
                  // Whole-tab hover, no midpoint hysteresis — can flicker at
                  // boundaries; damping is deferred (see #209).
                  if (overKey !== t.key) setOverKey(t.key);
                },
                onDragEnd: resetDrag,
              }}
            />
          );
        })}
      </div>
      <div className="rk-tabbar__actions">
        <Tooltip content="New session" side="bottom">
          <IconButton ref={newButtonRef} label="New session" onClick={onNew}>
            <Plus />
          </IconButton>
        </Tooltip>
        <Tooltip content="Files & diff" kbd="⌘\" side="bottom">
          <IconButton
            label="Toggle files & diff"
            active={panelOpen}
            onClick={onTogglePanel}
          >
            <PanelRight />
          </IconButton>
        </Tooltip>
      </div>
    </div>
  );
}
