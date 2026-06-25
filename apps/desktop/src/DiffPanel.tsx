import { useState } from "react";
import { IconButton } from "./ui";
import { X } from "./ui/icons";

type Tab = "files" | "diff" | "pr";

const TABS: { id: Tab; label: string }[] = [
  { id: "files", label: "Files" },
  { id: "diff", label: "Diff" },
  { id: "pr", label: "PR" },
];

/**
 * Right-side peek panel for reviewing a session's work — file list, unified
 * diff, and PR view. Currently a styled shell with an empty state only; the
 * file/diff surfaces light up once real data is wired in.
 */
export function DiffPanel({ onClose }: { onClose: () => void }) {
  const [tab, setTab] = useState<Tab>("diff");

  return (
    <aside className="rk-panel">
      <div className="rk-panel__head">
        <div className="rk-panel__tabs">
          {TABS.map((t) => (
            <button
              key={t.id}
              type="button"
              className={`rk-panel__tab${tab === t.id ? " rk-panel__tab--on" : ""}`}
              onClick={() => setTab(t.id)}
            >
              {t.label}
            </button>
          ))}
        </div>
        <IconButton label="Close panel" size="sm" onClick={onClose}>
          <X size={15} />
        </IconButton>
      </div>
      <div className="rk-panel__empty">
        <div className="rk-panel__empty-title">No changes to show yet.</div>
        <div className="rk-panel__empty-sub">
          File and diff views light up once this session has work to review.
        </div>
      </div>
    </aside>
  );
}
