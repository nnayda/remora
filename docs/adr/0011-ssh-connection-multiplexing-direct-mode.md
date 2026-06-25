# 0011. Multiplex direct-mode ssh over one authenticated master (ControlMaster)

- **Status:** Accepted
- **Date:** 2026-06-25
- **Issue/PR:** #63

## Context

Every session operation over the ssh transport is an independent ssh
invocation. `ssh_base_argv`
([`crates/remora-core/src/transport/ssh.rs`](../../crates/remora-core/src/transport/ssh.rs))
builds each call with only keepalive and connect-timeout options — there is no
connection reuse. A single discovery + spawn fans out into many short-lived
handshakes (`list-sessions`, `has-session`, `worktree-add`, `new-session`,
`set-environment`) plus the long-lived attach, and each pays a fresh TCP + auth
round trip. Against a hardware security key (a FIDO touch per connection), a
bastion/jump host, or a high-latency link, that is slow and — for the per-touch
case — actively annoying. It works directly against the "<5s spawn" and
live-sidebar responsiveness goals.

OpenSSH already solves this with connection sharing: one authenticated *master*
connection that subsequent calls ride as multiplexed slaves, skipping the
handshake. The question is not whether to use it but how to scope it, where the
control socket lives, how long the warm socket persists, and what the relay
inherits — the parts that are decisions rather than mechanics.

## Decision

We will add three OpenSSH connection-sharing options to `ssh_base_argv`, so
every direct-mode ssh call (the blocking setup commands **and** the long-lived
attach) shares one authenticated master:

- `ControlMaster=auto`
- `ControlPath=~/.ssh/remora-%C`
- `ControlPersist=60s`

This is a pure flag change behind the existing transport seam — no new
dependency, no local filesystem logic, no change to the sandbox.

**`auto`, not `yes`, is the safety choice.** With `auto`, a stale or orphaned
socket (host rebooted, master killed with `-9`, laptop slept past the persist
window) makes ssh fall back to a fresh, non-multiplexed connection instead of
wedging. A normal idle `ControlPersist` exit removes the socket itself. The
"don't let a dead master wedge new connections" requirement is therefore met by
the choice of `auto` rather than by any cleanup code we write — see
*Consequences* for the residual orphan case.

**`~/.ssh/remora-%C` keeps the socket path short, unique, and unprivileged.**
ssh expands the path itself — `~` to the local home directory, `%C` to a
fixed-length hash of (local-host, remote-host, port, user). Because the argv is
exec'd directly (no shell), ssh — not a shell — performs that expansion. The
`%C` hash gives one socket per host without us constructing the path from
`SshHost`'s partial fields (port/user may be `None` and resolved from
`~/.ssh/config`), and being fixed-length it stays well under the ~104-character
unix-socket path limit that a literal `%r@%h:%p` could blow past with long
hostnames. The socket lands in `~/.ssh`, which already exists with `0700`
permissions for ssh users, so the warm socket inherits owner-only access with no
directory creation on our part.

**`ControlPersist=60s` is a deliberately short warm window.** The first call to
a cold host creates the background master; every later call rides it as a slave.
In `attach`, that first call is the sub-second `has-session` preflight
([`ssh.rs`](../../crates/remora-core/src/transport/ssh.rs) `attach`), so the
master is owned by the probe and the long-lived attach is a *dependent slave* —
not, as an earlier draft of this ADR claimed, a master it holds open on its own.
While that slave is connected the master is busy (not idle), so the persist
window only needs to bridge the brief gaps between the short calls and before the
attach connects (discovery → the user scanning the list → spawn). 60s covers
that burst with a small, bounded warm-socket exposure surface.

**Scoped to direct ssh only.** The flags live in `ssh_base_argv`;
`kubectl_base_argv` builds its argv from scratch and shares no master, so the
kubectl transport is untouched (regression-guarded by
`base_argv_carries_no_ssh_connection_multiplexing_options`). Per AGENTS.md, this
stays an ssh-transport implementation detail — the UI and core remain
transport-agnostic.

## Alternatives considered

- **A Remora-owned socket dir (`~/.remora/ssh/cm-%C`), created `0700` in Rust.**
  Cleaner separation from the user's own ssh files and a natural home for a
  future stale-socket reaper, but it introduces local home-resolution + `mkdir`
  logic into a transport that has so far been pure string-building, for no
  security gain over `~/.ssh`'s existing `0700`. Rejected for the first cut;
  revisit if socket ownership ever needs to be Remora-managed.
