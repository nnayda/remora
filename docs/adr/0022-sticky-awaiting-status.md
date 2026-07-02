# 0022. `awaiting` is sticky: exits only via marker, user input, or teardown

- **Status:** Accepted
- **Date:** 2026-07-02
- **Issue/PR:** [#224](https://github.com/nnayda/remora/issues/224);
  amends the detector contract from [ADR-0013](0013-core-side-activity-detector.md)
  (which ported ADR-0012's policy); builds on
  [ADR-0010](0010-in-band-activity-osc-marker.md),
  [ADR-0018](0018-agent-prompt-preview-live.md)

## Context

`SessionStatus::Awaiting` is marker-only on entry (ADR-0012/0013): the
detector never infers it, so the red "needs you" pulse is high-confidence.
But nothing kept it alive. `Detector::on_bytes` flipped to `Working` on any
marker-less chunk, and the settle tick then decayed `Working → Idle`. While
an agent sits blocked at a prompt, cosmetic output keeps arriving anyway —
the tmux status-line clock repaints every minute, TUI agents repaint
spinners — so a correctly-fired `awaiting_input` marker degraded to a gray
`idle` pulse within ≤60s, exactly while the agent needed the user. The
desktop preview tooltip died with it: a status transition expires the stored
preview (by design — a preview belongs to its status episode, #61/#197), so
the spurious transition took the "the session says: …" text down too.
Observed in dogfood: a session at an interactive AskUserQuestion menu
showing a gray pulse and no tooltip.

## Decision

### The exit contract

While `state == Awaiting`, the detector leaves it only on:

1. **A state marker** — markers always win, exactly as before. An
   `awaiting_input` refresh re-asserts (and may carry a fresh preview);
   a `working`/`idle` marker exits.
2. **User input through Remora's own write path** — the one signal that
   unambiguously means "the user is responding", and already
   transport-agnostic. New: `Detector::on_user_input()` transitions
   `Awaiting → Working` (then normal settle decay); a no-op in every other
   state, where bytes already drive `Working`.
3. **Attach teardown** — the detector is per-attach; the desktop clears
   per-session state on detach (unchanged).

Marker-less output no longer exits `Awaiting`. The settle tick never did.
The detector stays clock-free, agent-agnostic, and still never *infers*
`awaiting` — this ADR only changes when it may be *left*.

### Bridge wiring: an atomic flag, not a new sender

The PTY bridge's writer thread sets one shared `AtomicBool` after each
**successful, non-empty** input write (a failed/torn-down write must not
read as "the user responded", and an empty write carries no user intent);
the detector thread swap-consumes it at every wake and dispatches through a
pure `wake_events(detector, user_input, bytes)` helper. Within a bytes wake,
the chunk dispatches **before** the input flag: a keystroke and an
`awaiting` marker can share one wake (type-ahead — the user answers just
before the marker arrives), and input-first would swallow the answer into a
stuck-red `Awaiting` nothing self-heals (a false hold, the worst direction
below); bytes-first lets the input exit whatever `Awaiting` stands at the
end of the wake, which at worst is a fail-soft false exit corrected by the
agent's next marker. On a silent wake with input, `on_user_input` runs
INSTEAD of the settle tick — typing counts as activity; settling in the
same wake would churn `Awaiting → Working → Idle` when a keystroke produces
no output (echo-off TUIs). `Resize` deliberately does not set the flag: it
causes repaints but is not the user answering.

`SeqCst` costs nothing that matters here (the store is keystroke-rate; the
swap is one RMW per wake, noise next to the chunk parse); correctness does
not depend on it — the flag is level-triggered and any later wake (including
a settle tick) observes it, so a missed pairing self-heals within one settle
window.

### Accepted approximation: the write path over-approximates "user input"

The write path carries everything the client terminal emits: keystrokes and
paste, but also terminal-initiated traffic — DSR/DA query replies, mouse
reporting (scrolling to *read* the prompt), focus-event reports,
bracketed-paste delimiters. Core cannot distinguish these without becoming
terminal-protocol-aware, which ADR-0003 forbids. The signal errs toward
exiting (fail-soft): a false "user input" while `awaiting` degrades to
exactly the pre-#224 behavior, and for permission prompts the Notification
hook's periodic nag re-asserts `awaiting`. A false *hold* would be worse —
a stuck-red pulse lies about needing attention.

### Ordering note

An input-caused `Status(Working)` may be emitted before cosmetic `Bytes`
that were already queued when the user typed. ADR-0013's byte→status
ordering guarantee is about *byte-caused* status events (a consumer must not
see a status flip before the bytes that triggered it); input-caused events
have no corresponding bytes, and status consumers record latest-state only.
The detector thread remains the sole `output_tx` sender.

## Alternatives considered

- **Writer thread sends a typed event into the raw channel (payload enum +
  cloned `raw_tx`).** The writer parks in `blocking_recv` until the caller
  sends or drops; its cloned `raw_tx` keeps the raw channel alive after
  reader EOF, so the detector never sees `Disconnected`, never drops
  `output_tx`, and an idle caller's `recv()` never returns `None`. Breaks
  the ADR-0013 teardown invariant. Rejected.
- **`Arc<Mutex<Detector>>` + cloned `output_tx` from the writer.** Already
  rejected by ADR-0013 (ordering + teardown).
- **Classify output (status-line vs pane content) so cosmetic bytes don't
  count.** Requires a tmux-layout-aware screen model in core — the wrong
  direction per ADR-0003 and #224's own design notes. Rejected.
- **Wall-clock decay of `awaiting` (e.g. auto-expire after N minutes).**
  Re-introduces a clock into a deliberately clock-free detector and makes
  the red pulse low-confidence again. Rejected.
- **Sequence the hook-side exit markers first, then make `awaiting` sticky**
  (raised by the cross-model review). Deliberately not taken: the stomp is
  the observed, user-visible bug; the desktop hero path is fully covered by
  the write-path exit; and detach/nag-markers bound the residual. The
  emitters land as the fast follow-up instead (see below).

## Consequences

Easier:
- The red pulse and its preview survive cosmetic terminal output; the #61
  tooltip lives exactly as long as the `awaiting` episode.
- `wake_events` makes the bridge's wake semantics pure and unit-tested.

Harder, and what we commit to / must still resolve:
- **Out-of-band answers don't exit** (typing in raw tmux on the host, or a
  future second client): no write-path signal fires, and today's hook
  recipe emits only `awaiting_input`, so `awaiting` can persist until the
  next marker or detach — an indefinite lie in the worst case, where the
  pre-#224 behavior lied within 60s. The completing half of this contract
  is hook-side `working`/`idle` exit markers (UserPromptSubmit /
  PostToolUse / Stop) — tracked as high-priority follow-up #239.
- **A future non-PTY transport (relay, #68) must plumb an equivalent
  user-input signal** or its `awaiting` exits will diverge from the PTY
  transports (ssh/kubectl share this one bridge today).
- **Reconnect does not restore `awaiting`** (pre-existing): markers are not
  replayed in tmux's attach repaint, so a fresh attach starts at `Unknown`
  even if the agent is still blocked. Unchanged by this ADR; the relay
  (#68) owns durable status.
