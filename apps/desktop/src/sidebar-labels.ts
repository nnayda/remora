/**
 * Accessible labels for the sidebar collapse/expand toggles. Shared so the
 * producing IconButtons (Sidebar / CollapsedRail) and the consuming focus
 * hand-off query in App.tsx can never drift — a rename here updates both the
 * rendered `aria-label` and the `button[aria-label="…"]` selector at once.
 */
export const SIDEBAR_EXPAND_LABEL = "Expand sidebar";
export const SIDEBAR_COLLAPSE_LABEL = "Collapse sidebar";
