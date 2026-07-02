<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/remora-wordmark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/remora-wordmark-light.svg">
    <img alt="Remora" src="docs/assets/remora-wordmark-light.svg" width="320">
  </picture>
</p>

<p align="center"><strong>Persistent remote coding-agent sessions, from any device.</strong></p>

Remora is a cross-platform client for native coding-agent CLIs — Claude Code
first, Codex and others by design — running on a remote sandbox. Each coding
session is a tab; each tab is a git worktree plus a tmux session on your
sandbox. Close the laptop mid-task, reopen it — or open your phone — and the
agent is still running, exactly where you left it.

> **Status: pre-alpha.** The desktop hero scenario now works in direct mode —
> spawn a session, drive the agent in an embedded terminal, close the app and
> reconnect over `ssh` or `kubectl exec`. There are no packaged releases yet,
> and the panels around the terminal are still landing. Run it from source
> ([Developing](#developing)); see [docs/ROADMAP.md](docs/ROADMAP.md) for
> what's left and [docs/VISION.md](docs/VISION.md) for where this is going.

## Why

- **The agent never runs on your device.** The agent, the checked-out code, and
  every tool call live in a disposable remote sandbox (Kubernetes pod, VPS,
  container). If the agent goes off the rails, the blast radius is a container —
  not the machine holding your SSH keys and browser cookies.
- **Persistence is borrowed, not invented.** The agent runs under tmux on the
  sandbox. Reconnect is "open a channel → `tmux attach` → repaint". No custom
  daemon to install on your infrastructure.
- **Your agent, not our agent.** Remora drives the agent's native interactive
  TUI through a real PTY — no SDK, no API keys, no reimplemented UI. Claude
  Code is the first supported agent; nothing in the core assumes it.
- **One codebase, every platform.** Tauri 2 targets macOS, Windows, iOS, and
  Android from a single UI.

## How it works

```
DIRECT MODE (default, zero infra)
  App ──ssh / kubectl exec──► sandbox (tmux: one session per worktree)

RELAY MODE (opt-in, enables phone-from-anywhere + push notifications)
  Phone ──WS/TLS──► relay (blind) ──WS/TLS──► bridge ──ssh / kubectl exec──► sandbox (tmux)
  App  ──┘          one end-to-end Noise session runs phone⇄bridge THROUGH the
                    relay, which forwards ciphertext it cannot read (ADR-0021)
```

The UI always talks to a `SessionSource` — in direct mode it drives
`ssh`/`kubectl exec` in-process; in relay mode the same interface is hosted
by a *bridge* on your own hardware, reached end-to-end-encrypted through a
blind relay ([ADR-0021](docs/adr/0021-blind-relay-bridge-trust-model.md)).
Your sandbox only needs `tmux`, `git`, and your agent's CLI.

## Repository layout

| Path | What it is |
| --- | --- |
| `apps/desktop` | Tauri 2 desktop app (React + TypeScript frontend, Rust shell) |
| `crates/remora-core` | Session model and the `SessionSource` transport seam |
| `crates/remora-protocol` | Wire protocol types shared by clients, the bridge, and the relay envelope (ADR-0021); specified in [docs/PROTOCOL.md](docs/PROTOCOL.md) |
| `crates/remora-relay` | Blind envelope-frame relay binary (ADR-0021) |
| `crates/remora-bridge` | User-side bridge library (ADR-0021): a `RemoteSource` driving `remora-core` end-to-end over Noise; hosted by the desktop today, standalone headless binary is future (#234) |
| `docs/` | Vision, architecture notes, the [wire protocol spec](docs/PROTOCOL.md), ADRs |

## Developing

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for prerequisites and the full
workflow. Short version:

```sh
pnpm install
pnpm dev        # tauri dev (desktop app)
pnpm test       # frontend tests
cargo test      # rust tests
pnpm lint       # biome
cargo clippy --workspace --all-targets
```

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Security
issues go through [SECURITY.md](SECURITY.md), please don't open public issues
for those.

## License

[AGPL-3.0-only](LICENSE).
