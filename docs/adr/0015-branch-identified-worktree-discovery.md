# 0015. Identify worktree sessions by branch, discover by path

- **Status:** Accepted
- **Date:** 2026-06-26
- **Issue/PR:** #124

## Context

[ADR-0004](0004-local-config-live-session-discovery.md) established a worktree path/branch naming convention as the discovery contract: sessions are stored at `~/.remora/worktrees/<project-id>/<session-id>` with the branch name `remora/<session-id>`. This ties the session identity (the slug) to the path, and the path to the branch — there is no flexibility to choose the branch name or worktree location.

Issue #124 surfaces a user need: spawn a session into a branch the user chooses (e.g., a feature branch name or a device label) and place the worktree at a location they control. The fixed convention forbids both — there is no way to map an arbitrary branch name back to a `session_id` for discovery, and the worktree path is hardcoded.

## Decision

We will make the **branch name the session identity** and discover sessions by joining the live branch (from `git worktree list`) to the canonical worktree path.

Concretely:

1. **The branch is the session identity and display name.** Instead of deriving a session id as a slug at spawn and naming the branch `remora/<session-id>`, the user chooses the branch name (or the app chooses a default), and the `session_id` is derived from the branch (Mechanism A in the codebase: `derive_session_id(branch: Option<&str>) -> Option<SessionId>`). This keeps trait signatures unchanged — `SessionSource` and `SessionMeta` remain parameterized by `session_id`, not branch — because the branch is resolved to a `session_id` locally before any transport call.

2. **Discovery joins on the canonical worktree path.** A worktree session's identity is anchored in its filesystem path. Worktree enumeration is driven by the user's configured projects (`config.projects`): discovery runs `git worktree list --porcelain` per configured project. The environment variable `REMORA_WORKSPACE` (set at spawn, holding the worktree path) is the join key: it identifies which porcelain-listed worktree path a given live tmux session corresponds to, matched on the canonicalized absolute path. The worktree path is immutable across device boundaries and pod restarts, and the branch is stored in git's metadata, not in tmux environment variables or config.

3. **The primary checkout surfaces as a Shared session.** The primary checkout is detected by path equality — the worktree whose path equals the project's configured `path`. Its `session_id` is derived from whatever branch happens to be checked out there (e.g., `main`, `develop`, `trunk`), exactly like any other worktree — there is no reserved branch name. It is surfaced as a Shared session, allowing operations on the base repo without requiring a separate worktree.

4. **Discovery surfaces every worktree of a configured project.** The discovery scan no longer looks only for tmux sessions matching a fixed naming pattern; it queries `git worktree list` directly for every project configured in the user's `config.toml`, scoped by `config.projects`. This gives a whole-workspace view: all worktrees (including orphaned ones) appear as sessions (stopped or live, depending on whether a tmux session exists), and the user can respawn or clean them up.

## Carve-out: Sandbox reads for respawn/teardown

Respawn (`run_respawn`) and teardown (`run_remove`) build `git` and `tmux` commands using the worktree path and branch names read from the authoritative `git worktree list` output. This is a deliberate, bounded softening of [ADR-0004](0004-local-config-live-session-discovery.md)'s principle that "nothing read from the sandbox builds a command."

