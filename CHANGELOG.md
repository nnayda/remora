# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- ssh transport (attach): `SshSource` opens a PTY channel to an existing
  remote tmux session over ssh (`tmux attach-session -d`), streaming bytes
  both ways and propagating resize, on a reusable `transport::pty_process`
  bridge. `naming::tmux_session_name` centralizes the
  `remora_<project>_<session>` convention. `spawn`/`list` are stubbed for
  later stages. Adds the `portable-pty` dependency.
- Project scaffold: pnpm + Cargo workspaces, Tauri 2 desktop app skeleton,
  `remora-core` / `remora-protocol` crates, CI, and community docs.
