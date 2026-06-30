# 0017. Reduce kubectl exec round-trips instead of reusing connections

- **Status:** Accepted
- **Date:** 2026-06-30
- **Issue/PR:** #106

## Context

Every `kubectl exec` the kubectl transport runs is an independent `kubectl`
subprocess that pays a fresh API-server round-trip: TLS handshake + auth +
the streaming (WebSocket/SPDY) upgrade + the API-server→kubelet hop. The
transport multiplies these:

- a worktree-mode `spawn` fires ~6 execs in a **dependent** chain (probe →
  fetch → symbolic-ref → verify → worktree-add → new-session), then attaches;
- `list()` fires `1 + N + M` **independent** execs (names, per-session
  metadata, `$HOME`, per-project `git worktree list`).

On a high-RTT cluster each op pays full setup, so spawns and the discovery poll
feel sluggish. #106 framed this as the kubectl analog of the ssh ControlMaster
cost (#63): now that ssh multiplexes over one authenticated master (ADR-0011),
kubectl is the transport still paying per-op setup. `kubectl` has no
`ControlMaster` equivalent — separate processes can't share a connection — so
the issue asked us to *investigate which* connection-reuse mechanism fits the
`RemoteExec` seam: a heavy in-process `kube-rs` client, or a `port-forward`
contraption.

The premise behind the issue — "the fix is to reuse the connection" — turned out
to be wrong. We built a throwaway benchmark (issue #106 spike, run on a throwaway
branch, not committed) that times a `K`-step in-pod operation under five
strategies against a live cluster, to find out *which axis* the latency actually
lives on before committing to a mechanism.

## Decision

**Reduce the number of sequential exec round-trips; do not reuse connections via
`kube-rs`.** Concretely, the follow-up implementation work (tracked in #182, not
here) is:

1. **`spawn`: collapse the dependent exec chain into fewer execs.** The
   probe/fetch/ref/verify/worktree-add/new-session steps become one (or a few)
   `sh -c` script(s) emitting structured output the client parses. The steps are
   data-dependent, so they can't be parallelized — but they *can* be batched.
2. **`list()`: parallelize the independent fan-out.** The `N` per-session
   metadata reads and `M` per-project worktree scans fire concurrently rather
   than sequentially. (Batching the per-session reads into one `tmux`/`sh`
   invocation is a complementary option.)

Both are **no-dependency** changes behind the existing `RemoteExec` seam — pure
argv/shell composition, no new crate, no async-runtime restructure, no change to
the pod contract (still `sh` + `tmux` + `git`), and no transport-specific
machinery leaking into core. They are also transport-shaped in the right way:
the ssh transport can adopt the same batching where it helps.

**We will not adopt a `kube-rs` in-process client (the issue's heavier option),
and we reject the `port-forward` option.** See *Alternatives*.

### Evidence

Spike medians, `K=6`-step operation, 3 rounds × 20 iterations, warm-up
discarded, variant order rotated. Live cluster: k8s **v1.36.1**, token auth,
high RTT (~1.2 s per round-trip).

| Variant | strategy | median (ms/op) | vs A | per-exec |
|---|---|--:|--:|--:|
| **A** | kubectl subprocess, sequential (status quo) | 7574 | — | ~1262 ms |
| **B** | kube-rs, fresh `Client` per exec | 5499 | +27% | ~917 ms |
| **C** | kube-rs, one reused `Client` | 3993 | **+47%** | ~665 ms |
| **D** | kubectl subprocess, concurrent | 1357 | **+82%** | — |
| **E** | one kubectl exec, batched steps | 1300 | **+83%** | — |

The result is structural, not incidental: **operation latency is dominated by
the number of sequential round-trips × RTT, not by per-connection setup.**

- Reusing the connection (C) eliminates per-call TLS/auth/config setup and saves
  ~27% per exec versus a cold client (B→C) — real, but it still issues **6
  sequential** round-trips, landing at ~4 s.
- Cutting the round-trip count to one (E) or overlapping the six (D) attacks the
  dominant axis and each cuts **~83%**, reaching ~1.3 s — **3× faster than the
  reused `kube-rs` client**, with no dependency.
- The reused client's ~665 ms/exec floor is the irreducible per-exec cost
  (API-server authz/admission/audit + the kubelet proxy hop + container-runtime
  exec startup) that connection reuse cannot remove and that batching/overlap
  sidestep by issuing fewer/concurrent requests.

Per the decision rule stated before the data was taken — *adopt `kube-rs` only if
it beats the no-dependency levers* — `kube-rs` fails outright: it is slower than
both free options while costing a heavy dependency.

The absolute numbers are RTT-specific (a local/low-RTT cluster shrinks all
wins), but the **ordering is structural** — round-trip-count reduction beats
connection reuse on any link where the per-exec round trip is the unit of cost,
which is exactly the high-RTT case #106 is about.

## Alternatives considered

- **In-process `kube-rs` streaming-exec client with a reused `Client`
  (the issue's primary heavy option).** Rejected. It is *slower* than the free
  levers on the workload that matters (variant C above), because it optimizes
  the wrong axis. It would also cost: a heavy `kube` + `k8s-openapi` dependency
  (compile time, binary size, TLS-stack and version-feature churn); an
  async/runtime bridge, since `RemoteExec` is synchronous inside
  `spawn_blocking` while `kube-rs` is async; a split transport (`kube-rs` for
  `run`, `kubectl` subprocess for the interactive `open_channel`), i.e. two
  auth/config/error paths in one transport; and worse, less familiar error
  diagnostics than `kubectl`. The spike also hit an **intermittent** WebSocket
  exec failure on the 1.36 cluster (`400 Bad Request`: a modern API server
  negotiates HTTP/2 over ALPN, which can't carry the HTTP/1.1 `101 Switching
  Protocols` that exec needs; `kubectl` forces HTTP/1.1, `kube-rs` 4.0 does not
  reliably). It succeeded on a later run, so this is a fragility, not a hard
  block — but a fragility we'd own forever for an option that is already slower.
- **Persistent `kubectl port-forward` + in-pod listener.** Rejected in
  principle, not benchmarked. `exec` and `port-forward` are different
  API-server subresources; `exec` does not ride a forwarded TCP port. Getting
  reuse from a forward requires a long-lived in-pod listener (an sshd, or a
  bespoke agent), which breaks the pod contract (today only `sh`/`tmux`/`git`),
  defeats the kubectl transport's reason to exist (it is for clusters where you
  have *only* API-server exec access; an in-pod sshd means you could have used
  the ssh transport), and adds stateful lifecycle to a stateless transport.
- **Persistent `exec` stream as a framed command runner.** Deferred, not built.
  One long-lived `kubectl exec sh` driven by a framed stdin/stdout protocol
  reuses a single exec stream without an in-pod listener. It would also reduce
  round-trips, but at the cost of a bespoke framing protocol and
  stream-lifecycle supervision — strictly more complex than batching for the
  same goal. Revisit only if batching proves insufficient.
- **Do nothing.** Rejected. The status quo (variant A) is the slowest by a wide
  margin on a high-RTT cluster, directly working against the spawn-latency and
  live-sidebar responsiveness goals — and the fix is cheap.

## Consequences

What becomes easier:

- Spawns and the discovery poll get dramatically faster on high-RTT clusters
  (~83% in the spike) with no new dependency and no change to the pod contract,
  directly serving the spawn-latency and live-sidebar goals.
- The `RemoteExec` seam stays subprocess-shaped and synchronous; no async
  runtime is dragged into the transport, and core/UI stay transport-agnostic.

What becomes harder / what we are committed to:

- **Batched spawn loses per-step error granularity.** Collapsing the dependent
  chain into one `sh -c` means a failure must be attributed via per-step status
  encoding in the script's structured output, rather than reading one exec's
  exit. The implementing change owns that encoding and its tests.
- **`list()` parallelism raises peak concurrent API-server connections** (N+M at
  once instead of one at a time) and concurrent auth — cheap for kubectl's
  cached token/exec-plugin auth, but a bounded fan-out is still prudent.
- **The win is RTT-proportional.** On a local/low-RTT cluster the absolute
  savings shrink; the change is still correct and never slower, but its value is
  greatest exactly where users feel the pain.
- **Connection reuse remains available as a future lever** if a workload ever
  becomes setup-bound rather than round-trip-bound (the B→C signal shows reuse
  *does* help ~27%/exec). Nothing here precludes it; we are sequencing the cheap,
  larger win first.
- **The kubectl/ssh parity argument is retired.** ssh multiplexes (ADR-0011),
  kubectl reduces round-trips — different mechanisms, same goal (fewer/cheaper
  per-op handshakes), each fitted to its transport, with no transport knowledge
  in core.
