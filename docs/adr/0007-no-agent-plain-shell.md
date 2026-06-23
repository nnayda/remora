# 0007. A session may run no agent — a plain shell

- **Status:** Accepted
- **Date:** 2026-06-21
- **Issue/PR:** #35

## Context

[ADR-0003](0003-agent-agnostic-sessions.md) defines a session as a tmux
process running *an agent command*. Some sessions want no agent at all — just
a terminal on the sandbox. Nothing represented "no agent": `Agent.command` was
a required non-empty argv and `new_session_tokens` always launched it.

## Decision

A plain shell is modeled as **data, not a new type**: an agent whose `command`
is the empty argv `[]`.

- Config validation accepts `command = []` (and still rejects blank elements in
  a *non-empty* command, and control characters).
- `plan_spawn` already clones `command` into `agent_argv`, so an empty command
  yields an empty argv with no further change. `REMORA_AGENT` still carries the
  agent id, so a live session round-trips through discovery.
- The shared `new_session_tokens` seam renders an empty argv as an explicit
  login shell `"${SHELL:-/bin/sh}" -l` — deterministic (independent of the host
  tmux's `default-command`) and identical to the shell the agent-exit fallback
  (#30/#44) drops to.

We rejected a three-valued `AgentSelection { Default, None, Agent }` enum
threaded through the protocol, both transports, the trait, the fake, and the
bridge (~15 files) in favor of this ~6-file, type-churn-free approach.

## Consequences

- There is no built-in "None (plain shell)" item in the new-session dialog;
  the user configures a plain-shell agent (e.g. `shell`) and it appears in the
  picker by id. Accepted as the cost of the smaller change.
- A *stopped* plain-shell session rediscovered after an app restart respawns as
  the project default (stopped sessions carry no tmux env) — the same existing
  limitation as any agent override.
- A project whose default agent has an empty command makes its default a shell;
  a follow-up guard is tracked in `TODOS.md`.