- **`ControlMaster=yes`.** Forces this connection to be *the* master and fails
  hard if the socket already exists — the exact wedge-on-stale-socket failure
  mode we want to avoid.
- **Construct `ControlPath` from `SshHost` fields ourselves.** Reintroduces the
  path-length risk, duplicates ssh's own `%C` token, and gets the uniqueness key
  wrong when port/user come from `~/.ssh/config` rather than our struct.
- **Proactively reap stale sockets (`ssh -O exit` / unlink on init).** Not
  needed for correctness — `auto` already degrades gracefully — and adds I/O and
  failure modes to every transport construction. Deferred (see *Consequences*).
- **Do nothing.** Leaves every op paying a full handshake; the per-touch
  hardware-key case stays painful and the spawn-latency goal stays out of reach.

## Consequences

What becomes easier:

- The user authenticates once per ~60s burst (best case); sequential discovery,
  spawn, and attach reuse the master and skip the handshake. The hardware-key,
  bastion, and high-latency cases all improve, directly serving the spawn-latency
  and live-sidebar goals.

What becomes harder / what we are committed to:

- **A warm, authenticated socket lingers up to 60s after the *last* connection
  closes — including a deliberate session close and one-shot ops.** Closing the
  attach kills only the local slave (`pty_process` kills the child ssh); the
  background master persists for the full `ControlPersist` window, and a lone
  `list`/`stop`/`remove` against a host you won't touch again still leaves a
  master warm for 60s. This is a small but real change to the security surface: a
  local actor able to read the `0600` socket (i.e. the same uid) could ride the
  master within that window. We accept this bounded exposure as the cost of the
  feature; the persist value is the knob to tighten if that trade-off changes,
  and firing `ssh … -O exit` when the last channel for a host drops would close
  it immediately (a possible follow-up, not required here).
- **Keepalive / half-open detection now rides the master, not the per-connection
  attach.** Because the attach is a slave (see the `ControlPersist` note above),
  its own `ServerAliveInterval`/`ServerAliveCountMax` are inert — keepalive is
  the master's job. This does *not* weaken the ~45s half-open detection that the
  reconnect/respawn UX depends on: every ssh call (including whichever becomes
  the master) carries the identical `ServerAliveInterval=15`/`CountMax=3`, so the
  master always probes on the same schedule and, on a dead link, tears down and
  takes every slave channel (the attach included) with it. What changed is the
  *owner* of the guarantee (now an emergent master-lifecycle property), not the
  timing. Worth a manual half-open check (sleep the laptop mid-attach) when real
  ssh e2e coverage lands.
- **Concurrent cold operations can each create a master, so "authenticate once"
  is best-case, not guaranteed.** The Tauri command layer dispatches `list`,
  `spawn`, and `attach` from independent IPC calls with no per-host
  serialization. Against a cold host, two ops firing together both see no socket
  and both try to become master; OpenSSH resolves the race safely (the loser
  falls back to its own connection), but the user may see two auth prompts /
  hardware-key touches. Priming the master with one connection before fan-out is
  a possible follow-up.
- **`~/.ssh` must exist for the socket to be created.** ssh does not `mkdir` the
  `ControlPath` parent. On a host where the user has never run ssh (no `~/.ssh`),
  the master cannot be created; with `auto` this degrades to a non-multiplexed
  connection (correctness preserved, the feature simply never engages, possibly
  with a per-call stderr warning). True for ssh users in practice, but not
  universal.
- **The rare orphaned-socket case degrades silently to today's behavior.** When
  a master dies uncleanly, its socket lingers; ssh prints "disabling
  multiplexing" and connects normally, so correctness holds but multiplexing
  stays off for that host until the socket is removed (or a later
  `ControlPersist` master replaces it). Proactive reaping is a possible
  follow-up, not a correctness requirement.
- **`%C` requires OpenSSH ≥ 6.7 (2014).** Older clients would treat `%C`
  literally and collide sockets across hosts; this is an accepted floor.
- **Relay mode does not inherit these flags and must not.** In relay mode the
  relay would own masters for many users and hosts at once, where `ControlPath`
  isolation, socket lifetime, and credential separation become a different and
  larger problem than the single-user direct case
  ([ADR-0002](0002-tauri-single-codebase-optional-relay.md)). This decision is
  scoped to direct ssh; relay-side connection sharing is explicitly deferred and
  must be designed on its own terms when the relay milestone lands.
- **Windows OpenSSH does not support ControlMaster multiplexing.** The flags are
  inert or warned-about there; the feature is effectively a no-op on that
  platform, consistent with Remora's macOS/Linux-first desktop targets.
