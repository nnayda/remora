# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Embedded terminal (desktop, roadmap stage 8): xterm.js wired to the Tauri
  bridge. A `SessionConnection` seam (`connection.ts`) buffers PTY output until
  the terminal subscribes, then streams it and delegates write/resize/close to
  the bridge handle; `connectSession` attaches an existing session or spawns a
  new one (attach→spawn→attach), surviving React StrictMode's double-mount and
  page reloads. A framework-agnostic `TerminalController` owns the xterm
  `Terminal` + `FitAddon`: raw bytes go to `term.write` (the emulator owns
  screen state — bytes are never parsed), keystrokes are UTF-8 encoded back, and
  DOM-driven resize is debounced and zero/unchanged-guarded, with a
  `[session closed]` notice on transport death. A thin `<Terminal>` React
  wrapper plus a one-session dev harness make the loop interactive against the
  fake `SessionSource`. Adds `@xterm/xterm` + `@xterm/addon-fit`.
- Tauri bridge (desktop, roadmap stage 7): the `src-tauri` layer now owns a
  `SessionSource` and exposes `session_{list,spawn,attach,respawn,write,resize,
  close}` Tauri commands, streaming PTY output to the React frontend over
  `tauri::ipc::Channel`. Runs on the in-process fake `SessionSource` this stage
  (real ssh/kubectl transports wire in at later roadmap stages). The forward-task lifecycle is
  register-before-spawn with a `oneshot` cancel and a biased `select!`, so
  `close()` is strictly silent; the handle-keyed registry recovers from mutex
  poisoning. Command-arg ids cross the IPC boundary as strings and are
  validated by constructing the protocol newtype (fail-closed on forged ids);
  frontend-facing error/metadata values stay display-only. TypeScript bindings
  are generated from Rust by tauri-specta (`apps/desktop/src/bindings.ts`,
  guarded by a drift test) and wrapped by a typed `bridge.ts` client. The UI
  talks only to this layer (ARCHITECTURE.md "one rule").
- ssh session discovery + respawn: `SshSource::list` discovers sessions on a
  host by joining live tmux sessions (`list-sessions` + per-session
  `show-environment` metadata) with stopped worktrees (`git worktree list`),
  scoped to configured projects. `attach` now runs a `has-session` liveness
  preflight and returns `SessionNotFound` for a missing session instead of
  optimistically attaching. `respawn` re-creates a stopped session's tmux
  session without re-adding its surviving worktree, fail-closed on a vanished
  worktree directory (`test -d` preflight) and attaching to the live session
  if a concurrent respawner won the `new-session` race. New transport-agnostic
  `discovery` module (pure parse + join of untrusted sandbox output, ADR-0004)
  and `naming` inverses (`parse_tmux_session_name`, `parse_worktree_path`).
- ssh transport (attach): `SshSource` opens a PTY channel to an existing
  remote tmux session over ssh (`tmux attach-session -d`), streaming bytes
  both ways and propagating resize, on a reusable `transport::pty_process`
  bridge. `naming::tmux_session_name` centralizes the
  `remora_<project>_<session>` convention. `spawn`/`list` are stubbed for
  later stages. Adds the `portable-pty` dependency.
- Project scaffold: pnpm + Cargo workspaces, Tauri 2 desktop app skeleton,
  `remora-core` / `remora-protocol` crates, CI, and community docs.
