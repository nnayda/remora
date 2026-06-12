# 0004. Configure hosts and projects locally, discover sessions from the sandbox

- **Status:** Accepted
- **Date:** 2026-06-12
- **Issue/PR:** —

## Context

A user runs agents on more than one sandbox (a k8s pod here, a VPS there),
works on several repos per sandbox, and spawns several sessions per repo.
The app needs a model for configuring and organizing all of that — and it
has to survive the hero scenario: a session launched from one device must
appear on every other device, mid-task, with zero infrastructure
([VISION.md](../VISION.md)).

Two kinds of state are in tension. How to *reach* a sandbox is inherently
client-side — there is a bootstrap problem: no sandbox can tell you how to
connect to it. What is *running* is inherently sandbox-side — tmux owns the
live processes ([ADR-0001](0001-tmux-session-persistence.md)), and any
client-side copy of that list goes stale the moment another device acts.

## Decision

We will model the world as **Host → Project → Session** and split state
along the line above:

- **Hosts and projects are declarative local configuration** — one
  human-editable TOML file per device (`~/.config/remora/config.toml` or
  platform equivalent). A *host* is a transport (`ssh` | `kubectl`,
  `docker` planned) plus its parameters; a *project* is a directory on
  that host with a workspace mode and a default agent (overridable per
  session). Agent launch commands live in the same file as adapter data,
  per [ADR-0003](0003-agent-agnostic-sessions.md).
- **Sessions are never stored in any client.** Clients discover them by
  listing tmux sessions on each host and joining them back to projects
  through a naming convention — `remora_<project-id>_<session-id>` —
  with metadata that doesn't fit in the name (agent, creation time,
  worktree path) kept in tmux session environment variables. Config `id`s
  are therefore stable join keys: immutable once created.
- **Projects declare a workspace mode.** `worktree` (git repos: each
  session gets a fresh worktree + branch; surviving worktrees with no tmux
  session surface as *stopped*, with one-click respawn after a pod
  restart) or `shared` (plain directories: sessions share the path, no
  isolation).

The sidebar renders this join — config tree × live discovery — with
host/session state at a glance. The full design (config schema, launch
sequence, states, UX) lives in the working spec; the durable parts are
reflected in [ARCHITECTURE.md](../ARCHITECTURE.md).

## Alternatives considered

- **Local session registry** (app records what it launches, checks
  liveness): lies whenever anything happens outside the app — another
  device spawns a session, a pod restarts. Reconciliation gets written
  anyway, but against two sources of truth instead of one.
- **Launcher + recent-sessions list** (the VS Code Remote / JetBrains
  Gateway shape): the established pattern for one-connection-at-a-time
  tools; Remora's pitch is many concurrent sessions across hosts at a
  glance, so the persistent tree is the product.
- **Project config on the sandbox** (every device discovers the same
  projects): connections must stay local regardless (bootstrap), splits
  config across two places, and puts Remora files on the sandbox in
  tension with "nothing custom on the sandbox".
- **Do nothing** (sessions only, no organizing model): the sidebar and
  multi-sandbox workflows have nothing to hang off; every spawn becomes a
  form to fill in.

## Consequences

What becomes easier:

- Reconnect-from-anywhere needs no sync protocol: same host config on two
  devices ⇒ same discovered sessions. The future relay hosts the same
  discovery behind a WebSocket without a new model.
- Config is dotfiles-friendly and hand-editable; the app watches the file,
  and a parse failure never causes a rewrite (last good config stays live).
- Pod restarts degrade gracefully: tmux dies, worktrees survive, sessions
  reappear as *stopped* + respawnable — answering part of the
  worktree-hygiene open question in VISION.md.

What becomes harder, and what we are committed to:

- The tmux naming convention and session-environment metadata are now wire
  format: versioned conventions, round-trip tested in `remora-core`, and
  changed only compatibly (old sessions must still be recognized).
- Config ids are immutable once created; the app must never offer id
  editing (renames touch only display names).
- Discovery cost scales with configured hosts (a `tmux ls` per host on
  connect/refresh); host state must degrade to `unreachable` without
  hammering dead transports.
- Cross-device *config* sync (as opposed to session visibility) is
  explicitly deferred to the relay milestone.
