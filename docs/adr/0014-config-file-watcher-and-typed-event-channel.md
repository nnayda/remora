# 0014. Watch the config file and push changes via a typed Rust→frontend event

- **Status:** Accepted
- **Date:** 2026-06-26
- **Issue/PR:** [#112](https://github.com/nnayda/remora/issues/112)

## Context

Stage 10 (ADR-0004, ADR-0006) reads the config file once on startup and re-reads
it only on an explicit user action (the sidebar's manual refresh command or a
full app restart). External edits — hand-adding a host or project in a text
editor — stay invisible until the user notices and clicks refresh.

The read-only sidebar carries the label "reflects your config." A stale tree
undercuts that claim: the user edits the file, switches back to the app, and the
sidebar still shows the old state. Because config edits are a natural part of the
early-adopter workflow (no GUI config editor exists yet for hosts), the gap is
user-visible immediately.

Closing it requires two things that did not previously exist:

1. A mechanism for the Rust shell to **push** a notification to the frontend
   without the frontend having polled for it — a Rust→frontend event channel.
   Until now the bridge was command-only (frontend calls Rust via `invoke`);
   there was no push direction.

2. A **file watcher** that detects config changes on disk and drives that channel.

## Decision

### Part A — First typed Rust→frontend event channel

We will establish the push direction of the bridge using `tauri-specta`'s
`collect_events!` macro, which generates typed bindings on the same principle
as `collect_commands!`. A zero-payload sentinel event `ConfigChanged` (a unit
struct decorated `#[derive(Event)]`) is registered in `commands.rs` alongside
the existing commands and surfaced to the frontend as `events.configChanged`.

The frontend subscribes via `subscribeConfigChanged(onChange)` in
`config-watch-listener.ts`, which calls `events.configChanged.listen(() => onChange())`.
On each ping the listener calls `discoveryStore.refresh()` — the same path as
the existing manual refresh — so no new data-loading logic is needed.

The event carries **no payload** (a "ping"). The frontend re-reads the full
config via the existing `config_get` command. This keeps the event decoupled
from `ConfigDto`'s shape: a future schema change does not require a matching
event update.

This is the first push-event in the codebase and sets the precedent for future
Rust→frontend events (e.g. activity-state changes in #69).

### Part B — Config file watcher in the desktop shell

We will watch the config file for changes using `notify` +
`notify-debouncer-full` in a new `config_watch` module (`apps/desktop/src-tauri/src/config_watch.rs`).

Key choices:

- **Watch the parent directory, not the file directly.** Atomic-rename editors
  (Vim `:w`, many GUI editors) write to a sibling temp file and rename it over
  the original. A direct `watch(config_path)` misses this; `watch(parent_dir,
  NonRecursive)` catches it. A path filter (`event_concerns_config`) discards
  sibling events so only the config file triggers a ping.

- **500 ms debounce** (`CONFIG_WATCH_DEBOUNCE`). Multi-write editors (`:w` on a
  buffer that was also auto-saved) emit several events in rapid succession; the
  debounce coalesces them into one sidebar refresh.

- **Non-fatal startup.** If `watch_config` fails (e.g. inotify limit reached),
  the error is logged to stderr and the app continues with manual refresh only.
  A watcher failure is not a reason to refuse to start.

- **Desktop shell scope.** The watcher lives in `apps/desktop/src-tauri`, not in
  `remora-core`. Core and protocol remain transport-agnostic; the relay can add
  its own watcher if and when it has a config file to watch.

- **Sidebar only.** The Settings editor (once it exists) is not live-refreshed
  from the watcher — doing so risks clobbering in-progress edits. The Settings
  form remains explicitly save-gated.

## Alternatives considered

- **`tauri-plugin-fs` watch.** The Tauri filesystem plugin exposes a
  `watch` API that wraps `notify`. Using it would import the entire fs plugin
  with its capability-system surface (read, write, scope) and require additional
  `capabilities` config — significant blast radius for a single-file watcher.
  Rejected in favour of the `notify` crate directly.

- **Emit the full `ConfigDto` as the event payload.** Loading and projecting the
  config on the watcher thread duplicates logic already in `config_get`, couples
  the event's wire shape to `ConfigDto`, and requires the event to stay in sync
  with future schema changes. The ping approach delegates reading to the existing
  command path. Rejected.

- **Watcher in `remora-core` for relay-mode parity.** YAGNI — the relay does
  not exist yet, and its config-change story is not designed. Keeping the watcher
  in the desktop shell avoids leaking a desktop concern into a shared crate.
  Rejected; the relay can grow its own when it needs one.

- **Live-refresh the Settings editor.** Would risk overwriting text the user is
  actively editing (race between keystrokes and an external save). Out of scope
  for this issue; the Settings form remains manually saved.

- **Do nothing (manual refresh only).** The status quo. Rejected because the
  discrepancy between the file on disk and the sidebar is user-visible from the
  first hour of use for early adopters who edit config by hand.

## Consequences

Easier:
- External config edits (new host, new project) appear in the sidebar within
  ~500 ms of saving, with no user action.
- The push-event precedent (`collect_events!`, `events.<name>.listen`) is
  established: future Rust→frontend events (activity state from #69, relay
  connection status) follow the same pattern.
- The watcher logic is decoupled from Tauri (callback injection) and is
  fully unit-tested without an `AppHandle`.

Harder, and what we commit to / must still resolve:
- **One background watcher thread** is added for the app's lifetime. On app exit
  the process tears it down; no explicit shutdown is needed.
- **New dependencies:** `notify` and `notify-debouncer-full` in
  `apps/desktop/src-tauri/Cargo.toml`.
- **Settings editor stays manual.** A future GUI config editor that wants
  live-reload from disk will need to solve the concurrent-edit race separately.
- **Smoke test pending.** `pnpm dev` requires a display server not available in
  the CI sandbox; end-to-end verification (edit config → sidebar updates) is
  deferred to dogfood on hermes.
