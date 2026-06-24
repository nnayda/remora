# 0008. Workspace mode is overridable per session, with effective mode discovered from real state

- **Status:** Accepted
- **Date:** 2026-06-24
- **Issue/PR:** [#34](https://github.com/remora-k8s/remora/issues/34)

## Context

[ADR-0004](0004-local-config-live-session-discovery.md) established that each project declares a workspace mode (worktree or shared), and that worktrees survive pod restarts as stopped sessions that can be respawned. A later feature request surfaced: users want to spawn a worktree session on a project with a shared default (or vice versa) — a one-time override, not changing the project's setting.

The catch: if a worktree session is spawned on a shared-default project, then later discovered (on another device, or after a pod restart), the discovery must recognize it as a worktree session, not fall back to the project's shared default. Otherwise, the session becomes invisible (no worktree to discover) and unremovable (no cleanup). This is the "silent leak" bug.

The core invariant from ADR-0004 — discovered state is untrusted, but builds commands exclusively from local config — still holds. The twist is that *effective* workspace mode must be discovered from real sandbox state (a surviving git worktree), not projected from config.

## Decision

We will add per-session workspace-mode override to `SpawnSpec` and compute each session's effective workspace mode from real sandbox state, rather than projecting it from project config.

Concretely:

1. **`SpawnSpec.workspace: Option<WorkspaceMode>`** allows the client to override the project's default workspace mode per spawn. The field is always serialized (mirrors `agent`). Missing keys deserialize to `None`, so older peers stay compatible without a `PROTOCOL_VERSION` bump.

2. **`WorkspaceMode` moves to `remora-protocol`** — it was in `remora-core::config` — so both `SpawnSpec` and `SessionMeta` can carry it. Core re-exports it for backward compatibility.

3. **`SessionMeta.workspace: Option<WorkspaceMode>`** is computed at discovery time from real sandbox state: if a surviving git worktree exists for the session, `workspace = Some(Worktree)`, otherwise `Some(Shared)`. This is true regardless of the project's config. The field is `Option` for forward compatibility with older senders (which emit `None`, and the client falls back to the project's configured mode).

4. **`plan_spawn` resolves effective mode** as `spec.workspace.unwrap_or(project.workspace)`: the override takes precedence, else the project's default.

5. **Teardown (`run_remove`) and respawn (`run_respawn`) re-probe real state** before operating. A `test -d` check on the worktree path verifies that a surviving worktree exists; commands are never built from discovered `SessionMeta` (consistent with ADR-0004's untrusted-metadata invariant). Respawn always plans worktree mode (enforced by `plan_spawn`'s guard on `plan.branch`).

## Alternatives considered

- **Thin spawn-time-only override** (store the override in config, not discovered state): effective mode reverts on the next discovery when another device polls, and a worktree session spawned on a shared-default project becomes undiscoverable. Rejected; it guts the feature.

- **Restrict override to the safe direction only** (allow shared-on-worktree, disallow worktree-on-shared): worktree sessions on shared-default projects are the more common and useful scenario (maximizing concurrency safety). Rejected; it guts the more valuable direction.

- **Persist a separate "intended mode" flag** (in config or metadata): deriving effective mode from real worktree existence is sufficient, non-forgeable (anyone with shell on the sandbox can create a worktree but not a symlink to one), and requires no new state machine. Rejected; simpler to derive.

- **Bump `PROTOCOL_VERSION`** (signal the addition of `workspace` fields): `SpawnSpec.workspace` and `SessionMeta.workspace` are optional fields that deserialize to `None` when absent, so older and newer peers can communicate without a version gate. No bump needed.

## Consequences

What becomes easier:

- Users can now spawn worktree sessions on shared-default projects (or vice versa) without changing the config, and the session persists across discovery and pod restart as the intended mode.
- The silent-leak bug closes: a worktree session spawned on a shared-default project is now discoverable (via the worktree check) and removable (teardown probes real state).

What becomes harder, and what we are committed to:

- `SessionMeta.workspace` is `Option`, so the client must have fallback logic: if `None` (older sender), use the project's configured mode. The fallback is safe because older senders were always config-first anyway (no override support).
- A session spawned in shared mode has no persistent worktree, so `plan_spawn` rejects respawn with `NotWorktreeProject`. The UI hides the Respawn button via `canRespawn(t.workspace)` (true only for `Worktree`). Shared-mode sessions remain effectively single-writer.
- The discovery scan (see ADR-0004) now runs for every project on every host, not just projects with a worktree default. Cost is small (one `git worktree list` call per project per host per refresh), but non-zero.
- Teardown and respawn now issue a `test -d` probe in addition to their main operation. This adds one round-trip per command, but is necessary to detect orphaned worktrees and avoid trusting stale discovered metadata for destructive operations.
