# Vision

Where Remora is going and why. The codebase map lives in
[ARCHITECTURE.md](ARCHITECTURE.md); individual decisions are recorded as
[ADRs](adr/).

## The problem

Running an interactive coding agent (`claude`, `codex`, …) on a remote
sandbox over plain SSH is clumsy: one connection at a time, no good way to
juggle several coding sessions, and when the laptop sleeps or the terminal
closes, the running agent process dies — even though the sandbox keeps
living.

Remora is a desktop (and eventually mobile) app where:

- Each coding session is a tab in a single window.
- Creating a session opens your agent in a fresh git worktree on the sandbox.
- Sessions **persist** — close the app, sleep the laptop, lose wifi, then
  come back and reconnect to the same live sessions exactly where they were.
- Repo files, git diffs, and pull requests are browsable without leaving the
  app.
- A session started on the laptop can be continued from a **phone** — check
  status, answer prompts, redirect the agent mid-errand.

## The hero scenario

**Seamless reconnect, across devices.** Close the laptop on a train mid-task,
open the phone at the next stop, and the agent is still running, exactly
where you left it. The session doesn't live in any client — it lives on the
sandbox — so every device is a thin window that re-attaches to the same live
process.

## What makes this differentiated

1. **The agent never runs on your device.** Tools like Conductor, Crystal,
   and claude-squad run the agent on your laptop, where it shares a machine
   with your SSH keys, browser cookies, and cloud credentials. Remora inverts
   that: the agent, the checked-out code, and every tool call live in a
   disposable remote sandbox. If the agent goes off the rails, the blast
   radius is a container, not your machine.
2. **Persistence is borrowed, not invented.** tmux already solves "process
   survives disconnect" ([ADR-0001](adr/0001-tmux-session-persistence.md)).
   Remora composes proven primitives into better UX rather than building a
   distributed system.

The competitive gap, concretely: several tools do "multiple agents in
worktrees" (claude-squad, Crystal, Conductor) and separate tools do
"persistent / roaming remote terminal" (tmux, mosh, vibetunnel, ttyd). A
newer category does "drive your coding agent from your phone" over an
end-to-end-encrypted relay — but their bridge is the machine *running the
agent*, so the agent and your credentials still share a computer. E2EE
relays are table stakes in that category now; Remora's surviving edge is the
sandbox-first inversion: the bridge holds only transport creds while the
agent stays in a disposable remote sandbox
([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)) — fused with
cross-platform reconnect as the headline.

## Constraints

- **Native agent CLIs only.** The agent's interactive TUI, driven via a real
  PTY. No headless modes (`claude -p`), no SDKs, no API keys. The app embeds
  a true terminal emulator; it never reimplements an agent's UI. Files /
  diffs / PRs are panels *around* the terminal, not a replacement for it.
- **Provider-agnostic.** Claude Code is the first supported agent and the
  primary test target, but nothing in the session model or protocol may
  assume it; Codex CLI is the proof
  ([ADR-0003](adr/0003-agent-agnostic-sessions.md)).
- **Cross-platform:** macOS (priority), Windows (required), iOS + Android
  (wanted) — one codebase
  ([ADR-0002](adr/0002-tauri-single-codebase-optional-relay.md)).
- **Agent execution stays off the local device** — a security requirement,
  not a preference (see the security invariants in
  [ARCHITECTURE.md](ARCHITECTURE.md)).
- **Open source.** Install friction matters: the default path must require no
  server. The relay is an opt-in upgrade for phone-from-anywhere and push
  notifications; a mesh VPN (e.g. Tailscale) is a documented no-relay
  alternative for connectivity, though notifications still need an always-on
  piece.
- **Nothing custom on the sandbox.** `tmux` + `git` + your agent's CLI,
  reached over `ssh` or `kubectl exec` (`docker exec` as a fast-follow).

## Feature surface (v1)

- Tabbed window, one tab per session.
- Sidebar organizing configured hosts and their projects, with live session
  state ([ADR-0004](adr/0004-local-config-live-session-discovery.md)).
