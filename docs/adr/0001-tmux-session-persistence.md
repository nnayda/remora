# 0001. Borrow session persistence from tmux

- **Status:** Accepted
- **Date:** 2026-06-11
- **Issue/PR:** — <!-- predates the repo; product context in VISION.md -->

## Context

Remora's core promise is that a coding session survives anything the client
does: close the laptop, lose wifi, switch to the phone — reconnect and the
agent is still running exactly where it was. Something on the sandbox must
keep the interactive agent CLI (`claude` today) alive, with its PTY and
scrollback, while no client is attached.

The hard constraint (see [VISION.md](../VISION.md)) is that we drive the
agent's *native interactive* TUI through a real PTY — no headless modes
(`claude -p`), no SDK. Today the process runs in the foreground of an SSH
connection, so it
dies the moment the laptop sleeps, even though the sandbox itself keeps
living. The problem is precisely "a terminal process that survives disconnect
and can be re-attached."

## Decision

We will not build persistence; we will borrow it from tmux.

Each Remora session is **one git worktree plus one named tmux session**
running the agent on the sandbox. The tmux session name is the session's
identity: listing sessions is `tmux ls`, reconnecting is *open a channel →
`tmux attach` → repaint*. Clients reach tmux over existing operator
credentials (`ssh` or `kubectl exec`); the channel is disposable, the tmux
session is not.

The sandbox requirements are exactly `tmux`, `git`, and the agent's CLI — no
Remora-specific daemon or background service to install, update, or
supervise.

## Alternatives considered

- **Do nothing** (keep `claude` in the foreground of an SSH connection): the
  process dies on every disconnect — this is the status quo the project
  exists to fix.
- **Custom Remora daemon** supervising agent processes and buffering
  output: maximum control (structured events, push hooks), but it is new
  distributed software every operator must install, trust, and keep running.
- **dtach / abduco**: lighter than tmux, but no scrollback buffer and a much
  smaller ecosystem — we would reinvent what tmux gives us for free.
- **mosh / ttyd-style connection layers**: make the *transport* resilient,
  but the process still dies with its server side; no detach-and-reattach
  semantics on their own.

## Consequences

What becomes easier:

- Reconnect-from-anywhere works with zero new infrastructure, and the failure
  modes are tmux's well-understood ones rather than ours.
- Sessions are inspectable and recoverable without Remora: an operator can
  always `ssh` in and `tmux attach` by hand. No lock-in, no daemon to debug.
- The blast radius of a misbehaving agent stays inside the sandbox; Remora
  adds no resident software that widens it.

What becomes harder, and what we are committed to:

- The client sees a terminal, not a structured event stream. Anything Remora
  wants to know about a session (is the agent waiting on a prompt?) must be
  derived from terminal output or layered on separately.
- Sandbox images must include tmux, and we depend on its CLI/control-mode
  behavior across versions.
- Per-session state beyond the scrollback (timestamps, task metadata) needs
  its own home; tmux only persists the terminal.

If a future requirement (e.g. rich push notifications or structured session
events) outgrows what tmux can express, that change reverses this decision
and needs a new ADR — see [CONTRIBUTING.md](../../CONTRIBUTING.md).
