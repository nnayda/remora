# remora-bridge

The headless Remora bridge
([ADR-0021](../../docs/adr/0021-blind-relay-bridge-trust-model.md)): a
`RemoteSource` that drives `remora-core` end-to-end over Noise, reachable
through a [blind relay](../remora-relay) from a phone or a second laptop while
the machine that owns your ssh keys stays asleep or off the desk entirely.
`remora-bridge` is both a library — the same `bridge.rs` engine the desktop
app hosts in-process today for its own relay mode — and a standalone binary
you can run as a long-lived daemon on its own box: a home server, a small VPS,
a container next to the sandboxes it drives. Same trust model, same wire
protocol, same paired devices; the only thing that changes is who hosts the
process.

This document covers running the binary as an operator: configuration, first
deploy, pairing without a desktop app in the loop, migrating an existing
desktop-hosted bridge over to it, the container image, and what's and isn't
reproducible about it. It is not the wire protocol spec (see
[docs/PROTOCOL.md](../../docs/PROTOCOL.md) and
[ADR-0021](../../docs/adr/0021-blind-relay-bridge-trust-model.md)) and not the
relay's own operator guide (see [crates/remora-relay/README.md](../remora-relay/README.md)).

## Configuration

`remora-bridge` reads the same `remora/config.toml` every Remora client
reads — the schema is owned by `remora-core`, not this crate: `[hosts.*]`,
`[projects.*]`, `[agents.*]` describe the sandboxes and sessions this bridge
can reach, exactly as they would in the desktop app's config. `serve`
additionally *requires* a `[relay]` section (`relay_url`, `registration_token`,
optional `push_wake_url`) — without one, `serve` refuses to start with a named
error rather than silently running mesh-less (mesh-only headless deployments
are a separate future mode, not this one).

Without `--state-dir` or `$REMORA_BRIDGE_STATE_DIR` set, the default config
path is Linux-first and documented as such: `$XDG_CONFIG_HOME` (or
`$HOME/.config` if unset) joined with `remora/config.toml`.

**State dir resolution**, highest priority first:

1. `--state-dir <dir>` on the command line.
2. `$REMORA_BRIDGE_STATE_DIR`.
3. The config file's parent directory (so a bare `remora-bridge serve
   /etc/remora/config.toml` with neither of the above keeps its state next to
   the config it read).

The state dir holds everything this bridge needs to survive a restart and
nothing else:

| File | What |
| --- | --- |
| `bridge_identity.toml` | The bridge's durable device id + X25519 static keypair. Never regenerate this without also re-pairing every device — see [Migration](#migration-from-the-desktop-bridge) for the one safe way to move it. |
| `bridge_identity.toml.lock` | The lifetime `flock` guarding the identity file — see [First deploy](#first-deploy-greenfield) and [Migration](#migration-from-the-desktop-bridge). |
| `bridge_roster.toml` | The paired-device roster: pinned static keys and per-pair PSKs. Re-asserted to the relay on every connect. |
| `daemon.lock` | The single-instance guard `serve` holds for its whole lifetime; a second `serve` against the same state dir fails fast rather than racing the first. |
| `ctl.sock` | The Unix control socket the one-shot subcommands (`pair`, `devices`, `revoke`, `status`, `fingerprint`) dial to talk to a running `serve`. |

Editing `[hosts]`/`[projects]`/`[agents]` takes effect on the next call that
resolves them — no restart. `[relay]` is different: `relay_url` and
`registration_token` are read once at `serve` startup, so changing either needs
a restart to take effect — the same no-hot-reload convention the relay's own
config follows. (`push_wake_url` is a device-side registration value the
headless daemon does not yet act on — see the [Known gap](#health--ops) below.)

## First deploy (greenfield)

Standing up a brand-new headless bridge (no desktop bridge to migrate from) is
four steps:

1. **`remora-bridge init [--state-dir <dir>]`** — entirely offline, no network,
   no `[relay]` needed yet. Mints (or loads, if one already exists) the
   identity file under the state dir and prints the two values everyone else
   needs:

   ```
   device_id   <64 hex chars>
   fingerprint <XXXX-XXXX-XXXX>
   ```

2. The relay operator adds a `[[bridges]]` entry naming this `device_id` and a
   fresh registration token to `relay.toml` and restarts the relay (see the
   relay's own [Token rotation and revocation](../remora-relay/README.md#token-rotation-and-revocation)).

3. Write the matching `[relay]` section — the same `relay_url` and
   `registration_token` — into this bridge's `config.toml`, alongside its
   `[hosts]`/`[projects]`/`[agents]`.

4. **`remora-bridge serve [<config.toml>] [--state-dir <dir>]`** — validates
   the config, claims the identity and single-instance locks, binds
   `ctl.sock`, and starts dialing the relay.

A wrong `registration_token` or a `device_id` the relay doesn't recognize does
**not** show up as a network error: `remora-bridge status` reports it plainly
as `rejected` (see [Health & ops](#health--ops)), never as an outage.

## Pairing (no camera — the whole story)

There is no camera in a headless container, so there is no QR code — pairing
is a copy-paste ceremony run against a live `serve`, e.g. from your phone's
terminal app over an already-established ssh/exec session to the box, or from
your own shell via `docker exec`:

```sh
docker exec -it <ctr> remora-bridge pair
```

`pair` opens a 120-second pairing window by default (`--ttl <secs>` to
override) and prints the window duration, then the code on its own indented
line, then waits:

```
Pairing window open (2m0s). Scan or paste on your device:

  remora-pair:1:<...>

