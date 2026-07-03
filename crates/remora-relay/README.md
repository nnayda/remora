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
[docs/PROTOCOL.md](../../docs/PROTOCOL.md) and
[docs/adr/0021-blind-relay-bridge-trust-model.md](../../docs/adr/0021-blind-relay-bridge-trust-model.md))
and not the pairing flow (ADR-0021, #232 — the roster this relay routes
against is asserted by each bridge, never configured here).

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
buffer_bytes = 1048576        # optional; this is the default
handshake_timeout_secs = 10   # optional; this is the default
max_connections = 1024        # optional; this is the default

[[bridges]]
token = "<bridge-registration-token>"
device_id = "<64-hex-char device id the token identifies as>"

[audit]
path = "/var/log/remora-relay/audit.log"  # omit this table to disable audit mode

[push]
enabled = false                     # optional; this is the default (push wakes off)
allow_http = false                  # optional; this is the default
allow_private_endpoints = false     # optional; this is the default
device_cooldown_secs = 30           # optional; this is the default
per_bridge_per_minute = 10          # optional; this is the default
max_in_flight = 32                  # optional; this is the default
```

- **`listen`** — the address the WebSocket server binds, e.g.
  `"127.0.0.1:9440"` for loopback-only or `"0.0.0.0:9440"` behind a reverse
  proxy.
- **`bridges`** — the *only* static admission list this file holds, and it
  admits bridges only. **Closed by default**: omit `[[bridges]]` entirely and
  no bridge can register — there is no implicit open mode. A bridge entry's
  token admits exactly the `device_id` it names, nothing else (a token is not
  a capability that can be reused for a different identity). Token comparison
  is constant-time. Devices are **not** configured here at all (ADR-0021 D4):
  a device authenticates against its own bridge's live, asserted roster
  (`AssertDevices`, sent by the bridge on every connect and every roster
  change) rather than any static relay-side list — there is no `[[devices]]`
  table to fill in. Pairing/revoking a device is entirely a bridge-side
  operation; see [Token rotation and revocation](#token-rotation-and-revocation).
- **`buffer_bytes`** — per-connection outbound buffer cap, in bytes (default
  1 MiB / `1048576`). This is the relay's load-shedding knob: because Noise
  transport nonces require strictly ordered, lossless delivery, the relay
  cannot drop individual frames without breaking the E2E session underneath —
  so shedding is connection-granular. A connection whose outbound queue
  exceeds this budget is killed (close code `4008`) rather than let its
  backlog grow unbounded; the sender that overflowed it is unaffected.
- **`handshake_timeout_secs`** — deadline for the pre-authentication handshake
  (default 10s): the WebSocket upgrade plus the first (hello) frame share one
  window. A client that opens a socket and never sends a hello would otherwise
  pin a task, an FD, and a read buffer indefinitely (a slowloris); once this
  deadline elapses the relay drops it. It applies only *before* a successful
  hello — an authenticated connection is never subject to it.
- **`max_connections`** — global cap on concurrent connections (default 1024).
  The accept loop holds a semaphore with this many permits; a newly accepted
  socket that cannot take one is dropped immediately, before the WebSocket
  upgrade, bounding total FDs and tasks. This is a **global** cap only —
  per-IP / per-sender fairness is part of the deferred rate-limiting follow-up
  and is deliberately out of scope here, so a single source that can open many
  connections can still consume the whole budget; put the relay behind a proxy
  or firewall that does per-source limiting if that is a concern.
- **`audit`** — opt-in; see [Audit mode](#audit-mode) below. Omitting the
  `[audit]` table disables it entirely (the default).
- **`push`** — opt-in push-wake delivery ([ADR-0023](../../docs/adr/0023-unifiedpush-first-wake-delivery.md));
  see [Push wake (opt-in)](#push-wake-opt-in) below. Omitting the `[push]`
  table, or any key inside it, falls back to its default — an absent table is
  identical to every field set to its default (push disabled).
  - **`enabled`** — master switch (default `false`). `false` means a
    well-formed `PushTrigger` from a bridge is still accepted (never a
    protocol violation) but never produces a delivered wake.
  - **`allow_http`** — admit cleartext `http://` endpoints (default `false`).
    Even when `true`, cleartext is only ever allowed to a private/loopback
    target — never to the public internet.
  - **`allow_private_endpoints`** — admit endpoints that resolve to loopback,
    RFC 1918 private, IPv6 ULA (`fc00::/7`), CGNAT (`100.64.0.0/10`), or
    `0.0.0.0/8` (which routes to the local host on Linux) addresses (default
    `false`), for LAN self-hosters (an ntfy instance on the same network).
    Link-local and cloud-metadata addresses (`169.254.0.0/16`, IPv6
    `fe80::/10`) are blocked unconditionally — no flag re-admits them — as are
    the documentation (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`),
    benchmarking (`198.18.0.0/15`), reserved (`240.0.0.0/4`), and tunnelled-v4
    IPv6 (6to4 `2002::/16`, Teredo `2001:0::/32`) ranges, where no legitimate
    push target lives.
  - **`device_cooldown_secs`** — minimum seconds between two delivered wakes
    for the *same device* (default `30`; `0` disables the cooldown), so one
    flapping session cannot spam one phone.
  - **`per_bridge_per_minute`** — token-bucket budget of delivered wakes per
    *bridge* per minute (default `10`; `0` blocks every wake from that
    bridge), so one compromised or buggy bridge cannot spam the relay's
    outbound path.
  - **`max_in_flight`** — global cap on simultaneously in-flight deliveries
    (default `32`). A wake that cannot immediately take a permit is dropped,
    never queued.

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
| `4001` | auth_failure | A bridge's hello token didn't match a configured `bridges` entry (or none is configured — closed by default); or a device's hello didn't match its claimed bridge's live asserted roster or an open pairing window. |
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

## Push wake (opt-in)

Off by default. When `[push] enabled = true`, the relay can wake a paired
device with a UnifiedPush notification when a session it owns needs
attention and no live connection is currently reaching it — see
[ADR-0023](../../docs/adr/0023-unifiedpush-first-wake-delivery.md) for the
full design and the network-target (SSRF) policy `allow_http` and
`allow_private_endpoints` gate.

**ntfy quick-start** — the fastest way to see a real wake on a real phone,
using [ntfy](https://ntfy.sh) as the UnifiedPush distributor:

1. Pick an unguessable topic name and set it as this device's push endpoint —
   in the desktop app's config, `[relay] push_wake_url =
   "https://ntfy.sh/<your-topic>"` (a device-level setting, not this relay's
   config).
2. Install the ntfy app (iOS/Android) and subscribe to the same topic.
3. On the relay, set `enabled = true` under `[push]` in `relay.toml` and
   restart the relay.
4. Leave the paired session idle without a connected device watching it; when
   it next needs attention, the relay POSTs a wake and the phone buzzes.

**Residual risk, named plainly:** the endpoint is a URL the *device* supplies
and the relay POSTs to unauthenticated — this is a bounded server-side request
forgery surface, not a theoretical one. Put concretely: an authenticated bridge
can make the relay a **low-rate outbound-POST reflector** to a public HTTPS URL,
bounded on every axis — a fixed constant body (no attacker-chosen bytes), the
resolve-check-pin network policy, redirects disabled, the deny-list, and the
per-device cooldown / per-bridge budget / global in-flight caps. Read those as
*bounds*, not elimination — named, and accepted. The push provider
(ntfy.sh, or whatever distributor the endpoint points at) learns *that* a
Remora session needed attention and *when* — never *why*: no session name,
branch, host, agent identity, or bridge identity ever appears in the request.
A relay (malicious or compromised) can also *forge* a wake to any registered
endpoint; per ADR-0021's framing, forging buys an attacker attention timing,
never session content. One more asymmetry worth knowing if you run
`allow_private_endpoints = true` on a cloud-hosted relay: IPv4 cloud-metadata
(`169.254.169.254`) is link-local and always blocked, but some clouds' IPv6
metadata equivalent (e.g. AWS's `fd00:ec2::254`) is a ULA address, so it is
reachable once that flag is on.

## Token rotation and revocation

There is no admin API and no hot-reload for **bridge** tokens: to rotate or
revoke one, edit `relay.toml` (remove or replace the `[[bridges]]` entry) and
restart the process. A bridge connection already routing under a removed
token stays up until it next disconnects or is killed by the operator (e.g.
`docker restart`); there is no live-kick of an already-admitted connection
today.

**Device** tokens are never in `relay.toml` at all (ADR-0021 D4) — pairing and
revocation are bridge-side operations (the desktop's Devices panel, or a
future headless-bridge equivalent) that take effect the moment the bridge next
asserts its roster to the relay, independent of a relay restart. Per
ADR-0021, this relay's `[[bridges]]` list is defense-in-depth on top of that —
the actual trust boundary for *devices* is the bridge's own roster and
revocation flow.

## Reproducible builds

**The relay binary is bit-for-bit reproducible.** Two independent builds from
a clean cache produce a byte-identical `remora-relay` binary (same sha256).
Three things make that hold, and they're all already in place: the base images
(`rust:1-bookworm`, `gcr.io/distroless/cc-debian12`) are pinned by digest and
the build runs `cargo build --release --locked`, so the same `Dockerfile`
always resolves the same inputs; the workspace's `[profile.release]` sets
`codegen-units = 1` and `strip = true`, which removes the two biggest sources
of Rust non-determinism (codegen-unit scheduling and path-bearing debug info);
and the build runs at a fixed path inside the image (`/src`), so the paths that
survive stripping (panic-location strings) are identical across machines.

Verify it yourself — this builds the image twice from a clean cache and asserts
the extracted binaries share one sha256:

```sh
./scripts/verify-relay-reproducible.sh
```

CI runs this weekly (`.github/workflows/relay-reproducible.yml`) as a
regression guard, on the same cadence as the digest bumps below; trigger it by
hand from the Actions tab after a base-image or release-profile change.

### Reproducing the image *digest*

A plain `docker build` does **not** produce a reproducible image *digest*: the
final layer records the binary's wall-clock mtime, and the image config records
a build timestamp, so two builds of the identical binary still get different
digests. To reproduce the digest as well, build with BuildKit's
`SOURCE_DATE_EPOCH` + `rewrite-timestamp` through the OCI exporter, which
normalizes both:

```sh
SOURCE_DATE_EPOCH=0 docker buildx build --no-cache \
  --output type=oci,rewrite-timestamp=true,dest=relay.oci.tar \
  -f crates/remora-relay/Dockerfile .
```

Two such builds yield an identical OCI manifest digest. (The legacy `docker`
exporter normalizes the config timestamp but not the copied binary's mtime, so
use the OCI exporter — the same format a registry stores — when you need a
reproducible digest.) The binary check above is the guarantee that matters for
operators verifying what they run; digest reproducibility is for verifying a
distributed image against a from-source rebuild.

### Keeping the digest pins current

The pinned base-image digests go stale as upstream patches them. Dependabot's
`docker` ecosystem (see `.github/dependabot.yml`) bumps them weekly — holding
the `1-bookworm` / `cc-debian12` tags, refreshing the `@sha256:` pin — and the
weekly reproducibility job re-verifies the build after each bump.

## Related

- [ADR-0021](../../docs/adr/0021-blind-relay-bridge-trust-model.md) — the
  trust model this crate implements (blind relay / user-side bridge split,
  metadata policy, threat model).
- [ADR-0023](../../docs/adr/0023-unifiedpush-first-wake-delivery.md) — the
  `[push]` wake-delivery design (registration, wake decision, SSRF policy)
  this crate's `push` module implements.
- [remora-protocol](../remora-protocol) — the envelope frame types this crate
  routes.
- `crates/remora-relay/tests/` — integration coverage for hello auth,
  adjacency routing, overflow/displacement kill behavior, and audit records.