**Why:** the worktree path and branch are immutable, git-managed state — not user-forged session names or metadata. Reading them from the source of truth (git, not tmux environment variables) is safer and more resilient than storing them in config at spawn. The precedent is [ADR-0009 / issue #52](0009-dynamic-kubectl-field-resolution.md), which resolved kubectl host fields from a local shell command (`{command}` substitution) — a similar trust decision for an immutable, locally-computed value.

**Scope:** only the path and branch are read; respawn always plans worktree mode (enforced by `plan_spawn`'s guard on `plan.branch`), and teardown probes real state (`test -d`) before operating, consistent with ADR-0004's untrusted-metadata invariant.

## Alternatives considered

- **Store the branch in tmux environment metadata** (like the old `remora/<session-id>` convention): survives pod restarts but requires the tmux session to exist. Orphaned worktrees (no tmux session) lose their branch identity and become undiscoverable. Rejected; the path-based join is more durable.

- **Require the fixed `remora/<branch>` naming and don't allow user branches.** Keeps the discovery contract unchanged but defeats the feature request. Rejected.

- **Store branch + path in config at spawn.** Defeats the whole-workspace view (only spawned sessions appear); orphaned worktrees are invisible until added back to config. Rejected.

- **Do nothing** (keep ADR-0004's contract). Blocks user branch selection and custom worktree locations.

## Consequences

What becomes easier:

- Users can now choose branch names, enabling workflows like feature branches, device labels, or personal naming schemes.
- Users can place worktrees at custom paths (via `REMORA_WORKSPACE` expansion), unlocking faster I/O on different filesystems or external drives.
- The whole-workspace view surfaces all worktrees, including orphaned ones; cleanup and housekeeping become intentional rather than deferred.
- Respawn is more resilient: the branch and path are recovered from git's authoritative state, not stale tmux metadata.

What becomes harder, and what we are committed to:

- **The path/branch convention is superseded as the discovery contract.** ADR-0004's `remora/<session-id>` naming convention is no longer the wire format for discovery; worktree paths and branches are now read from git. Old sessions (with branches matching `remora/<session-id>`) will still be discoverable (their branches map to `session_id`s via `derive_session_id`), but new sessions spawned with user-chosen branches do not follow the old pattern.

- **Detached-HEAD worktrees are nameless.** A worktree checked out at a detached HEAD (no branch) does not map to a `session_id` and is invisible to discovery. This is rare but possible (e.g., if the user manually checks out a commit). The app should warn or prevent detached-HEAD worktrees at spawn.

- **Provenance is no longer tracked.** The old convention — branch name encodes the session id — provided implicit auditability: a branch name like `remora/abc123` signals it was managed by Remora. New user-chosen branches have no such marker. Confirmation for destructive operations (e.g., removing a worktree) is deferred to PR B and becomes more important.

- **Discovery cost grows with the number of configured projects.** The scan now queries `git worktree list` for every project on every host, not just tmux sessions. This is a small cost per project (one git command, subsecond), but scales linearly; host state must still degrade gracefully on unreachable transports.

## Update (PR B1)

PR B1 implements the spawn-time override of `branch` and `worktree_root`:

1. **`SpawnSpec` gains `branch` and `worktree_root` fields**. `worktree_root` cascades: per-session overrides (supplied at spawn) flow to per-project defaults to per-host defaults to the convention `~/.remora/worktrees/<project-id>`. `branch` is chosen per-session at spawn, with no project or host default. When `branch` is `None`, the worktree is created at the back-compat path `~/.remora/worktrees/<project>/<session_id>` with branch `remora/<session_id>` (exactly as pre-B1); when `branch` is supplied, the worktree is created at `<worktree_root>/<branch>` with the supplied branch name.

2. **`derive_session_id` moves from `DefaultHasher` to FNV-1a/32**, making the id stable across Rust rebuilds and reproducible in TypeScript (B2's client-side session minting). This closes the hasher item of #153.

3. **Teardown (`run_remove`) and respawn (`run_respawn`) source the real worktree path and branch from authoritative `git worktree list`**, matched by `derive_session_id`. This is the "Sandbox reads for respawn/teardown" carve-out already specified above: the primary checkout (path equals project's `path`) is never `worktree remove`'d or `branch -D`'d — its "remove" only closes the tmux session (A2′ safety).

4. **Backward compatibility:** `branch == None` at spawn reproduces the exact pre-B1 behavior (convention path, `remora/<session_id>` branch naming), so B1 is safe to land before the desktop UI (B2, which brings the "Branch name" + `worktree_root` form fields and client-side session id minting).