Waiting for device...
```

Paste that `remora-pair:1:…` string into the device's pairing UI (or hand it
to whatever client-side flow consumes it). When a device shows up, `pair`
prints its claimed name and fingerprint and asks:

```
Confirm enrollment? [y/N]
```

The default is **no** — anything other than `y`/`Y`/`yes` rejects, and so does
EOF (the session getting dropped, e.g. an interrupted `docker exec`). There is
deliberately **no `--yes` flag** to skip this: ADR-0021's pairing model is that
enrollment is never silent — a device only joins the roster when a human at
the keyboard explicitly confirms it, every time. If the window expires before
a device arrives (or before you confirm), `pair` exits with an expiry message
and a nonzero status; just run `pair` again for a fresh code and window.

Once paired, manage the roster with:

- **`remora-bridge devices [--state-dir <dir>]`** — lists paired devices (id,
  name, fingerprint, enrolled/last-seen timestamps), or `no paired devices` if
  the roster is empty.
- **`remora-bridge revoke <device-id> [--state-dir <dir>]`** — removes a device
  from the roster and re-asserts the roster to the relay immediately.
  **Requires the live relay connection**: `revoke` checks bridge health first
  and refuses with a named error ("bridge is not connected to the relay
  (...); revoke needs the live connection to kick the device") if the bridge
  is not currently connected (still `starting`, `reconnecting`, or `rejected`),
  rather than writing a half-applied revocation to disk that the relay hasn't
  actually heard about yet.

## Migration from the desktop bridge

If you're already running the desktop app's built-in bridge
(`REMORA_REMOTE_LOOPBACK` dogfooding aside, its real relay mode) and want to
move that same bridge identity and paired-device roster onto a headless box so
your devices keep working with **zero re-pairing**, the recipe is:

1. Stop the desktop app (its `serve`-equivalent must not be holding the
   identity lock or the roster file when you move them).
2. **Move** — never copy — `bridge_identity.toml` and `bridge_roster.toml` from
   the desktop's config directory to the headless bridge's state dir.
3. Carry the desktop config's `[relay]` section (`relay_url`,
   `registration_token`, and `push_wake_url` if set) into the headless
   `config.toml` verbatim.
4. Remove `[relay]` from the desktop config, so the desktop no longer tries to
   host its own bridge with the same identity.
5. `remora-bridge serve` on the headless box. Every already-paired device
   reconnects and works immediately — the relay and every device only know
   this bridge by its `device_id` and static keypair, which didn't change.

**Move, don't copy.** Copying instead of moving leaves two processes each
believing they own one relay registration and one E2E identity — a genuine
split-brain, not just a paperwork problem, because both would try to assert
the same roster to the relay and both hold the same private key. The identity
`flock` (`bridge_identity.toml.lock`) catches this if both processes run on
the *same* machine — the second `serve`/`pair`/`init` against the copied file
fails fast with "another remora-bridge is already running" instead of racing.
It cannot catch it across machines: nothing stops you from copying the
identity file to a second host and starting a second `serve` there too — that
is on you to not do. There is no technical enforcement for the cross-machine
case; move the files, don't duplicate them.

## Two configs are two configs

The headless bridge reads its **own** `config.toml` — a separate file from
whatever config.toml your desktop app (or any other client) reads, even if you
copied one from the other on day one. They can drift, and nothing keeps them
in sync automatically. In particular, audit every host's ssh alias and
`IdentityFile` against the *container's* `~/.ssh/config` and mounted key, not
against the ssh config on your laptop — a host alias that resolves fine on
your laptop may not exist inside the container at all.

Drift between two bridges' configs for the same logical project is confusing
— the same project name can resolve to a different path, host, or agent
depending on which bridge a session was opened against — but it is never
corrupting: sessions are namespaced per bridge (tmux session names carry no
cross-bridge identity), so two bridges can never collide on the same session
or clobber each other's state. Worst case is operator confusion, not data
loss.

## Container

```sh
# from the repo root — the build context needs the whole Cargo workspace
docker build -f crates/remora-bridge/Dockerfile -t remora-bridge .
```

Unlike the relay's distroless image, this one is `debian-slim` plus an
`openssh-client` apt layer, because the bridge shells out to `ssh`
(`ControlMaster` multiplexing, [ADR-0011](../../docs/adr/0011-ssh-connection-multiplexing-direct-mode.md)) —
distroless has no shell for that. It runs as a non-root `remora` user
(uid `65532`) with a real, writable `~/.ssh` (`/home/remora/.ssh`), because
`ControlMaster` sockets live there and a read-only home would break
multiplexing entirely.

Mount table:

| Mount | Path | Mode |
| --- | --- | --- |
| config | `/etc/remora/config.toml` | ro |
| state volume | `/var/lib/remora-bridge` | rw |
| ssh key | `/home/remora/.ssh/id_ed25519` | ro (individual file) |
| known_hosts | `/home/remora/.ssh/known_hosts` | ro (individual file) |
| ssh config (optional) | `/home/remora/.ssh/config` | ro (individual file) |

Mount your key, `known_hosts`, and (optionally) an ssh config as **individual
read-only files inside** `~/.ssh` — never a read-only mount over the whole
`~/.ssh` directory. `ControlMaster` sockets are written into that same
directory at connect time ([ADR-0011](../../docs/adr/0011-ssh-connection-multiplexing-direct-mode.md));
a read-only `~/.ssh` would let ssh open the files it needs but never let it
create the socket multiplexing depends on. If you mount an ssh config, set
`BatchMode=yes` and `StrictHostKeyChecking=yes` in it: an ssh prompt (a
password, an unknown host key) has no TTY to answer it inside a headless
container and would hang the session that triggered it, silently, until
something kills it.

`ENV REMORA_BRIDGE_STATE_DIR=/var/lib/remora-bridge` is baked into the image,
so every subcommand run with a bare `docker exec <ctr> remora-bridge ...`
finds `ctl.sock` without needing `--state-dir`. Use that for probes:

- **Liveness** — `exec remora-bridge status`: exits 0 whenever the daemon is
  up and answering, regardless of relay connectivity. A slow or down relay
  should not make an orchestrator restart a perfectly healthy process.
- **Readiness-style** — `exec remora-bridge status --require-relay`: exits
  nonzero unless the relay connection is currently `connected`, for a check
  that wants "this bridge is actually reachable from a device right now."

`kubectl` transports are deliberately not bundled in this image (binary size,
and a bundled kubectl version skews against whatever your cluster actually
runs). Extend the image instead:

```dockerfile
FROM remora-bridge
# install the kubectl matching your cluster's version
```

## Health & ops

`remora-bridge status [--require-relay] [--state-dir <dir>]` prints the
current relay connection state and this bridge's own `device_id`/fingerprint,
then exits 0 if the daemon answered at all (liveness), or nonzero if
`--require-relay` was passed and the relay isn't currently connected. The
states it can report:

- **`connected`** — registered and serving; devices can reach this bridge now.
- **`reconnecting`** — between connections; the dial failed or a live
  connection dropped and the bridge is retrying with backoff. This is also
  what a *wrong `registration_token`* looks like at the hello stage: an
  untrusted relay owes an unrecognized peer no diagnostic, so it just closes
  the socket, and that's indistinguishable from a transient outage from the
  bridge's side.
- **`rejected`** — the relay admitted the connection but explicitly refused
  this bridge's device assertion (`AssertDevices`) — a config problem, not an
  outage. Check the `registration_token` in `[relay]` and the matching
  `[[bridges]]` entry on the relay side (`device_id` mismatch, revoked token).
  Read this state honestly: only *assert-stage* rejection is diagnosable this
  way. A bad token caught earlier, at the hello stage, still reads as
  `reconnecting` above, not `rejected` — if reconnect attempts keep climbing
  with no `rejected` ever appearing, suspect the token before you suspect the
  network.

Logs go to stderr (startup banner with `device_id`/fingerprint, state dir,
then any bridge-loop or ctl-server errors) — no separate log file, so wire up
your platform's usual stdout/stderr capture (`docker logs`, a Kubernetes log
pipeline). `SIGTERM` (and `Ctrl-C`/`SIGINT`) triggers a clean shutdown: the
bridge loop and ctl server are cancelled, the ctl socket is unlinked, and the
process exits — no forced kill needed.

Sessions themselves don't depend on the bridge staying up between requests:
they live in `tmux` on the sandbox ([ADR-0001](../../docs/adr/0001-tmux-session-persistence.md)),
so restarting `remora-bridge serve` (a redeploy, a crash-and-restart) never
kills a running agent — reconnect after restart is the same `tmux attach` any
other client reconnect is.

**Known gap:** push-wake delivery (opt-in `[relay] push_wake_url`,
[ADR-0023](../../docs/adr/0023-unifiedpush-first-wake-delivery.md)) is wired
end-to-end for the desktop app, whose session-output pump feeds the wake
trigger, but the headless daemon has no equivalent pump yet — a disconnected
phone paired only to a headless bridge will not currently get a push wake
when a session needs attention, and `[relay] push_wake_url` is consequently
inert in the headless config (carried through migration for when the pump
lands, but read by nothing today). Tracked as a follow-up issue at ship.

## Reproducible builds

**The compiled binary is bit-for-bit reproducible**, using the same recipe as
the relay: pinned base image digests, `cargo build --release --locked`, and
the workspace's `codegen-units = 1` + `strip = true` release profile. Verify
it yourself:

```sh
./scripts/verify-bridge-reproducible.sh
```

CI runs this weekly as a regression guard, on the same cadence as the relay's
and the same Dependabot-driven digest bumps.

**The container image digest is not** reproducible today, unlike the binary
inside it: the `apt-get install openssh-client` layer pulls whatever Debian
has published for `bookworm-slim` at build time, which is not pinned the way
the base image digests are (`snapshot.debian.org` pinning to make the apt
layer itself reproducible is a follow-up). If you need to verify what's
*running*, extract and hash the binary from inside the image and compare it
against a from-source build with the script above — that comparison is
reproducible even though the image digest around it isn't.

## Coexistence

Running a desktop-hosted bridge and a headless `remora-bridge` at the same
time is supported, as long as they are two genuinely *distinct* bridges — each
with its own `device_id`, static identity, and roster (see
[Migration](#migration-from-the-desktop-bridge) for how to move one bridge
from one host to the other; don't run the same identity in both places at
once). Devices see each bridge's sessions once, under that bridge — there is
no merging or deduplication across bridges, because there is no shared state
between them to merge. Two bridges (desktop + headless, or two headlesses)
each mutating the same underlying host/project config independently is
uncoordinated today, the same as it already is for two desktop installs
pointed at the same sandbox: nothing stops two processes from resolving the
same project and spawning conflicting sessions on it. That's a pre-existing
property of the direct-mode transport layer, not something specific to
running one of the two bridges headless.

## Related

- [ADR-0021](../../docs/adr/0021-blind-relay-bridge-trust-model.md) — the
  trust model this crate implements (blind relay / user-side bridge split,
  pairing, metadata policy, threat model).
- [ADR-0023](../../docs/adr/0023-unifiedpush-first-wake-delivery.md) — the
  opt-in push-wake design `[relay] push_wake_url` and the relay's `[push]`
  config feed into.
- [ADR-0011](../../docs/adr/0011-ssh-connection-multiplexing-direct-mode.md) — why
  `~/.ssh` must stay writable in the container.
- [crates/remora-relay/README.md](../remora-relay/README.md) — the relay's own
  operator guide (config, close codes, audit mode, reproducible builds).
- `crates/remora-bridge/tests/` — `pair_ceremony.rs` (real end-to-end pairing
  over a real relay + real `serve` binary), `daemon_cli.rs`, `loopback.rs`.
