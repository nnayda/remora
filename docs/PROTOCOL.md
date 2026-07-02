# Remora wire protocol

This document specifies the wire protocol a third-party client implements to
talk to a Remora session — the promise [ADR-0002](adr/0002-tauri-single-codebase-optional-relay.md)
made ("third-party clients can target `remora-protocol` without us building
them") and [ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)'s relay
envelope. The normative source is the [`remora-protocol`](../crates/remora-protocol)
crate; this page explains the shapes, the encodings, and the connection
sequence. Where a detail here and the crate disagree, the crate wins — every
wire example below is pinned by a round-trip test in that crate.

Remora is pre-alpha. The types are stable enough to document, but the protocol
is versioned precisely so it can still change (see [Versioning](#versioning)).

## The two layers

A client speaks two nested protocols. Which ones you implement depends on how
you reach the session:

1. **The session protocol** — session discovery, attach, the PTY byte stream,
   and activity events. This is the whole protocol in **direct mode** (a
   client embedded in a process that links `remora-core`, calling the
   `SessionSource` trait in-process — no bytes on a wire), and it is the
   **payload** carried end-to-end in relay mode. Modules:
   [`session`](../crates/remora-protocol/src/session.rs),
   [`channel`](../crates/remora-protocol/src/channel.rs),
   [`id`](../crates/remora-protocol/src/id.rs), and the
   request/response wrapper [`remote`](../crates/remora-protocol/src/remote.rs).

2. **The relay envelope** — a thin routing header the blind relay parses to
   forward opaque frames between paired devices, wrapping a Noise-encrypted
   payload it never reads. Present only in **relay mode** (phone/desktop ⇄
   relay ⇄ bridge). Module: [`envelope`](../crates/remora-protocol/src/envelope.rs).

```
Direct mode:   client ── SessionSource (in process) ── remora-core ── ssh/kubectl ── sandbox
Relay mode:    client ── WS/TLS ── relay (blind) ── WS/TLS ── bridge ── ssh/kubectl ── sandbox
                       └────────── one Noise session, end to end ──────────┘
                            (envelope routes it; the relay is never a party to it)
```

In relay mode the session protocol rides **unchanged** inside the Noise
session; the envelope and the `remote` wrapper are the only surface relay mode
adds. The bridge is the hosted `SessionSource` — it runs only on user hardware
(in the desktop app today), so relay mode never puts session content or
sandbox credentials on the relay.

## Versioning

Two independent version constants govern the wire, one per layer:

- **[`PROTOCOL_VERSION`](../crates/remora-protocol/src/lib.rs)** (currently
  **2**) — the session protocol: message shapes plus the id and tmux-naming
  conventions of [ADR-0004](adr/0004-local-config-live-session-discovery.md).
  It is exchanged as the first message on a fresh session
  (`ClientMessage::Hello` / `BridgeMessage::Hello`, below), so either side can
  fail closed on a mismatch before anything else is said. Nothing in direct
  mode's in-process path negotiates it; relay mode is where the negotiation is
  load-bearing.
- **`ENVELOPE_VERSION`** (currently **1**) — the relay envelope's own byte-0
  version, independent of `PROTOCOL_VERSION` (the two need not move together —
  today they are 2 and 1). `Envelope::decode` rejects any other value, and it
  is mixed into the Noise handshake prologue, so a client and bridge that
  disagree on it cannot complete a handshake.

A bump on either surface invalidates the modes that carry it — the deliberate
cost of gating compatibility on an explicit version rather than probing.

**Compatibility rules baked into the encoding:**

- **Message enums are externally tagged and reject unknown variants.** Adding a
  variant to `ChannelInput`, `ChannelOutput`, `ClientMessage`, `BridgeMessage`,
  `RemoteOp`, `RemoteResult`, `WireError`, `SessionState`, or `SessionStatus`
  is a **breaking** change — an older client fails closed on the unknown
  variant rather than skipping it. Such a change requires a `PROTOCOL_VERSION`
  bump. (These enums are `#[non_exhaustive]` in Rust for the same reason.)
- **Struct fields are forward-compatible.** Unknown object fields are ignored
  on deserialization, and new **optional** fields (`serde(default)` → `None`
  when absent) can be added without a bump: an older peer that omits the key
  still parses, and a newer field an older client never sends deserializes to
  `None`. `SessionMeta.branch`, `SpawnSpec.workspace`/`branch`/`worktree_root`
  are examples of fields added this way.

Implement a client defensively: tolerate absent optional keys and unknown
extra keys; treat an unknown enum variant as a version mismatch, not a
recoverable message.

## Encoding conventions

- **Session protocol: serde JSON, `snake_case`, externally tagged.** Enums
  encode as `{"variant_name": <payload>}` (a unit variant as the bare string
  `"variant_name"`). This is what rides inside the Noise session; the relay
  never sees it. A binary codec here is a later, type-compatible swap — clients
  should not depend on JSON specifically for the payload, only on the shapes.
- **Envelope: hand-rolled fixed-offset binary.** The relay is on the hot path
  for every keystroke, so its header is a fixed byte layout, not JSON (see
  [The relay envelope](#the-relay-envelope)). The one exception is the
  relay-visible `RelayHello` payload, which is serde JSON because the relay
  must read it to authenticate the connection.
- **Bytes are JSON number arrays.** `ChannelInput::Bytes` / `ChannelOutput::Bytes`
  encode raw PTY bytes as `[104, 105, ...]`. Bytes are **opaque** — never valid
  UTF-8 in general (split multibyte runs, ANSI control bytes) and never to be
  parsed; feed output to a terminal emulator, send input verbatim.
- **Ids are plain slug strings.** `ProjectId`, `SessionId`, `AgentId` serialize
  as their string (`"api"`, `"fix-login"`), validated on the way in (below).
- **`DeviceId` is 64 lowercase hex characters** in JSON (inside `RelayHello`)
  and 32 raw bytes on the envelope header.

## Identifiers

`ProjectId`, `SessionId`, and `AgentId` are lower-case slugs matching
`[a-z0-9-]+`, 1 to `MAX_ID_LEN` (**64**) characters
([`id`](../crates/remora-protocol/src/id.rs), [ADR-0004](adr/0004-local-config-live-session-discovery.md)).
The underscore is reserved as the separator in tmux session names
(`remora_<project-id>_<session-id>`), so a valid id can never break that parse.

Validation runs at **every** construction and deserialization path — a value
that is not a valid slug fails to deserialize, so a forged id cannot enter a
message. This is a load-bearing property for implementers: because ids validate
on the wire, there is no separate trust boundary to defend when an id arrives
inside `RemoteOp::Attach` or `SpawnSpec`. The `InvalidIdError` message escapes
control bytes and bounds its length, because it rides inside serde errors that
callers log.

```json
"fix-login"          // valid SessionId
"Fix_Login"          // rejected: upper-case + underscore
```

## The session protocol

### Session metadata and lifecycle

`SessionMeta` ([`session`](../crates/remora-protocol/src/session.rs)) is one
discovered session as rendered in a client's list.

```json
{"project_id":"api","session_id":"fix-login","state":"live","agent":"claude",
 "created_at":1765500000,"workspace_path":"/home/dev/.remora/worktrees/api/fix-login",
 "workspace":null,"branch":null}
```

| Field | Type | Notes |
| --- | --- | --- |
| `project_id` | `ProjectId` | Stable join key to local config. |
| `session_id` | `SessionId` | Minted client-side at spawn. |
| `state` | `SessionState` | `"live"` (tmux session exists) or `"stopped"` (only the worktree survives — respawnable). |
| `agent` | `string?` | Agent adapter id **as advertised by the sandbox**. Untrusted, display-only. |
| `created_at` | `u64?` | Unix epoch seconds, sandbox-advertised. Untrusted, display-only. |
| `workspace_path` | `string?` | Worktree path, sandbox-advertised. Untrusted, display-only. |
| `workspace` | `WorkspaceMode?` | Effective mode discovered from real state: `"worktree"` or `"shared"`. `null` from an older sender → client falls back to the project's configured mode. |
| `branch` | `string?` | Branch in the worktree (`git worktree list`), the session's display identity. `null` for a shared or detached-HEAD session. |

> **Discovered metadata is untrusted, display-only input.** Anyone with a
> shell on the sandbox — including the agent — can forge `agent`, `created_at`,
> `workspace_path`, `workspace`, and `branch`. They are plain optional strings
> for a reason: a forged value must never make the message undeserializable,
> and **nothing may build a command from them**. The producer side owns a
> matching invariant: it must *drop* sessions whose discovered names don't
> parse as ids and map unparseable metadata to `null`, so one forged element
> can never poison the enclosing message for every client
> ([ADR-0004](adr/0004-local-config-live-session-discovery.md)).

`SessionStatus` ([ADR-0013](adr/0013-core-side-activity-detector.md)) is
per-session agent activity, distinct from lifecycle `state`: `"working"`,
`"idle"`, `"awaiting"`, or `"unknown"`. `awaiting` is **marker-only** — never
inferred from a quiescent screen; `unknown` is a freshly-attached channel
before any byte arrives, never a parse failure.

`SpawnSpec` is a request to create a session. It carries **only references into
local configuration** — never paths or commands. Spawn builds the remote
command exclusively from the local project and agent-adapter config; the
`session_id` is minted client-side and creation fails closed if the tmux name
already exists.

```json
{"project_id":"api","session_id":"fix-login","agent":"claude",
 "base":null,"workspace":null,"branch":null,"worktree_root":null}
```

`agent`, `base` (git start-point), `workspace`, `branch`, and `worktree_root`
are all optional overrides; absent → the project/host default cascade.

### The attached channel

Once attached, a session is a byte stream plus resize — nothing smarter
([`channel`](../crates/remora-protocol/src/channel.rs)). Screen state belongs
to the client's terminal emulator; nothing transport- or core-side parses ANSI.

**`ChannelInput` (client → session):**

```json
{"bytes":[104,105]}                    // keystrokes / pastes for the PTY
{"resize":{"rows":30,"cols":100}}      // propagate a terminal resize
```

`TerminalSize` rows and cols are **always nonzero** — a `0x0` winsize is
rejected at every construction and deserialization path (it is a classic
divide-by-zero / render-bug source downstream). The protocol carries the
*requested* size; tmux reserves a status line, so the geometry the agent sees
may differ by a row (request 30, get 29). Remora-spawned sessions set
`window-size latest` (tmux ≥ 3.1) so the window follows the latest client to
write; compensation, if any, is a client concern.

**`ChannelOutput` (session → client):**

```json
{"bytes":[27,91,50,74]}                // raw PTY output — feed to an emulator, never parse
{"status_change":"awaiting"}           // a SessionStatus change (ADR-0013), ordered after its bytes
{"preview_update":"run tests? (y/n)"}  // one-line, already-sanitized preview of latest output
"marker_seen"                          // one-shot: the activity hook's first marker parsed this attach (#198)
```

`preview_update` is control-stripped and length-capped **by the sender**
(core) before it is constructed; render it as text. `marker_seen` fires once
per attach and carries no data — its presence is the whole signal that the
agent's activity hook is wired
([ADR-0019](adr/0019-liveness-ping-hook-confirmation.md)).

There is deliberately **no "detached" message**: channel death is observable
only locally, and each transport owns its own disconnect semantics.

Byte payloads are unbounded at the type level. **Transports own framing and
must cap message size** — a peer could otherwise force unbounded allocation.
(In relay mode the Noise layer enforces this; see `MAX_NOISE_PLAINTEXT` and
`chunk_bytes` in [`remora-bridge`](../crates/remora-bridge/src/noise.rs).)

### The remote request/response wrapper

In relay mode a client cannot call `SessionSource` methods in-process, so the
[`remote`](../crates/remora-protocol/src/remote.rs) module wraps them as
addressable messages that ride inside the Noise session (they have **no**
analogue in direct mode). These are the plaintext the Noise layer seals.

**`ClientMessage` (client → bridge):**

```json
{"hello":{"protocol_version":2}}                                        // first message on a fresh session
{"request":{"id":7,"op":{"attach":{"project_id":"api","session_id":"fix-login"}}}}
{"input":{"bytes":[104,105]}}                                           // a ChannelInput, after a successful Attach
```

**`BridgeMessage` (bridge → client):**

```json
{"hello":{"protocol_version":2}}
{"response":{"id":7,"result":"attached"}}                               // echoes the request id
{"output":{"status_change":"awaiting"}}                                 // a ChannelOutput
"channel_closed"                                                        // unsolicited: the attached channel's far end is gone
```

- **`Hello`** is exchanged first, both directions, carrying `PROTOCOL_VERSION`
  for the fail-closed version gate.
- **`Request { id, op }`** — `id` is a client-chosen correlation token the
  bridge echoes on the matching `Response`. Requests may interleave with each
  other and with the unsolicited `Input`/`Output` stream on the same session.
  `RemoteOp` is `"list"` (discover sessions across the bridge's hosts) or
  `{"attach":{project_id, session_id}}`.
- **`RemoteResult`** in a `Response` is `{"sessions":[SessionMeta,…]}` (answers
  `list`), `"attached"` (a successful `attach`; the channel stream follows on
  the same session), or `{"error": WireError}`.
- **`Input(ChannelInput)`** is meaningless before a successful
  `attach`; **`Output(ChannelOutput)`** and `channel_closed` are unsolicited
  stream events, not `Response`s.

`WireError` is the stable protocol projection of core's `SourceError` — it
evolves append-only under `PROTOCOL_VERSION`, not 1:1 with core:

```json
{"session_exists":{"project_id":"api","session_id":"fix-login"}}
{"session_not_found":{"project_id":"api","session_id":"gone"}}
{"workspace_dirty":{"message":"session `api_fix-login` has uncommitted changes that would be lost"}}
{"plan":{"message":"spawn could not be planned: unknown project `ghost`"}}
"channel_closed"
{"transport":{"message":"transport error: ssh exited"}}
```

Variants that carry typed identity in core keep it here; variants whose core
payload is backend-specific or already-formatted carry a **display-safe**
`message` string. `WireError` does no escaping of its own — the **sender** is
responsible for producing a value already safe to render (the same discipline
`SourceError::Transport` and `InvalidIdError` apply on the sending side).

## The relay envelope

Relay mode wraps every frame in a fixed-offset binary header the relay parses
to route, around an opaque payload it never inspects (Noise ciphertext in
practice; plain JSON only for the pre-Noise `RelayHello`).
[`Envelope`](../crates/remora-protocol/src/envelope.rs):

```text
offset 0    u8        ENVELOPE_VERSION            (decode rejects != 1)
offset 1    u8        frame type                  (decode rejects > 3)
offset 2    [u8; 32]  src routing id (DeviceId)
offset 34   [u8; 32]  dst routing id (DeviceId)
offset 66   payload   0..=65535 bytes             (decode rejects longer)
```

`ENVELOPE_HEADER_LEN` = 66; `MAX_ENVELOPE_PAYLOAD` = 65535. `Envelope::decode`
validates version, frame type, minimum length, and payload length, returning
`EnvelopeError::{UnknownVersion, UnknownFrameType, Truncated, Oversized}`.

**`FrameType`** (the byte at offset 1; the relay dispatches on it without
touching the payload):

| Byte | Variant | Meaning |
| --- | --- | --- |
| 0 | `Hello` | A device or bridge authenticating to the relay. Payload is a JSON `RelayHello`. |
| 1 | `Data` | Opaque session payload (Noise ciphertext). |
| 2 | `Pairing` | **Reserved** for the pairing follow-up (#232). The codec round-trips it; nothing constructs one yet. |
| 3 | `PushTrigger` | **Reserved** for the push follow-up (#233). Same status. |

**`DeviceId`** is an opaque 32-byte routing identity. `DeviceId::ZERO`
(all-zero) is reserved and valid **only** as the `dst` of a `Hello` frame,
before the peer has learned a real routing id; any other use is a protocol
violation the relay/bridge reject.

**`RelayHello`** (the `Hello` frame's payload, serde JSON) is the only
relay-visible payload. It is how a peer introduces itself *to the relay* before
any Noise session exists, so its fields are plaintext the relay legitimately
reads to route and authenticate the connection — and nothing more.

```json
{"role":"device","token":"<relay credential>",
 "device_id":"<64 hex>","routing_id":"<64 hex>","bridge_id":"<64 hex>"}
```

| Field | Meaning |
| --- | --- |
| `role` | `"bridge"` or `"device"`. |
| `token` | The relay-issued credential (rendezvous token, or bridge registration token) proving admission to routing — nothing more. |
| `device_id` | The long-lived pairing identity this peer authenticates as. |
| `routing_id` | The envelope routing id for this connection. **Equal to `device_id` for a bridge**; a device routes under a fresh per-connection id, not its own identity. |
| `bridge_id` | For a device: the bridge it wants routed to. For a bridge: its own id (mirrors `routing_id`). |

The relay's routing is **adjacency-scoped** (enforced in the relay's
[`router`](../crates/remora-relay/src/router.rs)): a device may address only
its declared bridge; a bridge may address only devices currently registered in
its own group; and a frame's envelope `src` must equal the sender's own
routing id (anti-spoof). None of this reads the payload — blindness is
structural.

## Connection sequence (relay mode)

For a client reaching a session through a relay:

1. **Open a WebSocket to the relay** (WS/TLS). The relay terminates TLS but
   never a Noise session.
2. **Send a `Hello` frame** whose payload is your `RelayHello` (role `device`,
   your relay `token`, your `device_id`, a fresh non-zero `routing_id`, and the
   `bridge_id` you want routed to). The relay authenticates the token against
   its roster and registers your routing id, or closes the connection. From
   here the relay forwards `Data` frames between you and the bridge; it is
   never a party to what follows.
3. **Run the Noise `IKpsk2` handshake with the bridge, end-to-end.** Two
   messages (`-> e, es, s, ss` then `<- e, ee, se`), carried as `Data`-frame
   payloads. You are the initiator: you pin the bridge's static public key
   (from pairing) and present your own; the per-pair PSK is mixed in; a
   [prologue](../crates/remora-bridge/src/noise.rs) binds `ENVELOPE_VERSION`,
   `PROTOCOL_VERSION`, and the three device ids into the transcript, so a
   malicious relay cannot re-point the session onto a different route. A wrong
   PSK, wrong pinned static, or mismatched prologue fails the first AEAD check.
   The cipher suite is `Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s`.
4. **Exchange `Hello` session messages** inside the now-established Noise
   transport: send `ClientMessage::Hello { protocol_version }`, expect
   `BridgeMessage::Hello { protocol_version }`, and fail closed on a mismatch.
5. **Drive the session.** Send `Request { op: List }` to discover sessions,
   `Request { op: Attach {…} }` to attach; on the `Attached` response, stream
   `Input` up and receive `Output` down until `channel_closed` or teardown.
   Each application message is one sealed Noise transport message in one `Data`
   frame; PTY byte runs larger than a chunk are split (`chunk_bytes`) so no
   single message trips the Noise plaintext cap.

Two envelope invariants constrain any implementation
([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)):

- **Security-relevant control messages are authenticated inside Noise or force
  a full re-handshake.** Envelope forgery must be *availability-only* by
  construction — a relay-forgeable resume token would be a session-hijack
  primitive. The relay can drop, delay, or flood frames; it can never forge or
  read one.
- **Noise transport messages must be opened in the exact order they were
  sealed** (`snow`'s nonce discipline). A relay cannot drop individual frames
  without killing the E2E session — load-shedding is connection-granular by
  construction — so resume is loss-free and ordered on the same session, or it
  surfaces to the session layer as a fresh attach.

## What the relay sees, and does not

The relay is a **blind forwarder**
([ADR-0021](adr/0021-blind-relay-bridge-trust-model.md)). By construction it
legitimately observes: opaque `DeviceId`s and pairing-group association
(routing); connection liveness, frame sizes, and timestamps; connection
credentials (`RelayHello.token`); and push tokens. The protocol **never**
requires it to see: session content, session names/previews, host config,
sandbox addresses or credentials, agent identity, or repo/branch names — and an
implementation must not leak these through routing headers, logs, or crash
reports.

One honest caveat: over an interactive per-keystroke PTY stream, **frame timing
and size are a known content side-channel** (SSH-style inter-keystroke timing
inference). This is a named, accepted risk; envelope padding/coalescing is a
consideration, not a v1 guarantee.

## Status and scope

The types above are what [relay slice 1](adr/0021-blind-relay-bridge-trust-model.md)
(#231) shipped: the envelope protocol, the Noise session, and one end-to-end
PTY stream (attach, list). Deliberately **not** yet specified here, and owned
by follow-ups: QR split-secret pairing and the `Pairing` frame (#232), push
notifications and the `PushTrigger` frame (#233), the headless bridge binary
(#234), and the durable ciphertext-mailbox session record (#71). When those
land, the reserved frame types and any new session messages version alongside
`PROTOCOL_VERSION`.

## See also

- [ADR-0021](adr/0021-blind-relay-bridge-trust-model.md) — the blind relay /
  bridge trust model and the envelope contract.
- [ADR-0002](adr/0002-tauri-single-codebase-optional-relay.md) — the
  `remora-protocol` seam and the third-party-client promise.
- [ADR-0004](adr/0004-local-config-live-session-discovery.md) — local config,
  discovered-state-is-untrusted, id conventions.
- [ADR-0013](adr/0013-core-side-activity-detector.md) — the activity detector
  behind `SessionStatus` / `StatusChange`.
- [ARCHITECTURE.md](ARCHITECTURE.md) — where the protocol crate sits in the
  system and the security invariants it enforces.
- [`remora-protocol`](../crates/remora-protocol) — the normative source.
