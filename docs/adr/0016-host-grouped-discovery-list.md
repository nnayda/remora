# 0016. Report discovery as per-host buckets, retain a transiently-down host's rows

- **Status:** Accepted
- **Date:** 2026-06-26
- **Issue/PR:** [#159](https://github.com/nnayda/remora/issues/159)

## Context

Discovery aggregates sessions across every configured host. The bridge's
`list` swallowed a per-host `source.list()` error, skipped that host, and
returned a *successful* partial flat list — it only returned `Err` when
*every* host failed. A `TODO(stage 11+)` flagged that this "silently drops a
down host".

The frontend's safety net — retain the last-good list — fired only on a thrown
rejection, i.e. only when all hosts were down. A transient single-host blip
(network hiccup, ssh stall, a momentarily unreachable host) therefore produced
a successful *shorter* list, so that host's session rows vanished from the
sidebar on the next 4s poll and reappeared a few seconds later. The whole
sidebar reshuffled on a momentary drop. The flat list carried no per-host
status, so the frontend had no way to tell "this host blipped" from "these
sessions ended".

## Decision

The bridge `list` returns one bucket per attempted host instead of a flat list:
`SessionListDto { hosts: Vec<HostSessionsDto> }` where
`HostSessionsDto { host_id, available, sessions }` (config order; sessions
sorted by `(project_id, session_id)` within each bucket). A host whose
`source.list()` errors is reported `available: false` with no sessions;
`Err(BridgeError::Transport)` is returned only when *every* host fails, which
keeps the existing global "discovery unavailable" banner. `remora-protocol`'s
`SessionMeta` is unchanged — host identity is config identity, carried by the
enclosing bucket at the desktop aggregation boundary, so core/protocol stay
host-agnostic (the one rule, ADR-0003/0004).

The desktop `DiscoveryStore` owns retention and timing. It keeps a per-host map
of last-good rows plus the timestamp of the first poll that found the host down.
A reachable host is authoritative (including a reachable-but-empty host, which
clears its rows — ended sessions still prune immediately). An unavailable host's
last-good rows are retained and its sessions marked "reconnecting" (the sidebar
dims them) until it has been continuously unreachable for longer than a 15s
grace window measured **from that first-down detection** (so a host gets a full
reconnecting window however slow its transport is to surface the error), after
which they are pruned; a host removed from config is reconciled out at once.
Resuming from a hidden window restarts the grace timer for any already-failing
host so a long hidden gap never prunes-then-reappears. This resolves the
`TODO(stage 11+)`.

## Alternatives considered

- **Do nothing / widen the poll interval.** A longer interval shrinks but does
  not close the flicker window, and slows genuine discovery. Rejected.
- **Frontend-only grace period on the flat list.** Hold any disappeared
  session for N polls before pruning. Needs no contract change, but cannot
  distinguish a host blip from a session that legitimately ended (so ended
  sessions linger the full window) and leaves the silent-drop TODO unresolved.
  Rejected.
- **Report only the failed host ids (`unavailable_hosts: Vec<String>`)** and
  have the frontend infer the reachable set as `configHosts − unavailable`.
  The frontend's config snapshot is loaded separately from the bridge's
  per-call config load, so the two host sets can diverge, and a
  reachable-but-empty host is then indistinguishable from a host that was not
  polled. Rejected in favor of reporting every attempted host's status.
- **Flat session list + a `host_id` stamped on each `SessionMetaDto`, plus a
  failed-host list.** Works, but the frontend must re-bucket by host and
  re-sort, and `SessionMeta`/its DTO grow a host field. The nested
  `hosts: [{ … , sessions }]` shape carries host identity structurally with
  fewer moving parts. Rejected.

## Consequences

- The silent-drop `TODO(stage 11+)` is resolved: one host down can never blank
  the sidebar, and the contract now names which hosts are reachable.
- A transient single-host drop keeps that host's rows in place (dimmed,
  "reconnecting"); other hosts are unaffected; all-hosts-down still surfaces
  `discoveryUnavailable`. A single-host setup whose only host is genuinely gone
  retains its last-known rows under that banner — deliberately, since a
  clearly-flagged stale list beats an empty sidebar.
- The list contract is no longer a flat `Vec<SessionMetaDto>`; consumers read
  per-host buckets and the frontend flattens for rendering. The grace window is
  a fixed 15s wall-clock constant in the store, sampled at poll cadence (a
  genuinely-gone host prunes within ~one poll of the deadline).
- A future relay must preserve per-host availability to keep this behavior;
  tracked with the relay-parity work in docs/ROADMAP.md.
