# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Config-driven new-session dialog (desktop): the "New session" dialog now
  picks a **project** and **agent** from the per-device config instead of
  free-text fields that could silently mismatch the TOML. The project dropdown
  shows each project's host (so you choose the host by choosing the project),
  and the agent dropdown lists configured agents, defaulting to the project's
  default. A new `agents` field on the config DTO carries the agent ids to the
  frontend (the launch argv stays server-side, test-enforced). The selection
  self-heals if config changes while the dialog is open, and an empty config
  shows a clear "no projects configured" state. Addresses #31; refines roadmap
  stage 9.
- Reconnect & respawn UX (desktop, roadmap stage 11): the app now heals broken
  sessions automatically. On window focus after a sleep/wake cycle,
  `reconnectAll` re-attaches every open tab; mid-session drops (ssh keepalive
  detects them within ~45 s) trigger the same reconnect path without user
  action. Sessions whose tmux died show a "Stopped" status and a one-click
  **Respawn** button — the bridge's new `agent` param carries the original
  agent id (surfaced by pre-stop discovery, D6) so the fresh session runs the
  same agent the user launched. A permanent failure (bad config, auth error,
  missing host) classifies as "Disconnected" and shows the cause instead of
  spinning forever (D5 error classification, exponential backoff with a cap).
  Remote `TERM` is now pinned to `xterm-256color` on every PTY spawn so real
  terminal sessions render colour and box-drawing correctly (closes #26). The
  sidebar's stopped-session rows gain a Respawn action that opens a new tab
  directly.
- Real ssh transport wiring (desktop, roadmap stage 11 precursor): the Tauri
  bridge now drives real ssh sessions instead of the in-process fake. It
  resolves the transport per call from the per-device config — a new
  `SourceResolver` trait + `ConfigResolver` (`bridge/resolve.rs`) map a
  project's host to an `SshSource`, and the bridge looks up
  `project → host → transport` on each `spawn`/`attach`/`respawn`/`list` (no
  command-signature change; `SshSource::new` opens no connection until used, so
  building one per call is free). `list()` aggregates across all configured ssh
  hosts, tolerates one host being unreachable (partial result), and errors only
  when every host fails — carrying the failing host's cause. A `kubectl` host
  surfaces a clear "unsupported (stage 12)" config error. The fake
  `SessionSource` is now test-only; an empty config shows the empty-state
  sidebar. `docs/DEVELOPMENT.md` documents the per-device `config.toml` for
  pointing the app at a real host.
- Sidebar with live session state (desktop, roadmap stage 10): a left sidebar
  renders the Host → Project → Session tree from the config-and-discovery join,
  refreshed live; clicking a live session attaches it as a tab. A new
  `config_get` Tauri command reads the per-device TOML (`remora-core` owns the
  `remora/config.toml` suffix, the shell supplies the OS config dir) and
  projects it through display-only DTOs in `bridge/dto.rs` — connection secrets
  (ssh user/host/port, kube pod/namespace/context) never cross the boundary, and
  a test enforces it. A missing config file is an empty sidebar (a fresh device
  is valid); only a genuinely unreadable file (permissions, parse error)
  surfaces as an error banner. The join itself is a pure, node-tested
  `buildTree`: configured hosts render even when empty, and discovered sessions
  whose project is not in config bucket under a synthetic "Unconfigured" host.
  Live polling lives in a plain `DiscoveryStore` (poll + in-flight guard,
  paused while the window is hidden and refreshed on focus, last-known tree
  retained on a failed poll); a thin `useSyncExternalStore` hook binds it to
  React. Frontend-only logic is node-tested; component rendering is covered by
  manual QA + a tracked e2e TODO. Runs on the in-process fake `SessionSource`.
- Tabs + one-click session spawn (desktop, roadmap stage 9): the app is usable
  for the first time. A tabbed window runs one session per tab, and a free-form
  "New session" dialog spawns sessions against the bridge. State and the
  connection lifecycle live in a plain, node-tested `SessionStore`: tabs are
  deduped by `project/session`, an in-flight-connect guard closes the orphaned
  connection if a tab is closed (or the store torn down) before it resolves, and
  a spawn-first `openSession` reports whether it spawned a fresh session or
  attached an existing one. A thin `useSyncExternalStore` hook binds the store
  to React; every terminal stays mounted (inactive panes hidden via inline
  `display:none`) so scrollback survives tab switches. The dialog owns the
  connecting/error UI (focus trap, Esc/Enter, restore-focus) and the tab bar
  carries `tablist`/`tab` ARIA roles with a visible focus ring. Frontend-only,
  on the in-process fake `SessionSource`.
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
