# Architecture

This is the working map of the codebase. Product direction and roadmap live
in [VISION.md](VISION.md); decisions, with the alternatives they beat, are
recorded as [ADRs](adr/).

## The shape of the system

A Remora session is **one git worktree plus one named tmux session** running
an agent CLI (`claude` today, `codex` and others by design — see
[ADR-0003](adr/0003-agent-agnostic-sessions.md)) on a remote sandbox. Clients
are thin windows onto those live tmux sessions; nothing about a session lives
in any client.

```
DIRECT MODE (default, zero infra)
  App ──ssh / kubectl exec──► sandbox (tmux: one session per worktree)

RELAY MODE (opt-in, enables phone-from-anywhere + push notifications)
  App   ──WS──► relay ──ssh / kubectl exec──► sandbox (tmux)
  Phone ──WS──┘
```

Persistence is borrowed, not invented: tmux already solves "process survives
disconnect", so reconnect is *open a channel → `tmux attach` → repaint*. The
sandbox needs only `tmux`, `git`, and the agent's CLI — no Remora daemon.

## The one rule

**UI code never talks to ssh/kubectl directly.** Everything goes through the
`SessionSource` trait in `remora-core`. That seam is what makes the relay an
optional drop-in rather than a fork of the app: in direct mode the
implementation drives `ssh`/`kubectl exec` in-process; in relay mode the same
interface is hosted behind a WebSocket and the client gets a remote
implementation. If a change makes the UI aware of the transport, it's going in
the wrong direction.

The same discipline applies to the agent: `remora-core` and `remora-protocol`
treat it as an opaque interactive CLI in a PTY. Agent-specific knowledge
(launch command, prompt-detection heuristics) is per-agent adapter data, never
a type or code path in core
([ADR-0003](adr/0003-agent-agnostic-sessions.md)).

## Crates and apps

| Unit | Purpose | Depends on |
| --- | --- | --- |
| `crates/remora-protocol` | Wire types every client speaks (session ids, messages). Deliberately dependency-light — it is the contract third-party clients build against. | `serde` only |
| `crates/remora-core` | Session model, the `SessionSource` trait, and its direct-mode implementations (ssh, kubectl exec). | `remora-protocol` |
| `apps/desktop/src-tauri` | Tauri 2 shell: owns processes/PTYs, exposes commands to the frontend. | `remora-core` |
| `apps/desktop/src` | React UI: tabs, embedded terminal, file/diff/PR panels. Talks only to the Tauri layer. | `@tauri-apps/api` |

A future `relay` binary will host `remora-core` behind a WebSocket and speak
`remora-protocol` to clients — same seam, no UI changes.

## Security invariants

- The agent, the checked-out code, and all tool execution stay on the sandbox.
  The client sends keystrokes and paints pixels — nothing more.
- Clients hold no long-lived secrets beyond what the user already uses to
  reach their sandbox (kubeconfig / SSH). In relay mode the phone
  authenticates to the relay and never holds a key to the sandbox.
- Sandboxes need no public ingress. Direct mode rides the user's existing
  reachability (VPN / bastion / kubeconfig); in relay mode the relay is the
  only thing that reaches them; with a mesh VPN (e.g. Tailscale), the mesh is
  the boundary.
- Sandbox hardening is recommended and documented, not enforced: resource
  limits, network egress policy, no host cloud credentials, and
  ephemeral/disposable pods.
- Changes that move execution, repository content, or credentials onto the
  client device violate the design and need an ADR, not just a PR.
