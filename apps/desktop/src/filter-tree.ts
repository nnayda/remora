import type { ProjectNode } from "./session-tree";

/** Filter the flat project list client-side by case-insensitive substring over
 * project label, host label, and sessionId. A project whose label or host label
 * matches keeps all of its sessions; otherwise only matching sessions survive
 * and the project drops if none match. A project with unchanged sessions is
 * returned by reference (not a fresh object), avoiding a needless allocation —
 * and ready to enable `React.memo` skipping if `ProjectGroup` is memoized later
 * (it isn't today, so this is a cheap correctness nicety, not an active win).
 * Empty query → the tree unchanged. Pure (no DOM) — unit-tested. */
export function filterTree(tree: ProjectNode[], query: string): ProjectNode[] {
  const q = query.trim().toLowerCase();
  if (!q) return tree;
  const out: ProjectNode[] = [];
  for (const project of tree) {
    const projMatch =
      project.label.toLowerCase().includes(q) ||
      project.hostLabel.toLowerCase().includes(q);
    const sessions = projMatch
      ? project.sessions
      : project.sessions.filter((s) => s.sessionId.toLowerCase().includes(q));
    if (projMatch || sessions.length > 0) {
      out.push(
        sessions === project.sessions ? project : { ...project, sessions },
      );
    }
  }
  return out;
}
