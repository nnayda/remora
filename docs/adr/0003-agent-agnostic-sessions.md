# 0003. Treat the agent as a pluggable interactive CLI

- **Status:** Accepted
- **Date:** 2026-06-11
- **Issue/PR:** —

## Context

Remora launches with Claude Code, but the value proposition — persistent,
sandboxed, reconnect-from-anywhere sessions — is about *where the agent runs
and how you reach it*, not which agent it is. Comparable interactive agent
CLIs (OpenAI's Codex CLI among them) run in a terminal the same way, and
[ADR-0001](0001-tmux-session-persistence.md) already defines a session as "a
terminal process under tmux" — which is agent-shaped, not Claude-shaped.

Baking `claude` into the session model, wire protocol, or UI would couple an
infrastructure product to one vendor's CLI and make every future agent a
refactor instead of a configuration.

## Decision

We will treat the agent as an opaque interactive CLI running in a PTY. A
Remora session is one git worktree plus one tmux session running *an agent
command* — `claude` is the default, never an assumption:

- `remora-protocol` and `remora-core` must not contain agent-specific types,
  commands, or output parsing.
- Agent-specific knowledge — the launch command, and any "agent is waiting
  for input" detection heuristics — lives in per-agent adapter configuration:
  data, not code paths.
- The UI renders a terminal. It never reimplements any agent's UI.

Claude Code is the first supported agent and the primary test target; Codex
CLI is the proof of agnosticism.

## Alternatives considered

- **Claude-only:** simpler heuristics (one prompt format to detect, one CLI
  to test), but ties the project to one vendor, and generalizing later means
  auditing the protocol and core for baked-in assumptions under
  compatibility pressure.
- **Deep per-agent integration** (structured events, SDK hooks per agent):
  richer signals, but contradicts the core constraint of driving native TUIs
  through a real PTY — that is SDK territory the project explicitly rejects
  (see [VISION.md](../VISION.md)).
- **Do nothing** (let `claude` references accrete and decide later): the
  cheapest today and the most expensive the day a second agent arrives.

## Consequences

What becomes easier:

- Supporting a new agent is an adapter entry plus heuristics, not a feature.
- Positioning widens from "a Claude client" to "your agent, in a sandbox you
  control, from any device".

What becomes harder, and what we are committed to:

- Core features may rely only on the lowest common denominator: a terminal.
  Anything smarter — notably the relay's "agent needs you" notification
  signal — must be implemented as per-agent heuristics, and each supported
  agent multiplies the testing surface.
- Docs and UI copy say "agent", using `claude` as an example rather than the
  definition; PRs that hardcode an agent into core or protocol need an ADR,
  not just a review.
