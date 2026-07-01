# 0018. Make the agent-prompt preview live: wrapped marker + sidebar tooltip

- **Status:** Accepted
- **Date:** 2026-06-30
- **Issue/PR:** [#61](https://github.com/nnayda/remora/issues/61); builds on
  [ADR-0010](0010-in-band-activity-osc-marker.md),
  [ADR-0013](0013-core-side-activity-detector.md),
  [ADR-0003](0003-agent-agnostic-sessions.md)

## Context

ADR-0013 shipped the preview pipe (marker scanner → `PreviewUpdate` → bridge →
`ActivityStore.setPreview`) but left it dormant: nothing produced a marker with
a preview segment and nothing displayed it. #61 makes it live.

## Decision

- **Reuse `PreviewUpdate(String)`; no protocol change.** A single sanitized line
  carries the agent's question. A structured title/body and a `PROTOCOL_VERSION`
  bump were rejected (the carrier already exists; no consumer needs more yet).
- **Producer = a documented per-agent hook recipe** (`contrib/agent-hooks/
  claude-code/remora-notify.sh`), not launch-time injection. The marker is
  emitted in the **tmux-passthrough-wrapped** form (ADR-0010) and written to
  **`/dev/tty`** (Claude Code captures hook stdout, so stdout never reaches the
  PTY). Launch-time injection / an `Agent`-config marker schema is deferred.
- **Consumer = a durable sidebar-row tooltip**, not a toast or a new panel. The
  preview gets its own reactive snapshot in `ActivityStore` (status consumers
  stay churn-free) and renders as the `SessionRow` native `title`, framed
  *sandbox-claimed* ("the session says: …"). A toast self-dismisses before an
  away user returns and is more code; a dedicated Activity panel is its own
  feature, deferred until the producer reliably delivers good text and the relay
  (#68) exists.

## Threat model

The marker payload is untrusted and forgeable by anything in the sandbox. Core
sanitizes (control/bidi strip, incl. the U+2028/U+2029 line/paragraph
separators that `char::is_control()` misses) and length-caps (80) before the text leaves
`remora-core`; the UI renders it as sandbox-claimed and never wires an action to
it. Trusted facts (host/session identity) come from client state, never the
payload. This is the threat model ADR-0010 reserved for #61.

## Consequences

- The preview is live end-to-end once a user installs the hook; awaiting +
  preview shows on hover over the session row.
- **Deferred (follow-up issues):** right-side Activity panel; toast + phone push
  (post-relay); launch-time hook injection / `Agent` marker schema; a
  "did a marker arrive" hook-misconfig diagnostic.
- **Limitations:** Claude Code's Notification text is often generic and fires for
  the idle nag; a misconfigured hook is a silent no-op. Both noted in
  `docs/agent-hooks.md`.
- The end-to-end `/dev/tty` + tmux-passthrough + real-hook path is verified by
  manual hermes dogfood; automated tests pin the wire contract (the wrapped
  marker round-trips to the scanner).
