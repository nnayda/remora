# 0013. Run the activity detector core-side as the sole output_tx sender

- **Status:** Accepted
- **Date:** 2026-06-26
- **Issue/PR:** [#69](https://github.com/nnayda/remora/issues/69);
  implements the byte-inspection seam deferred by [ADR-0012](0012-client-side-activity-detection.md);
  builds on [ADR-0010](0010-in-band-activity-osc-marker.md),
  [ADR-0003](0003-agent-agnostic-sessions.md)

## Context

ADR-0012 introduced a client-side activity detector for the direct-mode MVP
(#55) and explicitly deferred two things to #69: (1) adding a byte-inspection
seam to `ChannelOutput` (which ADR-0003 and ARCHITECTURE.md hold opaque), and
(2) the core/relay-side detector needed for the phone-while-away hero, where no
desktop client is attached when the agent posts a marker.

ADR-0010's follow-up list also left two protocol costs unsettled: the
`ChannelOutput` variant(s) for surfacing activity events, and the
`PROTOCOL_VERSION` bump those variants force.

Issue #69 resolves both: it adds the byte-inspection seam in the one place where
ADR-0003 permits it (core's own PTY bridge, not the transport-layer interface),
extends `ChannelOutput` with two typed variants on the existing stream, and runs
a single detector thread as the sole sender to `output_tx` — which is what
preserves both byte→status ordering and the `recv()→None` teardown signal. This
also supersedes ADR-0010's open "where does the parser run" question for the
attached path (it runs core-side, via `vte`); the client-side TS parser is
retired in PR2.

## Decision

### Protocol surface — `ChannelOutput` extended, `PROTOCOL_VERSION` 0→1

We will add two new variants to `ChannelOutput` in `remora-protocol`:

```rust
StatusChange(SessionStatus)   // working / idle / awaiting
PreviewUpdate(String)         // sanitized agent-claimed message text
```

alongside the existing `Bytes(Vec<u8>)`. `SessionStatus` is a new enum
(`Unknown | Working | Idle | Awaiting`) in `remora-protocol`. These variants
ride the **same channel** as `Bytes` — one `mpsc` receiver, ordered delivery —
which is the load-bearing property (see single-sender rationale below).
`PROTOCOL_VERSION` advances from 0 to 1 because this is a breaking change:
`ChannelOutput` is an externally tagged serde enum, so an older peer **fails
closed** on the unknown `status_change`/`preview_update` variants rather than
skipping them — compatibility is gated on the version bump, not on silent
tolerance. That is deliberately distinct from the OSC **marker** parser, which
*does* silently ignore an unknown `<type>`/`<ver>` (ADR-0010), because a forged
or garbage marker must never produce a false state.

`SessionStatus` is **not** added to `SessionMeta` or `list()` in this issue.
Status is attached-only in PR1: it flows to consumers that hold an open
`SessionChannel`, not to the discovery/enumeration path. Persisting status
across detached sessions is deferred to the relay (#68).

### Pure `Detector` in `crates/remora-core/src/activity/`

We will implement a pure, **clock-free** `Detector` struct (a port of the
TypeScript state machine in ADR-0012 into Rust):

- `on_bytes(&[u8]) → Vec<DetectorEvent>` — feeds the chunk to a `vte`-backed
  `MarkerScanner` and emits status/preview events. Bytes drive the state to
  `Working` unless a marker in the same chunk asserts a different state (the
  last marker in a chunk wins). The detector uses `vte` (newly added in this
  change) for correct incremental OSC assembly across PTY read boundaries, with
  a free DoS bound (bounded internal buffer, ignores oversized sequences).

- `on_tick() → Vec<DetectorEvent>` — called by the bridge thread after one
  settle window of silence; transitions `Working → Idle` and emits a single
  `Status(Idle)` event. `Awaiting` is **never inferred from quiescence** — it is
  marker-only.

The `Detector` holds no `Instant`, no `sleep`, no `Arc`. The settle clock lives
entirely in the bridge thread's `recv_timeout` call.

### Single detector thread — the sole sender to `output_tx`

The PTY bridge (`transport::pty_process`) runs **two** threads:

1. **Reader thread.** Reads PTY bytes and forwards raw `Vec<u8>` chunks to an
   internal bounded channel (`raw_tx → raw_rx`). It never touches `output_tx`.

2. **Detector thread.** Calls `raw_rx.recv_timeout(settle)`:
   - `Ok(bytes)` → forwards `ChannelOutput::Bytes` unchanged, then feeds
     `detector.on_bytes(&bytes)` and forwards any resulting `StatusChange` /
     `PreviewUpdate` messages — all on `output_tx`, in that order.
   - `Err(Timeout)` → calls `detector.on_tick()` and forwards any resulting
     `StatusChange(Idle)` — the settle window has elapsed.
   - `Err(Disconnected)` → `raw_tx` was dropped (reader EOF); the detector
     thread exits, dropping `output_tx` — the caller's `recv()` returns `None`,
     the standard teardown signal.

**Why the detector thread must be the sole `output_tx` sender:**

A second sender (e.g., a companion sweeper thread holding a cloned `output_tx`)
would break two invariants:

1. **Byte→status ordering.** `StatusChange` messages must follow the `Bytes`
   message(s) that caused them on the same `mpsc` channel, or a consumer could
   see a status flip before the bytes that triggered it. With one sender that
   inserts both in the same loop iteration, this is guaranteed without any lock.
   With two senders the interleaving is non-deterministic.

2. **`recv()→None` teardown.** `mpsc::Receiver::recv()` returns `None` only
   when **all** senders have been dropped. A cloned `output_tx` held by a
   sweeper thread would keep the channel open after the reader exits, stalling
   the teardown path. The single-sender design means dropping `output_tx` at the
   end of the detector thread is the authoritative close signal — verified by a
   dedicated teardown test in `pty_process.rs`.

The `plan-eng-review` for this issue explicitly rejected the companion-sweeper
alternative on these grounds.

### OSC-7366 parsing via `vte` + base64

`MarkerScanner` wraps a `vte::Parser` and accumulates OSC 7366 sequences
across PTY read boundaries. On a complete sequence it validates the `remora`
token, version, and type fields, then base64-decodes the state and optional
preview fields. `sanitize()` (`crates/remora-core/src/activity/sanitize.rs`)
strips C0/C1 control sequences and length-caps the decoded text before it
becomes a `SanitizedText`. The raw `Bytes` payload is forwarded unchanged to
the caller — core never strips the marker from the byte stream.

### Agent-agnostic argument — one-rule compliance

OSC-7366 is a **Remora wire convention**: the code `7366` and the mandatory
`remora` token are defined by Remora, not by Claude Code or any other agent.
The agent's lifecycle hook merely `printf`s a string that Remora specified — the
hook content is per-agent adapter data (ADR-0003), but the recognition code
matches a Remora-defined format, not an agent-specific one. `MarkerScanner`
never learns what agent is running; it pattern-matches `OSC 7366 ; remora ; …`
and raises a typed event. Core remains fully agent-agnostic.

### `awaiting` is marker-only; preview is dormant

`SessionStatus::Awaiting` is never emitted from `on_tick()`. A quiescent
terminal without a marker stays `Idle`. This keeps the red indicator
high-confidence (agent-asserted), matching the policy from ADR-0012.

`PreviewUpdate` events are emitted when the OSC-7366 `state` marker includes a
base64 preview field. The events are test-verified in this issue but are
**dormant end-to-end** until an emitter lands under #61 (the `Agent` adapter
hook that `printf`s the marker with a preview segment). The desktop bridge
(`BridgeOutput`) and the frontend will map them in PR2.

### One-window / one-pane passthrough constraint

As stated in ADR-0010: `allow-passthrough` is set on Remora's own tmux session,
not the user's. Markers fired from a background tmux window are silently
dropped by tmux before they reach the PTY reader. Remora's current
one-session=one-window=one-pane topology means every byte the reader sees comes
from the foreground pane. A future multi-window layout must add a guard.

### Scope boundary — attached-only, relay deferred

Status events flow to consumers that hold an open `SessionChannel` (e.g., the
Tauri bridge thread in PR2). No polling path, no `SessionMeta` field, no relay
persist. The persistent headless attach that powers the phone-while-away hero —
where the relay must park an attach even when no client is connected — is
deferred to #68. The protocol surface added here (`StatusChange`,
`PreviewUpdate`, `PROTOCOL_VERSION 1`) is designed to be relay-transparent: the
relay will forward these variants on the same channel without needing to
understand them.

## Alternatives considered

- **Separate event channel (`mpsc::Receiver<ActivityEvent>`) beside `Bytes`.**
  The caller would hold two receivers and `select!` across them. This breaks
  ordering (a `StatusChange` can race ahead of the `Bytes` that caused it) and
  doubles the surface the relay must forward. Rejected; single-stream ordering
  is the relay-era property to preserve.

- **Companion sweeper thread + `Arc<Mutex<Detector>>`, cloned `output_tx`.**
  The sweeper would call `on_tick()` on a timer and send `StatusChange(Idle)`
  directly to `output_tx`. Two problems: (1) the cloned `output_tx` breaks the
  `recv()→None` teardown (the channel stays open until the sweeper is also
  joined); (2) the sweeper and detector thread interleave their writes to
  `output_tx`, violating byte→status ordering. Rejected explicitly in
  plan-eng-review.

- **Hand-rolled OSC scanner (no `vte` dependency).**
  The sequences split across PTY read boundaries in non-obvious ways, and a
  naive `memchr`-based scanner would need to re-implement the same incremental
  state machine `vte` already provides. Adding `vte` (a small, proven
  terminal-parser crate) gives correct incremental assembly and caps buffer size
  automatically (DoS bound), so the one new dependency is worth it versus
  re-implementing and maintaining the same state machine by hand.

- **Byte-scraping the screen snapshot for preview text.**
  Would require maintaining a full VT100 screen model and scanning for
  Claude-specific UI patterns — exactly what ADR-0003 prohibits (agent-specific
  knowledge in core) and what ADR-0010 rejected ("agent-declared output regexes
  matched on the raw screen stream … brittle … agent-version-fragile"). The
  explicit marker is unambiguous by construction.

## Consequences

Easier:
- The activity state machine is now core-side and transport-transparent: the
  relay (#68) can forward `StatusChange`/`PreviewUpdate` events without
  knowing what they mean.
- `ChannelOutput` carries exactly one receiver; ordering is a structural
  guarantee, not a convention.
- PR2 (desktop migration) can retire the client TypeScript detector
  (`activity-store.ts`, `osc-marker.ts`) and drive status from bridge events —
  simpler, no client-side parse race.
- The `Detector` and `MarkerScanner` are pure functions with no I/O; their unit
  tests cover state transitions, quiescence, marker-only awaiting, preview
  sanitization, and teardown without any process spawning.

Harder, and what we commit to / must still resolve:
- **PR1 events are unconsumed until PR2.** The Tauri `BridgeOutput` enum and
  the `forward_loop` in `bridge/mod.rs` do not yet map `StatusChange` /
  `PreviewUpdate`; those arms are added in PR2. Until then, the detector thread
  produces events that are silently discarded when `output_tx.blocking_send`
  returns `Err` (consumer dropped). No regression — the status indicator was
  previously client-side only.
- **Client TS detector retired in PR2 (committed).** `activity-store.ts`,
  `osc-marker.ts`, and their tests are deleted in PR2 once the bridge events
  are wired. This is a documented commitment from ADR-0012 ("The migration …
  is a documented commitment, not optional cleanup").
- **`SessionStatus` not on `SessionMeta`/`list()` yet.** Un-opened or detached
  sessions still show no activity state. The full status-in-list path requires
  the relay headless attach (#68).
- **Preview is dormant.** `PreviewUpdate` events are emitted by the detector
  when a marker with a preview field arrives, but no agent hook currently emits
  that field (#61). The infrastructure is tested; end-to-end preview requires
  #61's emit wiring.
- **kubectl arm unverified** (carried from ADR-0010). The ssh path is proven;
  kubectl shares the same post-attach `open_pty` PTY path but differs in PTY
  flags and was not live-run against a real cluster (RBAC). Re-run tracked in
  ADR-0010's follow-ups.
