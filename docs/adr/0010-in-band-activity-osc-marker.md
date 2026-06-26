# 0010. Carry agent-activity signals in-band via a tmux-passthrough OSC marker

- **Status:** Accepted
- **Date:** 2026-06-24
- **Issue/PR:** [#67](https://github.com/nnayda/remora/issues/67) (spike);
  de-risks [#55](https://github.com/nnayda/remora/issues/55),
  [#61](https://github.com/nnayda/remora/issues/61)

## Context

A session today exposes only `Live`/`Stopped` — whether the tmux session exists
(`crates/remora-protocol/src/session.rs`). It says nothing about what the *agent*
is doing: thinking, idle with output ready, or blocked waiting for you. VISION.md's
relay milestone needs exactly this ("how does the relay detect 'agent is waiting
for you' … a status signal from the agent itself?", docs/VISION.md, Open
questions), and the phone hero wants the push to carry *what* is being asked
(#61), not just *that* something waits (#55).

The architecture constrains the approach. Core and protocol treat the agent as an
opaque PTY CLI; recognizing agent state is agent-specific knowledge that must live
as **per-agent adapter data, never code paths** (ADR-0003). The signal must also
work identically in direct and relay mode and add **nothing to the sandbox**
beyond what Remora already needs (tmux + git + agent). A private terminal escape
(OSC) is normally swallowed by tmux unless passthrough is enabled — but Remora
**spawns its own tmux session**, so enabling passthrough on *that* session at
create time is Remora configuring its own session, not the user's. Spike #67
built a real `tmux → ssh → @xterm/headless` harness and proved the marker
survives the byte path unwrapped, the envelope is consumed, a tricky payload
round-trips byte-exact, and nothing renders. This ADR records the mechanism the
spike settled and the design questions it surfaced for #55/#61.

## Decision

We will signal agent activity with an **in-band private-OSC marker** carried over
Remora's own tmux session, recognized by generic (never agent-specific) code:

- **Marker grammar.** Inner sequence, BEL-terminated; sent wrapped in tmux's
  passthrough envelope so tmux forwards it to attached clients and strips the
  envelope:

  ```text
  inner:    ESC ] 7366 ; remora ; <ver> ; <type> ; <base64-payload> BEL
  on-wire:  ESC P tmux ; ESC ESC ] 7366 ; remora;<ver>;<type>;<b64> BEL ESC \
  ```

  `7366` is our private code; the mandatory `remora` token is the real collision
  guard (the number is not formally allocated — the token, not the number,
  scopes matching). `<type>` is `state` (#55) or `notify` (#61); the payload is
  base64 so it can carry arbitrary bytes without colliding with the `;`
  separator or the C0 terminators.

- **Emit = per-agent data (ADR-0003).** An agent adapter declares a hook command
  that `printf`s the wrapped marker. No Remora binary runs on the sandbox; the
  marker is data the agent's own lifecycle hooks emit.

- **Enable passthrough on our own session.** Add `set-option -t <name>
  allow-passthrough on` to the atomic create-time trailer chain in
  `new_session_tokens` (`crates/remora-core/src/transport/remote.rs`), *after*
  the load-bearing `remain-on-exit` guard (a mid-chain `set-option` failure
  aborts the rest, and `allow-passthrough` is absent on tmux < 3.3). When
  passthrough is unavailable, the design degrades to the bell fallback.

- **Recognize generically.** A parser matches `OSC 7366` + the `remora` token,
  validates/decodes the payload, and raises a typed activity/notification event.
  The spike proved an xterm-grade OSC handler consumes the marker cleanly
  (returns handled → never rendered) and that an unknown `<type>`/`<ver>` or a
  malformed payload is consumed without error or garbage. **Where the parser
  runs — the client's terminal (`apps/desktop/src/terminal-controller.ts`, which
  is wiring-only today and would gain a `registerOscHandler`) vs. a core- or
  relay-held persistent attach — is a #55/#61 decision, coupled to detached
  detection below.** Core/protocol stay agent-agnostic either way.

- **Bell fallback.** A plain terminal `BEL` is forwarded by default tmux options
  and caught via the terminal's bell event — a low-confidence "something
  happened" signal for agents that emit no richer marker. It carries no payload
  and cannot say *which* session rang, so it stays a fallback, never primary.

Sandbox-supplied payload text is **untrusted and forgeable** (anyone with a shell
on the sandbox can emit the same marker — the token is collision-protection, not
authentication). Core must length-cap and strip control sequences from the
**decoded** bytes (base64 only protects the on-wire framing; the decoded value is
arbitrary and could re-inject terminal escapes), and render it as *agent-claimed*
— trusted facts (which host/session) come from client/relay state, never the
payload (#61's threat model).

## Alternatives considered

- **Agent-declared output regexes matched on the raw screen stream (no marker).**
  Brittle against spinner frames and alt-buffer redraws and agent-version-fragile;
  an explicit marker is unambiguous. Kept as the **screen-quiescence safety net**
  (#55) for agents that emit nothing — complementary, not the primary signal.
- **A side channel (extra socket/file/port on the sandbox).** Richer, but breaks
  "nothing extra on the sandbox" and needs separate relay plumbing. The in-band
  marker rides the existing channel, identical in direct and relay mode.
- **Bell only.** No payload, no state, can't localize the session — fine as a
  fallback, can't be primary.
- **Reuse a vendor OSC (iTerm2 1337, VSCode 633, …).** Couples us to another
  tool's evolving semantics and risks collisions. We use our own code + token.
- **Do nothing.** Leaves VISION.md's notification signal unanswered; blocks
  #55/#61.

## Consequences

Easier:
- #55/#61 build on a proven channel: same path in direct and relay mode, nothing
  on the sandbox, agent-agnostic recognition. New agents gain signaling as a
  config entry, not a feature.

Harder, and what we commit to / must still resolve in #55/#61:
- **Detached detection is unsolved by this spike.** A passthrough marker only
  reaches a parser on an *attached, streaming* channel; tmux does not buffer it.
  The phone-while-away hero means no client is attached when the marker fires —
  so something (the relay, or a per-session headless attach) must hold an attach
  and parse. The spike de-risked the channel and transport, **not** the listener.
- **Parse-location is undecided** (client vs core/relay), coupled to the above;
  core-side parsing would need a new byte-inspection seam on the hot path
  (`ChannelOutput` is opaque "never parse" today) and was not de-risked.
- **Protocol cost.** Surfacing activity to the relay or the poll path means a new
  `ChannelOutput` variant and/or a `SessionMeta`/`SessionState` field — both
  documented breaking changes forcing a `PROTOCOL_VERSION` bump. Define
  forward/back-compat first (unknown `<ver>`/`<type>` ignored, never a false
  state).
- **The marker is only as complete as the agent's hooks** (some waits fire no
  hook), so the quiescence fallback is **load-bearing and must ship together**,
  not be deferred — default to neutral/unknown, never a false "needs input", with
  a defined state lifecycle (staleness, reattach replay, coalescing).
- **Emitting needs the agent's hooks configured to run the printf** — sandbox-side
  agent configuration that is in tension with "nothing extra on the sandbox".
  Decide deliberately (launch-time injection vs pre-config) and keep it
  idempotent/removable/agent-version-tolerant. The `Agent` config (today only
  `command`) gains a declarative marker/state/quiescence schema.
- **Passthrough only on Remora's own session, never the user's**, and the marker
  must never be emitted from a background tmux window (passthrough is dropped
  there). Remora's one-session=one-window=one-pane topology guarantees this; a
  future multi-window layout must add a guard/test, not just honor a doc note.

Follow-ups: implement the parser/event/sanitizer and client handler under #55/#61;
add the passthrough trailer to `new_session_tokens`; **re-run the spike
harness over `kubectl exec`** (the spike proved ssh; kubectl shares the identical
post-attach `open_pty` PTY byte path but differs in PTY flags and an in-pod
TERM/locale preamble, and was not live-run — RBAC); add an integration test that
a marker split across PTY read boundaries still never renders; update
ARCHITECTURE.md when the parser lands.

**#69 (ADR-0013):** The parser/event/sanitizer landed **core-side** in #69 via
`vte` — not only the client — superseding the open "parse-location" question for
the attached path. `ChannelOutput` gained `StatusChange`/`PreviewUpdate` variants
on the single ordered stream, and the detector thread is the sole `output_tx`
sender. The client TS parser (`osc-marker.ts`) is retired in PR2.
