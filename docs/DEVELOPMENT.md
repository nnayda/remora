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

Run the full pre-push gauntlet (what CI runs):

```sh
pnpm lint && pnpm typecheck && pnpm test
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

> **Note:** `cargo clippy`/`cargo test` on the full workspace compile the Tauri
> shell, which needs the system libraries above. If you only touched
> `crates/`, you can iterate faster with
> `cargo test -p remora-core -p remora-protocol`.

## Repository layout

| Path | What it is |
| --- | --- |
| `apps/desktop` | Tauri 2 desktop app — React + TypeScript frontend in `src/`, Rust shell in `src-tauri/` |
| `crates/remora-core` | Session model and the `SessionSource` transport seam |
| `crates/remora-protocol` | Wire protocol types shared by clients and the future relay |
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

## Releases

1. Update `CHANGELOG.md` (move entries from *Unreleased* to a new version
   section) and bump `version` in `package.json`, `apps/desktop/package.json`,
   `apps/desktop/src-tauri/tauri.conf.json`, and `[workspace.package]` in
   `Cargo.toml`.
2. Tag: `git tag vX.Y.Z && git push --tags`.
3. CI builds macOS (arm64 + x86_64) and Windows bundles into a **draft**
   release; check the artifacts, then publish it by hand.
