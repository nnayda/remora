# 0019. Confirm activity-hook installs with a positive affirmation, never a silence accusation

- **Status:** Accepted
- **Date:** 2026-07-01
- **Issue/PR:** [#198](https://github.com/nnayda/remora/issues/198); builds on
  [ADR-0010](0010-in-band-activity-osc-marker.md),
  [ADR-0013](0013-core-side-activity-detector.md),
  [ADR-0018](0018-agent-prompt-preview-live.md)

## Context

ADR-0013 infers `working`/`idle` from raw byte quiescence; ADR-0018 made the
agent-prompt preview live by having a hook recipe emit an in-band OSC marker
on Claude Code's `Notification` event. Both leave one gap ADR-0018 named as a
deferred follow-up: **a misconfigured or missing hook is a silent no-op.** No
preview ever shows, but the byte/quiescence heuristic keeps working
regardless, so an `idle` session with no preview is indistinguishable between
two very different states — (A) the marker pipeline never fires (broken
install), or (B) the hook is fine but the agent simply hasn't asked anything
yet. #198 asks for a lightweight "did a marker ever arrive on this session"
signal that separates A from B.

The recipe we ship (`remora-notify.sh`) fires only on `Notification`, so a
latch that only counts that marker can't flip until the agent's first
question — it can't prove a correct install *before* that point. A liveness
heartbeat is needed. The first design drafted for this heartbeat surfaced it
as a passive accusation ("hook not detected" once idle with no marker seen).
Eng-review's outside-voice pass caught a load-bearing defect in that framing:
`tmux attach-session` repaints the pane on every reconnect (bytes flow →
`working` → settle → `idle`) while firing **no** Claude Code hook —
`SessionStart` fires on Claude *process* start, not terminal re-attach, and
`UserPromptSubmit` needs the user to type something first. So a perfectly
healthy session, freshly reconnected and merely being looked at, sits at
`idle` with no marker seen this attach — exactly the state a silence
accusation would flag as broken. Absence of a marker is genuinely ambiguous
(broken vs. quiet-but-fine); presence is not. This forced a pivot.

## Decision

We add a **payload-free liveness marker** and latch, and surface confirmation
only as a **positive affirmation on presence**, never as a message on absence.

- **New marker type `ping`.** `MarkerHit` is restructured from a struct into
  an enum so a ping can carry no status:

  ```rust
  pub enum MarkerHit {
      State { status: SessionStatus, preview: Option<SanitizedText> },
      Liveness,
  }
  ```

  `parse_marker` gains a `TYPE_PING` branch; unknown types still drop silently
  (ADR-0010's threat model is unchanged). A `Liveness` hit carries no status or
  preview — it exists solely to prove the marker pipeline is alive.

- **`Detector` gains a `marker_seen: bool` latch.** In `on_bytes`, the first
  hit of *any* variant — `State` or `Liveness` — flips the latch false→true
  and emits a one-shot `DetectorEvent::MarkerSeen`. `State` hits still drive
  `Status`/`Preview` exactly as before; a lone `Liveness` hit only latches, it
  never changes `Status`. The latch is idempotent: once true it stays true for
  the life of the attach and never re-emits.

- **Protocol gains `ChannelOutput::MarkerSeen`** (a unit variant), and
  `PROTOCOL_VERSION` bumps **1 → 2**. This bump is bookkeeping, not
  enforcement: `channel.rs` documents the convention that adding a variant
  bumps the version, and this change honors that convention so a future
  version handshake has an accurate number to read. **Nothing reads
  `PROTOCOL_VERSION` today — there is no handshake.** The actual runtime
  fail-closed behavior — an older peer erroring out on an unrecognized
  `marker_seen` tag instead of silently misinterpreting it — comes from serde's
  **externally-tagged** representation of `ChannelOutput`: deserializing an
  unknown variant tag is a hard error, with no `#[serde(other)]` catch-all to
  swallow it. (`#[non_exhaustive]` is a separate, Rust-side concern — it forces
  downstream `match`es to carry a wildcard arm; it does not affect wire
  deserialization.) Do not read this ADR as claiming the version bump itself
  enforces compatibility; it doesn't, yet.

- **Recipe: `contrib/agent-hooks/claude-code/remora-ping.sh`**, wired to
  Claude Code's `SessionStart` *and* `UserPromptSubmit` events, writing the
  wrapped marker to `/dev/tty` (never stdout — see `docs/agent-hooks.md`).
  `SessionStart` proves the hook the moment a fresh session boots (the
  primary win: state B is resolved before the agent's first question).
  `UserPromptSubmit` re-earns the affirmation on the next interaction after a
  reconnect, since `SessionStart` does not fire on re-attach and the latch
  resets per attach.

- **Surfacing: a positive tooltip line, never a negative one.** The
  `SessionRow` native `title` gains "Activity hook active" whenever
  `markerSeen` is true for that session, with no gating on `working`/`idle`/
  `unknown` — presence is unambiguous, so it's safe to show in any state. When
  `markerSeen` is false the tooltip says **nothing** about the hook at all; it
  falls back to the existing stopped-state hint or `awaiting` preview
  unchanged. This reverses the original "hint when not detected" plan.

## Alternatives considered

- **Passive "hook not detected" accusation once idle with no marker seen** —
  the original design. Rejected in eng-review: it false-positives on every
  reconnect (`tmux attach` repaints the pane into `working`→`idle` while
  firing no Claude hook), which would train users to distrust or ignore the
  signal. Dropping it also removed a layer of idle-gating and attach-race
  reasoning the accusation design needed — the positive-only design is
  strictly simpler.
- **Latch existing markers only, no dedicated `ping`** — can't distinguish a
  broken hook from "hasn't asked yet" before the agent's first question,
  which is the primary case #198 asks to resolve. Rejected.
- **Active hook probe / `remora doctor`** (spawn a throwaway probe session, or
  inspect the sandbox's agent config to verify the hook is wired) — the most
  conclusive alternative, but a larger, differently-shaped design that also
  pushes against ADR-0003 (core stays agent-agnostic; this would need
  agent-specific config parsing). Deferred as a follow-up candidate if the
  passive affirmation proves insufficient in dogfood.
- **A structured `PROTOCOL_VERSION` handshake** — out of scope here; the bump
  is kept only to maintain `channel.rs`'s existing convention, not to gate
  anything today.

## Threat model

The `ping` marker is untrusted, exactly like every other marker (ADR-0010):
anything running in the sandbox can forge one. A forged `ping` can only
**assert a false "hook active" affirmation** — it fails toward a
false-positive, never toward a false negative. Because `Liveness` carries no
status or preview, a forged ping can never fabricate `working`/`idle`/
`awaiting` state; the worst it can do is make a broken install look confirmed.
This is acceptable for a diagnostic: a sandbox capable of forging the ping is
already capable of forging the real activity/preview markers, and the
affirmation drives no trust-bearing action — it's an informational tooltip
line, not a gate on anything.

## Consequences

- A correctly installed hook proves itself within the first settle window
  after `SessionStart`, before the agent has to be asked anything — resolving
  the primary ambiguity #198 raised.
- **Per-attach limitation:** `markerSeen` resets on detach and is re-earned by
  the next marker (a `UserPromptSubmit` ping or a `Notification`) after
  reconnect. Persisting "this session's hook is known good" across attaches
  needs the relay (#68); out of scope here, same boundary ADR-0013 drew for
  status.
- No new visible chrome: the affirmation rides the existing native tooltip
  (ADR-0018's tooltip-first decision), so there is no DESIGN.md change.
- **Follow-up candidate:** an active hook probe / `remora doctor` check,
  tracked as a future issue if dogfood shows the positive affirmation isn't
  discoverable or conclusive enough on its own.
