# Development

## Prerequisites

- **Node.js** ≥ 24 and **pnpm** 11 (`corepack enable` will pick up the
  pinned version from `package.json`).
- **Rust** stable via [rustup](https://rustup.rs) — the exact toolchain and
  components (`rustfmt`, `clippy`) come from `rust-toolchain.toml`
  automatically.
- **Tauri system dependencies** for your OS — see the
  [Tauri prerequisites guide](https://v2.tauri.app/start/prerequisites/).
  On Linux that means `libwebkit2gtk-4.1-dev`, `libxdo-dev`, `libssl-dev`,
  `libayatana-appindicator3-dev`, `librsvg2-dev`, and `build-essential`
  (the same list CI installs in `.github/workflows/ci.yml`).

## Getting started

```sh
pnpm install
pnpm dev          # tauri dev — compiles the Rust shell and opens the app
```

## Everyday commands

| Command | What it does |
| --- | --- |
| `pnpm dev` | Run the desktop app with hot reload |
| `pnpm build` | Production build of the desktop app |
| `pnpm test` | Frontend tests (Vitest) |
| `pnpm typecheck` | TypeScript `tsc --noEmit` across the workspace |
| `pnpm lint` | Biome lint + format check |
| `pnpm lint:fix` | Auto-fix lint and formatting |
| `cargo test --workspace` | Rust tests |
| `cargo clippy --workspace --all-targets -- -D warnings` | Rust lints (CI fails on warnings) |
| `cargo fmt --all` | Format Rust code |
| `cargo deny check` | License / advisory / source audit ([cargo-deny](https://embarkstudios.github.io/cargo-deny/)) |

## Design system gallery

The `apps/desktop/src/ui/` component library and the `styles/tokens/` are
showcased in a dev-only gallery. With `pnpm dev` running, open
**http://localhost:1420/showcase.html** to browse every component and token in
light and dark (use "Cycle theme"). It mounts the *real* `ui/` components against
the real tokens, so it can't drift from the shipped library — and `showcase.html`
is not an entry in `vite build`, so it never reaches the production app.

Run the full pre-push gauntlet (what CI runs):

```sh
pnpm lint && pnpm typecheck && pnpm test
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

> **Note:** `cargo clippy`/`cargo test` on the full workspace compile the Tauri
> shell, which needs the system libraries above. If you only touched
> `crates/`, you can iterate faster with
> `cargo test -p remora-core -p remora-protocol`.

## End-to-end ssh tests

`crates/remora-core/tests/ssh_e2e.rs` exercises the ssh transport against a
live sshd + tmux. Every test is `#[ignore]`d so the default suite stays
hermetic. They cover the full lifecycle:

- **attach** — attaches to a pre-existing tmux session, resizes, runs
  `echo remora-e2e-ok`, and asserts the marker shows up in the PTY stream.
- **spawn** — a shared-workspace spawn (and a duplicate-session block) and a
  worktree-mode cold start that creates a fresh worktree.
- **discovery + respawn** — stops a session, discovers it as *stopped* via
  `list`, respawns it (reusing the surviving worktree, no `git worktree add`),
  and checks that respawning a vanished worktree fails closed as
  `SessionNotFound`.

To run them, point the tests at a reachable host. The attach test needs a tmux
session created up front:

```sh
# on the host:
tmux new-session -d -s remora_demo_one
# locally:
REMORA_E2E_SSH_HOST=<host> REMORA_E2E_PROJECT=demo REMORA_E2E_SESSION=one \
  cargo test -p remora-core --test ssh_e2e -- --ignored --nocapture
```

Environment variables:

- `REMORA_E2E_SSH_HOST=<host>` — required; the ssh destination.
- `REMORA_E2E_SSH_USER=<user>` — optional.
- `REMORA_E2E_SSH_PORT=<port>` — optional.
- `REMORA_E2E_PROJECT=<slug>` — optional (default `demo`); the attach test's
  project id. With `REMORA_E2E_SESSION` it selects which tmux session to attach
  to (`remora_<project>_<session>`), so it must match the session created on
  the host.
- `REMORA_E2E_SESSION=<slug>` — optional (default `one`); the attach test's
  session id (see above).
- `REMORA_E2E_PATH=<dir>` — working dir on the host for the shared-workspace
  spawn test (defaults to `~/e2e`).
- `REMORA_E2E_GIT_PATH=<repo>` — path to an existing git repo on the host;
  required for the worktree spawn, discovery, and respawn tests.

## Repository layout

| Path | What it is |
| --- | --- |
| `apps/desktop` | Tauri 2 desktop app — React + TypeScript frontend in `src/`, Rust shell in `src-tauri/` |
| `crates/remora-core` | Session model and the `SessionSource` transport seam |
| `crates/remora-protocol` | Wire protocol types shared by clients, the bridge, and the relay envelope ([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)) |
| `crates/remora-relay` | Blind envelope-frame relay binary ([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)) |
| `crates/remora-bridge` | User-side bridge ([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)): a `RemoteSource` driving `remora-core` end-to-end over Noise. Library + standalone headless binary (`remora-bridge serve`, see [crates/remora-bridge/README.md](../crates/remora-bridge/README.md)); hosted in-process by the desktop today, or run as its own daemon/container |
| `docs/` | [VISION.md](VISION.md), [ARCHITECTURE.md](ARCHITECTURE.md), [ADRs](adr/) |

## CI

Every PR runs three required jobs (`.github/workflows/ci.yml`):

- **Frontend** — Biome, `tsc`, Vitest
- **Rust** — `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`
- **cargo-deny** — license allow-list, RustSec advisories, dependency sources

CodeQL code scanning (`.github/workflows/codeql.yml`) also runs on every PR,
analyzing the TypeScript, Rust, and workflow files; findings appear under the
repo's Security tab rather than as a failing check.

PR titles must follow Conventional Commits (enforced by
`.github/workflows/pr-title.yml`); see
[CONTRIBUTING.md](../CONTRIBUTING.md).

Pushing a `v*` tag builds desktop bundles and attaches them to a draft GitHub
Release (`.github/workflows/release.yml`).

## Connecting to a real host (dogfooding)

The app reads a per-device config from the OS config dir (not the repo):
`~/.config/remora/config.toml` on Linux, `~/Library/Application Support/
remora/config.toml` on macOS. It is never committed. To drive a real ssh host
(here, an alias `hermes` defined in your `~/.ssh/config`):

```toml
[hosts.hermes]
transport = "ssh"
host = "hermes"            # any ssh destination, including a ~/.ssh/config alias

[agents.claude]
command = ["claude"]
[agents.codex]
command = ["codex"]
[agents.shell]
command = []               # no agent: an empty command is a plain login shell

[projects.<id>]
host = "hermes"
path = "~/<repo>"          # a git repo for worktree mode
workspace = "worktree"     # fresh worktree + branch per session
agent = "claude"           # overridable per session
```

The host needs only `tmux`, `git`, and the agent CLI on `PATH` (a plain-shell
agent needs no CLI at all). With no hosts configured the sidebar shows the
empty state — there is no in-app fake at runtime (the fake is test-only).

## Releases

1. Update `CHANGELOG.md` (move entries from *Unreleased* to a new version
   section) and bump `version` in `package.json`, `apps/desktop/package.json`,
   `apps/desktop/src-tauri/tauri.conf.json`, and `[workspace.package]` in
   `Cargo.toml`.
2. Tag: `git tag vX.Y.Z && git push --tags`.
3. CI builds macOS (arm64 + x86_64) and Windows bundles into a **draft**
   release; check the artifacts, then publish it by hand.
