# Architecture Decision Records

This directory records significant architectural decisions for Remora. Each
ADR captures one decision: the context that forced it, the choice made, the
alternatives considered, and the consequences. ADRs are append-only — if a
later change reverses or extends a decision, add a new ADR that supersedes
the old one rather than editing history (see
[CONTRIBUTING.md](../../CONTRIBUTING.md)).

## Index

| ADR                                      | Title                                | Status   |
| ---------------------------------------- | ------------------------------------ | -------- |
| [0001](0001-tmux-session-persistence.md) | Borrow session persistence from tmux | Accepted |
| [0002](0002-tauri-single-codebase-optional-relay.md) | Build one Tauri codebase for all platforms, with an optional relay | Accepted |
| [0003](0003-agent-agnostic-sessions.md) | Treat the agent as a pluggable interactive CLI | Accepted |
| [0004](0004-local-config-live-session-discovery.md) | Configure hosts and projects locally, discover sessions from the sandbox | Accepted |
| [0005](0005-async-session-source-on-tokio.md) | SessionSource is async on tokio; channels are message pipes | Accepted |
| [0006](0006-app-managed-config-writes.md) | The app writes the config file through a validated editor channel | Accepted |
| [0007](0007-no-agent-plain-shell.md) | A session may run no agent — a plain shell | Accepted |
| [0008](0008-per-session-workspace-override.md) | Workspace mode is overridable per session, with effective mode discovered from real state | Accepted |
| [0009](0009-dynamic-kubectl-field-resolution.md) | kubectl host fields may be resolved from a local shell command at connect time | Accepted |
| [0010](0010-in-band-activity-osc-marker.md) | Carry agent-activity signals in-band via a tmux-passthrough OSC marker | Accepted |
| [0011](0011-ssh-connection-multiplexing-direct-mode.md) | Multiplex direct-mode ssh over one authenticated master (ControlMaster) | Accepted |

## Statuses

- **Proposed** — under discussion, not yet in effect.
- **Accepted** — the decision in effect today.
- **Deprecated** — no longer applies, with no direct replacement.
- **Superseded by ADR-NNNN** — replaced by a later decision.

## Adding an ADR

1. Copy [template.md](template.md) to `NNNN-short-kebab-title.md`, using the
   next sequential number.
2. Fill in the context, decision, and consequences. Keep it short — a page or
   two; link to code or docs instead of restating them.
3. Add a row to the index above.
4. Submit it in the same PR as the change it justifies, where possible.
