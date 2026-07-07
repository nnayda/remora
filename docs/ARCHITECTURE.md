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
  Phone ──WS/TLS──► relay (blind) ──WS/TLS──► bridge ──ssh / kubectl exec──► sandbox (tmux)
  App  ──┘         routes ciphertext         holds creds; = the desktop app
                   only, holds no creds      or a headless container, always
                                             on user hardware (ADR-0021)
  (one Noise session runs END-TO-END phone⇄bridge, through the relay — the
   per-hop WS/TLS is transport framing, never where encryption terminates)
```

Persistence is borrowed, not invented: tmux already solves "process survives
disconnect", so reconnect is *open a channel → `tmux attach` → repaint*. The
sandbox needs only `tmux`, `git`, and the agent's CLI — no Remora daemon.

## Hosts, projects, sessions

Sessions are organized as **Host → Project → Session**
([ADR-0004](adr/0004-local-config-live-session-discovery.md)). A *host* is a
configured transport (ssh, kubectl exec; docker planned); a *project* is a
directory on it with a workspace mode (`worktree` for git repos — fresh
worktree + branch per session — or `shared` for plain directories) and a
default agent, overridable per session.

State splits along a hard line. Hosts and projects are **local declarative
config** — one human-editable TOML file per device. Sessions are **never
stored in any client**: they are discovered by listing tmux sessions named
`remora_<project-id>_<session-id>` (ids are `[a-z0-9-]` slugs, so the name
parses unambiguously; extra metadata rides in tmux session environment
variables) and joining them back to config. That join is what the sidebar
renders, and it is why a session launched on one device appears on every
other device configured for that host. After a pod restart, a surviving
worktree with no tmux session surfaces as *stopped*, with one-click
respawn.

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
| `crates/remora-protocol` | Wire types every client speaks (session ids, messages). Deliberately dependency-light — it is the contract third-party clients build against, specified in [PROTOCOL.md](PROTOCOL.md). | `serde` only |
| `crates/remora-core` | Session model, the `SessionSource` trait, and its direct-mode implementations (ssh, kubectl exec). | `remora-protocol`, `tokio`, `async-trait` |
| `crates/remora-relay` | Blind envelope-frame relay binary (ADR-0021): routes opaque frames between authenticated WebSocket connections; never depends on crypto or session-content crates. | `remora-protocol`, `tokio-tungstenite` |
| `crates/remora-bridge` | User-side bridge (ADR-0021): holds the bridge static identity, the paired-device roster, and a `RemoteSource` (`SessionSource` impl driven end-to-end over Noise, through the relay or loopback). Library + standalone headless binary (`remora-bridge serve`) — hosted in-process by the desktop, or run as its own daemon/container; see [crates/remora-bridge/README.md](../crates/remora-bridge/README.md). | `remora-core`, `remora-protocol`, `snow`, `tokio-tungstenite` |
| `apps/desktop/src-tauri` | Tauri 2 shell: owns the `SessionSource` instance(s) and an open-channel registry; exposes `session_*` Tauri commands (spawn/attach/list/write/resize/respawn/close) that stream PTY output to the frontend over `ipc::Channel`. The channel also carries typed activity events — a status value and a sanitized preview — produced by the core-side detector ([ADR-0013](adr/0013-core-side-activity-detector.md)) and consumed by the UI, which renders them and performs no detection of its own. The UI talks only to this layer. | `remora-core` |
| `apps/desktop/src` | React UI: tabs, embedded terminal, file/diff/PR panels. Talks only to the Tauri layer. | `@tauri-apps/api` |

Relay mode splits along the trust line
([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)): the **blind
relay** (`remora-relay`, a standalone binary) routes end-to-end-encrypted
`remora-protocol` frames between paired devices without being able to read
them, and the **bridge** (`remora-bridge`, a library) hosts a `RemoteSource`
that drives `remora-core` end-to-end over Noise — same seam, no UI changes.
The bridge is only ever hosted on user hardware: it runs in-process inside
the desktop app (dev-only loopback dogfood behind
`REMORA_REMOTE_LOOPBACK=1`), or as a standalone headless `remora-bridge`
binary/container for laptop-asleep access (#234) — see
[crates/remora-bridge/README.md](../crates/remora-bridge/README.md) for
running the headless binary as an operator. Relay **slice 1** (envelope
protocol, one E2E PTY stream — attach, list — and per-session mutual exclusion
below the `SessionSource` seam) has since been joined by real QR split-secret
device pairing (#232), opt-in UnifiedPush wake delivery
([ADR-0023](adr/0023-unifiedpush-first-wake-delivery.md), #233), and the
headless bridge binary itself (#234) — with push-wake delivery from that
headless daemon specifically still a follow-up (see the bridge README's
Health & ops section).

## Security invariants

- The agent, the checked-out code, and all tool execution stay on the sandbox.
  The client sends keystrokes and paints pixels — nothing more.
- Everything a client discovers from a sandbox (tmux session names,
  environment metadata) is untrusted input: anyone with a shell there —
  including the agent — can forge it. Discovered state informs display and
  the config join only; spawn and respawn build commands exclusively from
  local configuration ([ADR-0004](adr/0004-local-config-live-session-discovery.md)).
- Clients hold no *sandbox-reaching* secrets beyond what the user already
  uses (kubeconfig / SSH); on-device end-to-end identity keys (per-device
  Noise statics, ADR-0021) are the deliberate carve-out — generated on the
  device, never leaving it, never able to reach a sandbox. In relay mode the
  phone authenticates **end-to-end to the bridge** (PSK-paired, pinned
  statics); it presents only a routing credential to the relay, and never
  holds a key to the sandbox.
- Sandboxes need no public ingress. Direct mode rides the user's existing
  reachability (VPN / bastion / kubeconfig); in relay mode only the
  user-side bridge reaches them — the relay reaches nothing and routes only
  ciphertext; with a mesh VPN (e.g. Tailscale), the mesh is the boundary.
- The relay stores no plaintext session content and no sandbox credentials,
  ever ([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)). E2E keys
  are generated on-device and never leave it. A fully compromised relay
  yields metadata and denial of service — no session *plaintext*, no code,
  no sandbox access; timing/size side channels over the interactive stream
  remain and are named as an accepted risk in ADR-0021.
- Sandbox hardening is recommended and documented, not enforced: resource
  limits, network egress policy, no host cloud credentials, and
  ephemeral/disposable pods.
- Changes that move execution, repository content, or credentials onto the
  client device violate the design and need an ADR, not just a PR.
