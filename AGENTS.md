# AGENTS.md

Remora — persistent remote coding-agent sessions from any device. Tauri 2
desktop app (React + TypeScript frontend, Rust shell) over shared Rust crates.
**Pre-alpha:** the desktop hero scenario works in direct mode (spawn a session,
drive the agent in an embedded terminal, reconnect over `ssh`/`kubectl exec`);
there are no packaged releases yet and the panels around the terminal are still
landing. See docs/ROADMAP.md for what's left.

## Commands

```sh
pnpm install
pnpm dev          # tauri dev — desktop app with hot reload
pnpm test         # frontend tests (Vitest)
pnpm typecheck    # tsc --noEmit across the workspace
pnpm lint         # Biome lint + format check (pnpm lint:fix to auto-fix)
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Full pre-push gauntlet (mirrors CI):

```sh
pnpm lint && pnpm typecheck && pnpm test
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

**Gotcha:** full-workspace `cargo test`/`clippy` compile the Tauri shell, which
needs system libraries (webkit2gtk etc. — see docs/DEVELOPMENT.md). If you only
touched `crates/`, iterate with `cargo test -p remora-core -p remora-protocol`.

## Layout

- `apps/desktop` — Tauri 2 app: frontend in `src/`, Rust shell in `src-tauri/`
- `crates/remora-core` — session model + the `SessionSource` transport trait
- `crates/remora-protocol` — wire types shared by clients and the future relay
- `docs/` — VISION.md (direction), ARCHITECTURE.md (system map),
  ROADMAP.md (MVP build order + stage status), adr/ (decisions)

## The one rule

UI code never talks to ssh/kubectl directly — everything goes through the
`SessionSource` trait in `remora-core`. Similarly, core/protocol treat the
agent as an opaque interactive CLI in a PTY; agent-specific knowledge (launch
command, prompt heuristics) lives only in per-agent adapter data. If a change
makes the UI transport-aware or the core Claude-aware, it's going the wrong
direction. See docs/ARCHITECTURE.md.

## Conventions

- PR titles follow Conventional Commits (CI-enforced); PRs are squash-merged,
  so the title becomes the commit message.
- Rust: `unsafe_code` denied workspace-wide; `clippy::unwrap_used` warns;
  clippy warnings fail CI.
- TypeScript: Biome is the style arbiter — no manual formatting debates.
- Architectural decisions get a new ADR in `docs/adr/`; never rewrite old ones.
- Working artifacts — design specs, implementation plans, session notes,
  throwaway spikes (`docs/superpowers/`, `notes/`, `spikes/`) — stay local
  and are never committed (gitignored). Durable outcomes belong in ADRs,
  VISION.md, or ARCHITECTURE.md instead.
- Releases bump the version in 4 places: root `package.json`,
  `apps/desktop/package.json`, `apps/desktop/src-tauri/tauri.conf.json`, and
  `[workspace.package]` in `Cargo.toml`.