- One-click session spawn (worktree + tmux + agent on the sandbox).
- Embedded terminal rendering the agent's native TUI via PTY.
- Seamless reconnect from the same or any reachable machine (direct mode).
- Repo file browser, git diff viewer, PR review panel.
- Mode toggle: direct (default) vs relay.

## Success criteria

- Spawn a session from the desktop app — worktree created, the agent running
  under tmux — in under ~5 seconds, no manual SSH.
- Run several sessions as tabs simultaneously.
- Spawn a session running a second agent CLI (e.g. `codex`) with no changes
  to the protocol or core — only adapter configuration.
- Sleep/close the app, reopen, and every session re-attaches live, mid-task —
  in direct mode, with zero extra infrastructure.
- Browse files, view a git diff, and review a PR without leaving the app.
- (Relay milestone) Start a session on the laptop, get a push notification on
  the phone when the agent blocks for input, and answer it from the phone on
  the same live session.
- A new user gets the desktop hero working in ~2 minutes without deploying
  any server.

## Roadmap

1. **Spike the spine:** from a throwaway script, open a PTY on the sandbox,
   start an agent (`claude`) under tmux in a worktree, detach, re-attach,
   stream to a local xterm.js window. Prove reconnect-after-sleep end to end.
2. **Define `SessionSource` + the wire protocol** so direct mode and the
   future relay are the same shape.
3. **Tauri desktop shell:** tabs, embedded terminal, session spawn and
   list/reconnect — direct mode, `ssh` + `kubectl` adapters.
4. **Read-only panels:** file browser, git diff viewer, then PR review.
5. **Desktop CI:** signed/notarized macOS + Windows builds, auto-update on
   tag.
6. **Relay mode (opt-in):** blind relay + user-side bridge hosting
   `SessionSource` behind a WebSocket
   ([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)); document the
   Tailscale no-relay path.
7. **Mobile client + push notifications:** Tauri mobile build; relay fires
   "agent needs you"; phone answers on the same live session.
8. **Fast-follows:** the `docker exec` backend, and a second agent adapter
   (`codex`) to prove the agent-agnostic seam
   ([ADR-0003](adr/0003-agent-agnostic-sessions.md)).

## Distribution

Code nobody can install is code nobody uses, so distribution is part of the
project from day one:

- **Desktop:** signed + notarized `.dmg`/`.app` (macOS), signed `.msi`/`.exe`
  (Windows), auto-update via the Tauri updater.
- **Mobile:** iOS (TestFlight → App Store) and Android (APK/Play) from the
  same codebase, gated on the relay/Tailscale path being ready.
- **Relay:** a small container image with a `docker run`/compose example and
  a Helm chart.
- **CI/CD:** a GitHub Actions matrix building all targets on tag, publishing
  to GitHub Releases and the stores, pushing the relay image to a registry.

## Open questions

- **Tauri mobile maturity:** is a PTY rendered in a mobile webview (xterm.js)
  acceptable, or does the phone need a thinner adapted view (status + quick
  actions) instead of a full TUI on a small screen?
- **Mobile prompt ergonomics:** quick-action buttons (approve / deny / common
  replies) over the raw terminal?
- **Notification signal:** how does the relay detect "agent is waiting for
  you" — parse the PTY stream for prompt patterns, a tmux hook, or a status
  signal from the agent itself? Whatever the mechanism, the heuristics are
  per-agent adapter data, not core code
  ([ADR-0003](adr/0003-agent-agnostic-sessions.md)).
- **Worktree/branch hygiene:** naming and cleanup of stale worktrees and
  tmux sessions. (The pod-restart half is decided: surviving worktrees
  surface as *stopped* with one-click respawn —
  [ADR-0004](adr/0004-local-config-live-session-discovery.md).)
- ~~**Relay auth**~~ — answered: QR split-secret pairing (relay-visible
  rendezvous token + relay-blind PSK in the Noise handshake); see
  [ADR-0021](adr/0021-blind-relay-bridge-trust-model.md).
- ~~**Relay configuration source**~~ — answered: the relay has no copy.
  Host config and credentials stay on the user-side bridge (the desktop
  app or a self-run headless container); the relay routes ciphertext only
  ([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)).
- **Session display names:** sessions are never stored client-side and
  sandbox-side metadata is untrusted — where could a user-assigned session
  label durably live?
