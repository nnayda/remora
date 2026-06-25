import type { RefObject } from "react";
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
  onNew,
  newButtonRef,
  panelOpen,
  onTogglePanel,
}: TabBarProps) {
  return (
    <div className="rk-tabbar" role="tablist" aria-label="Sessions">
      <div className="rk-tabbar__tabs">
        {tabs.map((t) => {
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
