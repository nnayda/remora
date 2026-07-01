# 0019. Launch-time hook injection: `Agent.provision` + inline `--settings`

- **Status:** Accepted
- **Date:** 2026-07-01
- **Issue/PR:** [#196](https://github.com/nnayda/remora/issues/196); follow-up
  to [#61](https://github.com/nnayda/remora/issues/61) / #193, reserved by
  [ADR-0010](0010-in-band-activity-osc-marker.md) ("decide deliberately:
  launch-time injection vs pre-config"); see also
  [ADR-0018](0018-agent-prompt-preview-live.md) (deferred launch-time
  injection to this issue).

## Context

ADR-0010/0013 shipped the marker *consumer*: core parses an in-band OSC-7366
marker into `SessionStatus` + preview, and the desktop surfaces it (#146, #148,
#193/ADR-0018). Only the *emit* side stayed manual — a user hand-copies
`contrib/agent-hooks/claude-code/remora-notify.sh` into the sandbox and
hand-edits `~/.claude/settings.json` to wire a Notification hook to it
(`docs/agent-hooks.md`). ADR-0018 explicitly deferred launch-time injection and
an `Agent`-config marker schema to this issue.

Manual install has two costs: every sandbox repeats the same manual step
(Remora doesn't "own" the marker it defined), and a misconfigured or missing
hook is a silent no-op — no preview, no signal anything is wrong.

This issue splits into two PRs. **PR1 (this ADR)** is the provisioning schema
+ launch-time injection + a default-on desktop template — it kills the manual
step and makes markers work by default. It does *not* address the
silent-misconfig pain; that is PR2 (see Consequences).

## Decision

- **Launch-time injection over pre-config.** Remora writes the hook to the
  sandbox itself, at spawn time, rather than requiring it to pre-exist (the
  gap ADR-0010 flagged as needing a deliberate call). Remora now owns
  installing the marker it defined.

- **Generic single-file provisioning over a typed marker/hook schema.** `Agent`
  gains one new field: `provision: Option<ProvisionFile>`
  (`crates/remora-core/src/config/mod.rs`), where `ProvisionFile { path,
  content, mode: Option<u32> }` is **opaque bytes to a sandbox path** — core
  never parses `content` or learns "hook," "Claude," or the marker wire
  format. This keeps core agent-agnostic (`docs/ARCHITECTURE.md`'s "the one
  rule"): the *capability* ("materialize one file before launch") is generic;
  the *recipe* (what file, what content) is per-agent data supplied by the
  config or the desktop's authoring surface. A `Vec<ProvisionFile>` was
  considered and rejected for PR1 — since settings are passed inline (below),
  provisioning installs exactly one always-identical file; a list only adds
  `toml_edit` array-of-tables authoring hazard for a need that doesn't exist
  yet. Grow to a list only when a real second file appears.

- **Inline `claude --settings '{…}'` + a Remora-owned provisioned script, over
  merging into the user's real `~/.claude/settings.json`.** `--settings`
  accepts inline JSON and is documented to layer on top of file-based settings
  (the user's `model`, `permissions`, `env`, MCP servers survive; Remora's JSON
  sets only `hooks.Notification`). This means the only *file* provisioned is
  the notify **script** (`~/.remora/hooks/claude-notify.sh`, mode `0o755`) —
  there is no `--settings <path>` and no read-modify-write of a file Remora
  doesn't own. The hook `command` inside the inline JSON references the script
  via `$HOME` (`$HOME/.remora/hooks/claude-notify.sh`), not `~` — Claude Code
  runs hook commands through `sh -c`, which expands `$HOME` but does not
  guarantee tilde expansion. The script's own destination path is written via
  the existing `quote_remote_path` (`~/…` → `"$HOME"/…`), so the write path and
  the read path resolve identically; a test pins this.

- **Non-fatal `StepId::Provision` batch step, best-effort, before
  `new_session`.** The #182 spawn batch (`transport/batch.rs`,
  `transport/remote.rs`) is a closed `StepId` enum with a fail-closed parser
  and per-step `fatal: bool` under `StopOnError`. Injection is a new
  `StepId::Provision` variant (token `"provision"`), not a prepended raw
  string. `build_spawn_steps` orders it immediately before `NewSession`
  (`Fetch → WorktreeAdd → [Provision] → NewSession → Passthrough → SetEnv×N`)
  and marks it `fatal: false` — explicitly mirroring `Passthrough`'s
  contract: a write failure (missing `base64`, full disk, permissions)
  degrades to no-marker, exactly like today's manual-install failure mode, and
  never aborts the spawn. Content is base64-encoded locally and decoded on the
  sandbox (`printf %s '<b64>' | base64 -d > <path> && chmod <mode> <path>`) so
  arbitrary bytes — control characters, quotes, newlines — survive exactly,
  sidestepping shell-quoting hazards. Runs unchanged on both `ssh` and
  `kubectl` transports (shared batch assembly); `respawn` re-provisions
  through the same batch (idempotent — content is identical every spawn).

- **`contrib/agent-hooks/claude-code/remora-notify.sh` stays the single source
  of truth; the desktop template embeds a drift-guarded copy.** The recipe
  strings (script body, inline settings JSON, provision path) live in the
  frontend only (`apps/desktop/src/config-editor-model.ts`,
  `claudeMarkerTemplate()` / `applyClaudeTemplate()`), wired to a
  **"Claude Code (activity markers)"** button on the new-agent form
  (`AgentForm.tsx`). Core stays agent-agnostic; the desktop is a config
  *authoring* surface and may be agent-aware (the user already types
  `claude`). A Vitest drift-guard
  (`apps/desktop/src/claude-template.drift.test.ts`) imports the real contrib
  file via Vite's `?raw` and asserts the embedded template's script body is
  byte-identical (newline-tolerant) to it, so the two copies can't silently
  diverge.

- **The undocumented hook-merge caveat (D6).** Whether a `--settings` `hooks`
  object *shadows* a user's own `Notification` hook (deep vs. shallow merge)
  is not documented upstream. Risk is judged low in a coding sandbox, and the
  recipe's runtime behavior is already proven end-to-end (#193, `marker.rs`
  wire-contract tests) — injection only changes *who installs it*, not how it
  runs. This is not treated as a PR1 blocker; it's documented as a caveat in
  `docs/agent-hooks.md` and is to be verified empirically in the hermes
  dogfood (manual GUI verification, tracked, not yet performed as of this
  ADR).

## Consequences

- Creating a Claude Code agent from the desktop template is now zero-setup:
  markers work without touching the sandbox by hand. The manual
  copy-paste-into-`~/.claude/settings.json` recipe becomes a documented
  fallback/override, still valid for non-Claude agents or hand-rolled setups.
- `Agent` config gains `provision` under `#[serde(default)]`, so existing
  `[agents.x] command = [...]` configs keep parsing unchanged (`provision =
  None`); the format-preserving TOML writer round-trips a
  `[agents.<id>.provision]` sub-table by writing `content` as a plain string
  value (`toml_edit`'s `value()` helper), which the serializer renders as an
  escaped basic string — backslashes, quotes, and newlines in the script are
  auto-escaped, not hand-escaped by us — and this round-trip is unit-tested
  for byte-identical content.
- **Deferred (follow-up issues, tracked per AGENTS.md):**
  - **The misconfig/"did a marker ever arrive" diagnostic (PR2).** Its trigger
    signal needs its own design: "agent has `provision` ⇒ expects a marker" is
    a flawed proxy — provisioning a file is not the same as a marker actually
    emitting (a `tmux < 3.3` passthrough failure also yields no marker and
    would mis-blame the hook). PR2 must pick a sounder signal before shipping
    a diagnostic.
  - A rich per-file provision editor in the desktop form (today `provision`
    round-trips untouched through edit; TOML hand-editing covers power users
    meanwhile).
  - Edit-mode template access (today the template button only appears on
    agent *creation*, not edit).
  - Multi-file provision (`Option<ProvisionFile>` → a list) — only if a real
    second file appears; no evidence of need yet.
- **Limitations:** the hook-shadowing behavior (D6) is empirically unverified
  as of this ADR (hermes dogfood pending); Claude Code's Notification text
  remains generic and fires for the idle nag, both already noted in
  `docs/agent-hooks.md`.
- **See also:** [ADR-0010](0010-in-band-activity-osc-marker.md) (the marker
  wire format and tmux-passthrough requirement this ADR's provisioned script
  still relies on), [ADR-0018](0018-agent-prompt-preview-live.md) (the
  consumer side this ADR's injection feeds; it deferred launch-time injection
  here). Neither is edited by this ADR — ADRs are append-only.
