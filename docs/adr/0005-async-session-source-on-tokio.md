# 0005. SessionSource is async on tokio; channels are message pipes

- **Status:** Accepted
- **Date:** 2026-06-12
- **Issue/PR:** —

## Context

`SessionSource` is the seam everything crosses: the Tauri shell drives it
to render terminals, real transports (ssh, kubectl exec) implement it, and
the future relay hosts it behind a WebSocket
([ARCHITECTURE.md](../ARCHITECTURE.md)). The spine spike established its
contract — spawn → channel, attach → channel, list without attaching;
channels are byte-stream plus resize, and channel death is only observable
locally. What the spike left open is the concurrency model: PTY I/O is
blocking at the bottom (a `portable-pty` reader thread), while the two
first consumers — the Tauri 2 shell and the planned relay — are both
tokio-native. Whichever world the trait lives in, somebody writes an
adapter; the question is who, and how many times.

## Decision

We will make `SessionSource` an `async` trait on tokio, dyn-compatible via
the `async-trait` crate, and make channels **concrete `tokio::sync::mpsc`
pairs of `remora-protocol` messages** rather than a trait:

- `spawn`/`attach` return a `SessionChannel` — a bounded `Sender<ChannelInput>`
  / `Receiver<ChannelOutput>` pair. Resize rides the input queue.
- Channel death is structural: a transport drops its endpoints, and the
  caller observes send errors / `recv() == None`. No close, detach, or
  state-change notification exists, matching the spike's finding that
  disconnect semantics are per-transport and never remotely observable.
- The queue bound counts *messages*, not bytes; capping each `Bytes`
  payload is the sending transport's framing obligation. The bound is only
  meaningful when both hold.
- Transports own their internal blocking I/O threads and bridge them to
  the mpsc pair; nothing in core blocks the runtime.
- `remora-core` takes tokio with the `sync` and `rt` features only.

## Alternatives considered

- **Sync trait, thread-backed channels:** keeps core runtime-agnostic and
  dependency-light; matches blocking PTY I/O directly. Rejected because
  both real consumers are tokio-native — every consumer would re-write the
  same thread-to-async adapter that the trait can express once, and the
  relay milestone would force an async wrapper anyway.
- **Native `async fn` in traits (no `async-trait` crate):** not
  dyn-compatible; the Tauri bridge holds sources as `Box<dyn
  SessionSource>` per host, so dynamic dispatch is part of the contract.
- **A channel *trait* (async read/write/resize methods):** lets each
  transport hide its plumbing, but each one then reinvents buffering and
  death signaling, and consumers convert back to message streams to
  forward anyway. The concrete mpsc pair states that directly.

## Consequences

What becomes easier:

- The Tauri bridge and relay consume the seam natively — forward messages,
  no thread adapters per consumer.
- Channel death semantics are identical across transports by construction,
  and the in-process fake exercises exactly the code paths real callers use.
- Backpressure is built in: bounded queues throttle a PTY firehose.

What becomes harder, and what we are committed to:

- `remora-core` and every transport commit to tokio. Embedding the crate
  in a non-tokio host means bringing a runtime along.
- `async-trait`'s boxing adds a small per-call allocation on the seam —
  irrelevant next to PTY latency, but it is the price of dyn dispatch.
- Transports must never block the runtime; their blocking I/O lives on
  dedicated threads they own and reap. CI cannot see a violation — review
  must.
- "Backpressure" does not extend to the PTY itself: a PTY cannot be paused.
  When the output queue fills (a stalled or backgrounded consumer), a
  transport must decide its own overflow policy — block its reader (and
  risk stalling the remote agent) or drop output (tmux repaint-on-reattach
  recovers screen state). The seam does not mandate one; each transport
  documents its choice. The fake parks on `output.send` (the blocking
  variant), which is the simplest, not the recommended, policy.
