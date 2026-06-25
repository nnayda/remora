# 0012. Detect agent activity client-side in the attached terminal, quiescence-primary

- **Status:** Accepted
- **Date:** 2026-06-25
- **Issue/PR:** [#55](https://github.com/nnayda/remora/issues/55);
  builds on [ADR-0010](0010-in-band-activity-osc-marker.md),
  [ADR-0003](0003-agent-agnostic-sessions.md)

## Context

ADR-0010 proved the OSC-7366 marker survives the tmux → ssh byte path and can
carry agent-state signals in-band with nothing extra on the sandbox. It left two
questions open for #55:

1. **Where does the parser run** — in the client's streaming terminal, or in a
   core/relay-held persistent attach?
2. **How is "agent is thinking" distinguished from "agent has finished and is
   waiting for you"** — the two hardest states to separate from a raw byte
   stream?

#55 needs a user-visible per-session activity indicator (spinner / blue / red dot
in the sidebar and tab strip) for the desktop-direct MVP. The quiescence and
marker channels are both available (ADR-0010); the design must decide which runs
the parser and which signals carry authority over which states.

Two constraints shape the answer. First, `ChannelOutput` is opaque by policy
(ADR-0003, ARCHITECTURE.md): core and protocol treat the agent as an opaque PTY
CLI, and adding a byte-inspection seam on the hot path was explicitly not de-risked
by the spike. Second, xterm (`@xterm/xterm`) already reassembles OSC sequences
across PTY read boundaries (tested in the spike) and raises a clean handled/ignored
callback — the parsing infrastructure is already in `TerminalController`'s xterm
instance.

A third practical fact resolves the coverage question: all open tabs stay mounted
(`App.tsx:277` hides inactive tabs with `display:none` rather than unmounting
them), so every open session has a live `TerminalController` and xterm parser
regardless of which tab is foreground. Client-side detection therefore covers all
open sessions in the sidebar automatically.

## Decision

We will run activity detection **client-side, inside each tab's `TerminalController`**,
using two cooperating signals managed by a shared `ActivityStore`:

- **Quiescence (byte-arrival) is the primary live detector.** `TerminalController`
  calls `store.noteOutput(key)` on every `bytes` message from the session. The
  store's state machine transitions to `working` on the first bytes and then to
  `idle` after `SETTLE_WINDOW_MS` of silence (conservative — named, not magic).
  Blue (`idle`) means "output has paused", not "agent has definitely finished" —
  the UI tooltip and ADR copy must say "paused", not "done".

- **OSC-7366 marker is parsed and wired, but dormant.** `TerminalController` also
  registers an OSC-7366 handler (via `registerOscHandler`) that calls
  `store.noteMarker(key, state)` when the marker fires. `parseActivityMarker`
  (pure, in `osc-marker.ts`) validates the grammar and sanitizes the decoded
  payload (untrusted — ADR-0010 threat model). The full marker pipeline is
  ready and tested; it is **dormant** because no agent hook currently emits the
  marker (emit wiring is a #55 follow-up, blocked on the launch-inject-vs-pre-config
  decision for the `Agent` schema).

- **Marker-only red.** `awaiting_input` (`MarkerState`) is **never inferred from
  a stable screen**. The state machine only enters `awaiting` when the OSC-7366
  marker explicitly carries `state=awaiting_input` and the session is settled. A
  quiescent terminal without a marker stays blue. This makes the red indicator a
  high-confidence signal (agent-asserted) rather than a guess.

- **`ChannelOutput` stays opaque; core stays agent-agnostic.** No byte-inspection
  seam is added to core or the relay. The detection runs entirely in the
  TypeScript layer on the decoded PTY stream.

- **No protocol surface in #55.** Cross-wire status, the structured event stream,
  input-gating, and detached-session detection (for un-opened sessions where no
  client is attached) are deferred to
  [#69](https://github.com/nnayda/remora/issues/69).

- **Duplication-then-migration is a conscious choice.** #69 needs a core/relay-
  side detector so that the phone-while-away hero works when no desktop client is
  attached. The client detector introduced here is the de-risked path for the
  direct-mode MVP (core-side parsing was not de-risked by the spike). The
  detection rules — marker grammar, quiescence state machine — are implemented as
  self-contained, well-tested TypeScript units (`osc-marker.ts`,
  `activity-store.ts`) so they port to Rust cleanly when #69 lands. The desktop
  will then migrate to consume #69's events, and the client detector will be
  retired. #69's body has been updated to reflect this sequencing so the two
  issues do not contradict.

## Alternatives considered

- **Core-side detector now.** Would add a byte-inspection seam to `ChannelOutput`
  (opaque by policy) and require headless attach for detached sessions — that is
  approximately #69's scope and was not de-risked by the ADR-0010 spike. Deferred.

- **Quiescence-only (drop the marker pipeline).** Loses the proven OSC-7366
  channel and makes the red "awaiting input" state impossible to surface with
  confidence. The marker pipeline ships ready even while dormant; the cost is
  negligible.

- **Observe xterm's `parsedWrite` / render callbacks.** Couples activity detection
  to the render cadence (batched frames, alt-buffer switches, animation redraws)
  rather than raw byte arrival, producing noisy false working→idle transitions.
  Byte arrival on the `bytes` message is the correct signal.

- **Per-session headless attach in core now.** Would enable detached detection but
  adds a persistent process per session on the relay — #69's design decision, not
  #55's.

## Consequences

Easier:
- Day-one indicator value: spinner vs blue is user-visible immediately without any
  agent-side hook configuration. Red is wired and will light up as soon as emit
  lands.
- `ChannelOutput` stays opaque; the ADR-0003 invariant is not touched.
- All open tabs are covered automatically by the always-mounted `TerminalController`
  design — no polling, no separate attach.
- The quiescence state machine and OSC parser are self-contained tested units,
  ready to port to Rust for #69.

Harder, and what we commit to / must still resolve:
- **Status covers only open tabs** (sessions that have been opened in a tab at
  least once during this desktop session). Un-opened or detached sessions show no
  activity state until #69 lands. This is the intentional #55 scope.
- **Emit wiring is a documented follow-up.** The `Agent` adapter config gains a
  declarative emit-hook schema (the launch-inject-vs-pre-config call, idempotency,
  version-tolerance) — tracked under #55. Until then, red is never shown.
- **Client detector is temporary.** The migration (#69 → desktop consumes core
  events → client detector retired) is a documented commitment, not optional
  cleanup.
- **kubectl arm unverified.** ADR-0010 proved the ssh path; the kubectl PTY path
  shares the same `open_pty` code but was not live-run against a real cluster
  (RBAC). Re-run tracked in ADR-0010's follow-ups.
- **OSC passthrough widens the terminal surface.** Enabling `allow-passthrough`
  means any process in the tmux pane can emit OSC sequences (OSC 52 clipboard,
  other vendor OSCs) that reach the client's xterm unfiltered. Acceptable under
  the app's own-sandbox threat model (the pane already runs trusted agent code),
  but noted for future relay / multi-tenant contexts.
