# 0006. The app writes the config file through a validated editor channel

- **Status:** Accepted
- **Date:** 2026-06-18
- **Issue/PR:** #32

## Context

[ADR-0004](0004-local-config-live-session-discovery.md) made hosts, projects,
and agents declarative local configuration in one human-editable TOML file, and
deliberately made the config types *deserialize-only*: the `remora-core::config`
module doc states "the app never rewrites this file." The only way to add or
change a host/project/agent was to hand-edit the TOML and restart.

That is a poor fit for a desktop app whose whole job is spawning sessions on
configured hosts: first-run setup and "add another host" should be doable in the
UI (#32). Hand-editing is also error-prone, and a single typo currently has no
in-app recovery. Reversing the "never writes" decision is an architectural
change, so it gets its own ADR rather than a rewrite of ADR-0004.

Two tensions have to be resolved by this decision:

- The bridge already exposes a **redacted** display projection of the config
  (`ConfigDto`) that deliberately omits connection secrets, so the sidebar never
  receives them. Editing a host needs those exact values back, which the display
  channel must not carry.
- ADR-0004 keeps connection details client-side and off the future relay wire.
  A management surface that round-trips full connection details must not become
  a way for secrets to cross that wire.

## Decision

We will let the app **write** the per-device config through a new, validated
write-back path, kept strictly separate from the redacted display path.

- **`remora_core::config::ConfigDocument`** wraps a `toml_edit` document so edits
  preserve the user's comments and formatting. It exposes explicit
  `insert_*` / `update_*` / `remove_*` mutators for hosts, projects, and agents.
  `insert_*` fails if the id already exists and `update_*` fails if it does not,
  so a create can never silently overwrite an existing entry. Every mutation on a
  valid base re-validates by round-tripping through the existing
  `Config::from_toml_str`, so the single existing validation path is the only
  source of truth and referential integrity (e.g. refusing to delete a
  referenced host) falls out for free. `save` writes atomically via a sibling
  temp file + rename (`tempfile`), created `0600` on unix.
- **`parse_lenient`** opens a syntactically-valid but semantically-invalid file
  in a degraded mode that skips whole-document re-validation, so the user can
  delete or replace the one broken entry that prevents the file from loading
  instead of being sent back to a text editor.
- **A separate, local-only editor channel** carries full connection values
  (`EditorConfigDto` + `config_get_editable` + the mutation commands) for the
  management UI. The redacted `ConfigDto` display path is unchanged. The editor
  channel is **local-only and must never be exposed through the relay**;
  connection details stay client-side per ADR-0004.

Config ids remain immutable join keys (ADR-0004): the UI offers create-with-id,
rename-display-name, and delete, never id editing.

## Alternatives considered

- **Keep config read-only; add an "open in $EDITOR" button.** Smallest change,
  but it does not satisfy in-app create/edit, leaves first-run as hand-edited
  TOML, and offers no structured validation feedback.
- **Regenerate the whole file from the typed `Config` on save.** Simpler than
  `toml_edit`, but it discards every comment and normalizes formatting on the
  first save — hostile to a dotfiles-friendly, hand-edited file.
- **Reuse the redacted `ConfigDto` for editing.** Would either leak secrets onto
  the display channel or make edit forms unable to show current values. Keeping
  two explicit channels is the honest split.
- **Do nothing.** Leaves the only configuration path as hand-editing a TOML file
  and restarting — the gap #32 exists to close.

## Consequences

What becomes easier:

- First-run setup and host/project/agent management happen in the app; a typo in
  an otherwise-valid edit is rejected with a reason instead of corrupting the
  file.
- Validation lives in exactly one place; mutators cannot drift from load-time
  rules.

What becomes harder, and what we are committed to:

- The `remora-core::config` module doc no longer says "never rewrites"; it points
  here. ADR-0004 stays intact and is referenced, not rewritten.
- The editor channel is a second place connection secrets exist in memory at the
  bridge boundary. It is local-only by construction (Tauri IPC) and guarded so it
  can never share a code path with the redacted display DTO; it must never be
  surfaced through the relay milestone.
- Writes are serialized within the app process (a bridge mutex); cross-process
  and cross-device write safety are **out of scope** and deferred to the relay
  milestone, consistent with ADR-0004 deferring config sync.
- File permissions are hardened only on unix (`0600`). On Windows the file
  inherits default ACLs; secrets-at-rest hardening there is a known, accepted gap
  to revisit if Windows becomes a shipping target.
