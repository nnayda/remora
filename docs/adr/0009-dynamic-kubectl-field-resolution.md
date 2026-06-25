# 0009. kubectl host fields may be resolved from a local shell command at connect time

- **Status:** Accepted
- **Date:** 2026-06-24
- **Issue/PR:** #52

## Context

[ADR-0004](0004-local-config-live-session-discovery.md) keeps host connection
parameters as declarative local configuration and explicitly states that ids and
config values are "never interpolated into shell strings". That guard works well
for static hosts, but kubectl pod names are not always static: a pod targeted by
a Deployment or HPA can be replaced with a new name on every restart. Requiring
the user to hand-edit the config on every pod replacement defeats the pod-restart
recovery story that ADR-0004 itself introduced (stopped sessions → respawn).

A kubectl host's `pod`, `namespace`, `context`, and `container` fields need an
opt-in mechanism that resolves the current value from the live cluster at connect
time, without turning every config field into an arbitrary code path.

## Decision

We will allow any of the four kubectl fields to be declared as a
`{ command = "…" }` table instead of a bare string. Only fields declared as
`{ command }` are shell-evaluated; fields kept as bare strings retain the full
ADR-0004 "never interpolated" guard unchanged.

**Evaluation is local, bounded, and validated before use.**

- Execution runs on the client via `sh -c`, at argv-build time, never inside
  the pod. This mirrors how `kubectl` itself resolves context from the local
  kubeconfig.
- A `RESOLVE_TIMEOUT` of 10 s and a 64 KiB output cap bound every invocation;
  a hung or runaway selector fails the connect cleanly instead of freezing
  discovery.
- The resolved string is re-validated through the same `literal_field_problem`
  guard used at load time for literal config values: no control characters, no
  embedded newlines, no leading `-`, no edge whitespace. A resolved value can
  never smuggle a flag or a newline into the kubectl argv.

**Resolution is ephemeral and re-run on every access.**

Resolution runs on every connect and on every discovery poll (~4 s while the
window is focused). The resolved value is never persisted into `SessionMeta` or
any cached state; a renamed pod is picked up automatically with no config edit.

**Single-active-pod assumption.**

A `{ command }` selector is expected to resolve to exactly one pod name. The
resolver fails *closed* on ambiguity: raw multi-line stdout is rejected by the
re-validation guard (`literal_field_problem` treats the embedded newline as a
control character), so a selector that matches N pods errors out unless the user
explicitly collapses it to one line (e.g. piping through `head -n1`). Ambiguity
is therefore only masked when the user opts into `head -n1` themselves; the
implementation never silently picks a pod from multi-line output. Multi-replica
and HPA scenarios — where a selector might legitimately match N pods — are
unsupported; which pod a `head -n1` lands on is the user's responsibility.
A resolution that yields more than one whitespace-separated value (newline-per-pod
from `-o name`, or a space-separated jsonpath list) now surfaces a clear "selector
matched N values, expected exactly 1" error — with the count and a sample of the
matches — instead of the generic control-character rejection (#115); the `head -n1`
path stays masked by construction, since the shell collapses the matches before
Remora sees them.

**Pod-replacement recovery requires resolution + respawn + a persistent worktree.**

Resolution alone is not enough for transparent pod-restart recovery. When the
old pod disappears, the stopped-session detection in ADR-0004 surfaces the
session as *stopped*. Re-connecting requires: (1) the `{ command }` field
resolving to the new pod name, (2) `respawn` recreating the tmux session and
agent in the new pod, and (3) the git worktree surviving on a persistent volume
so that work-in-progress is not lost. All three are necessary; resolution handles
only the first.

**Resolution stays behind the `SessionSource` / `remote.rs` seam.**

`resolve_local_command` lives in remora-core's `transport/remote.rs` and is
called from `KubectlSource` before building the kubectl argv — so resolution is a
transport-internal concern. The UI and the protocol crate have no knowledge of
selectors or resolution: the UI receives only the redacted display DTO, and
`SessionSource` consumers see an opaque transport. (remora-core's transport layer
necessarily *does* know — that is where `resolve_local_command` runs.) This
preserves the ADR-0004 / AGENTS.md rule: UI code never talks to kubectl directly,
and the agent/session abstraction stays selector-unaware.

**Trust boundary.**

`{ command }` fields run arbitrary local code with the user's privileges on every
discovery poll. Safety rests entirely on the guarantee inherited from ADR-0004:
config is a local, self-authored file. Syncing or relaying a config that contains
`{ command }` fields to another device is out of trust scope; such fields in a
relayed config must be rejected or stripped until explicitly revisited. This is
tracked in #114.

## Alternatives considered

- **Expand ADR-0004's shell-evaluation prohibition to cover dynamic pods too.**
  Forces the user to maintain a side-channel script that writes a fresh pod name
  into the config file on every pod restart, externalising the problem without
  solving it.
- **Store the resolved pod name in `SessionMeta` after first connect.**
  Turns a live discovery value into a cached config value that goes stale the
  moment the pod is replaced — the original problem.
- **A first-class label-selector field resolved by calling the kubectl API.**
  More structured, but requires embedding cluster credentials or a kubeconfig
  lookup, re-implementing what `kubectl get pod -l …` already does, and
  constraining the selection language to what Remora knows about. A bare `sh -c`
  command lets users use any kubectl idiom or cluster-specific tooling.
- **Do nothing.** Users must hand-edit the pod name after every restart; the
  pod-restart recovery story in ADR-0004 has a manual gap.

## Consequences

What becomes easier:

- kubectl hosts targeting ephemeral pods (Deployments, ArgoCD managed apps,
  etc.) survive pod replacement without a config edit; the stopped-session UI
  becomes a true one-click respawn path.
- Any local command can serve as the selector — `kubectl`, `jq`, `helm`,
  cluster-specific CLIs — without Remora needing to understand them.

What becomes harder, and what we are committed to:

- The `{ command }` / bare-string duality must be preserved in the config schema,
  the document writer, the editor DTO, and the UI toggle (Tasks 1–6, #52); all
  four layers must agree on which variant is in play.
- Every discovery poll runs `sh -c` for each dynamic field on each kubectl host;
  a slow selector adds latency to the sidebar refresh. The 10 s timeout is the
  backstop; users writing expensive selectors accept that cost.
- The single-active-pod assumption and the masked-ambiguity behaviour are now
  documented behaviour; ambiguity in *raw* multi-match output (more than one
  whitespace-separated value) is detected and reported with a clear "matched N
  values, expected 1" error (#115). The `head -n1` path remains masked by
  construction (the shell collapses the matches before resolution sees them).
- ssh has an analogous dynamic-host scenario (a bastion whose hostname changes).
  `resolve_local_command` is written to be reusable there, but ssh dynamic-host
  support is not implemented in this issue and is deferred to future work.
- The editor channel introduced by ADR-0006 now carries `{ command }` values.
  The local-only and relay-exclusion rules from ADR-0006 apply with equal force;
  command fields must never cross the relay wire.
