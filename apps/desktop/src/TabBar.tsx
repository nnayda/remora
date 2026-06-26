import { type RefObject, useEffect, useState } from "react";
import type { ActivityState } from "./activity-store";
import type { Tab } from "./session-store";
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
  const dragIndex = dragKey ? tabs.findIndex((t) => t.key === dragKey) : -1;
  const resetDrag = () => {
    setDragKey(null);
    setOverKey(null);
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
  return (
    <div className="rk-tabbar" role="tablist" aria-label="Sessions">
      <div className="rk-tabbar__tabs">
        {tabs.map((t, index) => {
          const active = t.key === activeKey;
          // The drop indicator sits on the side the dragged tab would land on:
          // trailing edge when dragging rightward, leading edge when leftward.
          // `dragIndex !== -1` guards the brief window where the dragged tab has
          // left `tabs` but the reset effect hasn't run yet (else every hovered
          // tab would wrongly show a trailing indicator).
          const isOver =
            overKey === t.key && dragKey !== null && dragIndex !== -1;
          const dropAfter = isOver && dragIndex < index;
          const dropBefore = isOver && dragIndex > index;
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
              dropBefore={dropBefore}
              dropAfter={dropAfter}
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
                  e.preventDefault(); // mark this tab a valid drop target
                  e.dataTransfer.dropEffect = "move";
                  if (overKey !== t.key) setOverKey(t.key);
                },
                onDrop: (e) => {
                  e.preventDefault();
                  if (dragKey && dragKey !== t.key) onReorder(dragKey, t.key);
                  resetDrag();
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
