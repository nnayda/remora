import type { RefObject } from "react";
import type { Tab } from "./session-store";

interface TabBarProps {
  tabs: Tab[];
  activeKey: string | null;
  onFocus: (key: string) => void;
  onClose: (key: string) => void;
  onNew: () => void;
  newButtonRef: RefObject<HTMLButtonElement | null>;
}

/** Thin shell: renders tabs from the store snapshot, emits callbacks. */
export function TabBar({
  tabs,
  activeKey,
  onFocus,
  onClose,
  onNew,
  newButtonRef,
}: TabBarProps) {
  return (
    <div className="tabbar" role="tablist" aria-label="Sessions">
      {tabs.map((t) => {
        const label = t.key; // `${projectId}/${sessionId}` — see tabKey()
        const active = t.key === activeKey;
        return (
          <div
            key={t.key}
            role="presentation"
            className={active ? "tab tab--active" : "tab"}
          >
            <button
              type="button"
              role="tab"
              aria-selected={active}
              className="tab-label"
              onClick={() => onFocus(t.key)}
            >
              <span
                className={`tab-status tab-status--${t.status}`}
                aria-hidden="true"
              />
              {label}
            </button>
            <button
              type="button"
              className="tab-close"
              aria-label={`Close ${label}`}
              onClick={() => onClose(t.key)}
            >
              ×
            </button>
          </div>
        );
      })}
      <button
        type="button"
        ref={newButtonRef}
        className="tab-new"
        onClick={onNew}
      >
        + New session
      </button>
    </div>
  );
}
