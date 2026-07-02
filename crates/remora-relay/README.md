# remora-relay

A blind envelope-frame relay for Remora relay mode
([ADR-0021](../../docs/adr/0021-blind-relay-bridge-trust-model.md)). It
accepts outbound WebSocket connections from bridges and devices and routes
opaque [`remora-protocol`](../remora-protocol) envelope frames between paired
peers. It never terminates the Noise session those frames carry and never
inspects their payload — that is enforced by this crate's dependency graph,
not just convention: `remora-relay` must never depend on `snow`,
`remora-core`, or any other crypto/session-content crate. The relay legitimately
sees routing metadata (device IDs, connection liveness, frame sizes,
timestamps) — never plaintext, session content, or sandbox credentials. See
ADR-0021's "Metadata policy" and "Threat model" sections for the full
boundary, including the one accepted risk (frame timing/size is a known
side-channel over an interactive PTY stream).

This document covers self-hosting the relay: config reference, the container
image, and operational posture. It is not the wire protocol spec (see
[docs/adr/0021-blind-relay-bridge-trust-model.md](../../docs/adr/0021-blind-relay-bridge-trust-model.md)
and the tracked follow-up PROTOCOL.md, #236) and not the pairing flow (#232).

## Running it

```sh
remora-relay /etc/remora/relay.toml
```

The config path is the one and only argument. There is no hot-reload:
changing the file has no effect until the process restarts (see
[Token rotation](#token-rotation-and-revocation) below).

### Container image

```sh
# from the repo root — the build context needs the whole Cargo workspace
docker build -f crates/remora-relay/Dockerfile -t remora-relay .
docker run -v /path/to/relay.toml:/etc/remora/relay.toml:ro -p 9440:9440 remora-relay
```

Multi-stage build: `rust:1-bookworm` compiles with `cargo build --release
--locked`, then the binary runs in `gcr.io/distroless/cc-debian12` (no shell,
no package manager) as the image's unprivileged `nonroot` user. See
[Reproducible builds](#reproducible-builds) for what the digest pins in the
`Dockerfile` do and don't guarantee today.

## Config reference

```toml
listen = "0.0.0.0:9440"
buffer_bytes = 1048576  # optional; this is the default

[[bridges]]
token = "<bridge-registration-token>"
device_id = "<64-hex-char device id the token identifies as>"

[[devices]]
token = "<device-token>"
device_id = "<64-hex-char device id>"
bridge_id = "<64-hex-char device id of the bridge this device routes through>"

[audit]
path = "/var/log/remora-relay/audit.log"  # omit this table to disable audit mode
```

- **`listen`** — the address the WebSocket server binds, e.g.
  `"127.0.0.1:9440"` for loopback-only or `"0.0.0.0:9440"` behind a reverse
  proxy.
- **`bridges`** / **`devices`** — the admission list. **Closed by default**:
  omit either list (or the whole file's `[[bridges]]`/`[[devices]]` tables)
  and nothing in that role can register — there is no implicit open mode. A
  bridge entry's token admits exactly the `device_id` it names, nothing else
  (a token is not a capability that can be reused for a different identity).
  A device entry's token is scoped to one `(device_id, bridge_id)` pair — the
  same device paired with two bridges needs two entries, and revoking one
  never affects the other. Token comparison is constant-time.
- **`buffer_bytes`** — per-connection outbound buffer cap, in bytes (default
  1 MiB / `1048576`). This is the relay's load-shedding knob: because Noise
  transport nonces require strictly ordered, lossless delivery, the relay
  cannot drop individual frames without breaking the E2E session underneath —
  so shedding is connection-granular. A connection whose outbound queue
  exceeds this budget is killed (close code `4008`) rather than let its
  backlog grow unbounded; the sender that overflowed it is unaffected.
- **`audit`** — opt-in; see [Audit mode](#audit-mode) below. Omitting the
  `[audit]` table disables it entirely (the default).

Device IDs are 32-byte values written as 64 lowercase hex characters.

## TLS

The relay itself speaks **plain WebSocket** — it does not terminate TLS.
Put it behind a reverse proxy or ingress (nginx, Caddy, an Ingress/Ambassador
in Kubernetes) that terminates `wss://` and forwards plain `ws://` to the
relay's `listen` address. This keeps the relay's job to exactly one thing
(routing opaque frames) and lets TLS cert management live where every other
service's already does. Clients and bridges connect to the proxy's `wss://`
URL; the relay never needs a certificate.

## Close codes

The relay's WebSocket close code on every teardown is one of:

| Code | Reason | Meaning |
| --- | --- | --- |
| `1000` | normal | Peer closed cleanly, or the socket reached EOF. |
| `4001` | auth_failure | The hello's token didn't match a configured `bridges`/`devices` entry (or none is configured — closed by default). |
| `4002` | protocol | A malformed frame, an illegal frame type for the connection's state, or an envelope `src`/`dst` adjacency violation. |
| `4004` | peer_gone | The envelope's destination device isn't currently registered with the relay. Sent only to a **device** sender whose bridge is gone (it has nothing left to do). A **bridge** addressing a departed device is routine — under D3 a device reconnects with a fresh routing id, so its old id goes offline on every reconnect — so the relay drops that undeliverable frame and keeps the bridge connection up rather than tearing down every other device's session on it. |
| `4008` | buffer_overflow | This connection's outbound buffer exceeded `buffer_bytes`; it was shed as a slow consumer. The frame's *sender* is unaffected — only the slow destination is killed. |
| `4009` | replaced | A newer connection registered under the same routing id and displaced this one (e.g. the same device reconnected before the old socket was cleaned up). |

These are also the `close_reason` values recorded in the audit log (see
below) when audit mode is on.

## Audit mode

Off by default. When `[audit]` is set, the relay appends one JSON Lines
record **per connection close** (never per frame) to the configured path:
role (`bridge`/`device`/`null` if the connection never completed a hello),
device id, routing id, inbound/outbound frame and byte counts, connection
lifetime, and the close reason from the table above. It never contains
payload bytes — the relay is blind to them by construction, so there's
nothing to log even by mistake.

The log file is created `0600` (owner read/write only). Treat it as
sensitive and **keep retention short**: even without payloads, aggregate
per-connection frame counts and timing are a keystroke-timing/size
side-channel over an interactive PTY stream — the same accepted risk ADR-0021
names for the relay's live routing metadata. A short-retention rotation
policy (e.g. daily rotation, a few days' keep) bounds how much of that
side-channel accumulates at rest; this crate doesn't rotate the file itself,
so wire up `logrotate` or your platform's equivalent. Audit mode is a
regression guard against accidental leakage in this codebase, not an
operator-facing security audit trail — a malicious relay operator can
already see the same metadata live, on or off.

## Token rotation and revocation

There is no admin API and no hot-reload. To rotate or revoke a token: edit
`relay.toml` (remove or replace the `[[bridges]]`/`[[devices]]` entry) and
restart the process. Connections already routing under a removed token stay
up until they next disconnect or are killed by the operator (e.g. `docker
restart`); there is no live-kick of an already-admitted connection today. Per
ADR-0021, the relay's admission list is defense-in-depth — the actual trust
boundary is the bridge's device roster and its own revocation flow, which
takes effect independent of the relay.

## Reproducible builds

**Baseline, not verified.** The `Dockerfile`'s base images
(`rust:1-bookworm`, `gcr.io/distroless/cc-debian12`) are pinned by digest,
and the build uses `cargo build --release --locked`, so the same Dockerfile
always resolves the same inputs — no floating `latest` tag can silently
change what gets built. That's the reproducibility *baseline*: pinned
inputs. It is not yet a verified guarantee that two builds from those same
inputs produce a byte-identical binary (Rust codegen and Docker layer
construction both have known sources of non-determinism — embedded build
paths, timestamps, codegen-unit scheduling). Bit-for-bit build verification,
plus a process for keeping the pinned digests current as upstream images
patch, is tracked in
[#251](https://github.com/nnayda/remora/issues/251).

## Related

- [ADR-0021](../../docs/adr/0021-blind-relay-bridge-trust-model.md) — the
  trust model this crate implements (blind relay / user-side bridge split,
  metadata policy, threat model).
- [remora-protocol](../remora-protocol) — the envelope frame types this crate
  routes.
- `crates/remora-relay/tests/` — integration coverage for hello auth,
  adjacency routing, overflow/displacement kill behavior, and audit records.
