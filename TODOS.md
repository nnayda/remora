# TODOS

Deferred work captured during reviews. Each item has enough context to pick
up cold.

## ssh `ControlMaster` connection multiplexing

- **What:** Reuse one ssh connection across all ops to a host (spawn's
  worktree-add + create + attach, plus stage-4 attach and stage-6 list)
  instead of a fresh TCP+SSH+auth handshake per invocation.
- **Why:** A single worktree-mode spawn opens ~3 ssh connections; on a
  high-RTT link that's several seconds of pure connection setup before the
  agent is interactive. `-o ControlMaster=auto -o ControlPath=… -o
  ControlPersist=…` (openssh built-in, Layer 1) collapses them to one.
- **Pros:** Faster first session and every subsequent op; lower auth load.
- **Cons:** Control-socket path lifecycle + cleanup; must be applied
  consistently across `ssh_base_argv` so spawn/attach/list share the master.
- **Context:** Raised in the stage-5 eng review (finding 2) and confirmed by
  the outside voice as the optimization that makes un-batching spawn's tmux
  commands free. Design it once across all ssh ops, not per-call. See
  `docs/superpowers/specs/2026-06-15-ssh-spawn-design.md` (Deferred section).
- **Depends on:** none; best done after stage 6 (list) exists so all three
  ops adopt the shared base argv together.

## ssh execution-phase timeout (watchdog)

- **What:** Bound how long a blocking remote command (`RealSshExec::run` →
  `std::process::Command::output()`) can run. Today only the *connect* phase
  is bounded (`-o ConnectTimeout=10`); a remote `git`/`tmux` that hangs
  mid-execution (NFS stall, lock contention) while the ssh session stays alive
  blocks the `spawn_blocking` thread indefinitely.
- **Why:** Enough concurrent spawns/retries against a degraded host fill the
  tokio blocking pool and starve the runtime.
- **How:** `std::process` has no built-in timeout — spawn the child, wait on a
  watchdog thread with a deadline, `kill()` on expiry (or move to a timeout-
  capable exec). Add at the `SshExec`/`RealSshExec` seam so the fake is
  unaffected.
- **Context:** Raised in `/review` (adversarial pass) on the stage-5 ssh-spawn
  branch (`feat/ssh-spawn`). ConnectTimeout was added inline; the execution
  watchdog was scoped out as heavier work.
- **Depends on:** none.
- **Priority note (stage-6 eng review):** `SshSource::list()` issues
  `1 + N + M` blocking remote commands per discovery refresh (vs spawn's ~3),
  so the unbounded-execution exposure multiplies. Bump this above ControlMaster
  once discovery ships.

## tmux 3.0 `#{E:}` inline session metadata (collapse discovery round-trips)

- **What:** Read `REMORA_*` session metadata inline in the single
  `tmux list-sessions -F '…#{E:REMORA_AGENT}…'` call instead of one
  `show-environment` per live session, collapsing `list()`'s `1 + N` round-trips
  to `1` (+ M worktree scans).
- **Why:** N sequential ssh handshakes for metadata is the bulk of discovery
  latency on a high-RTT link. tmux 3.0's `#{E:VAR}` format expands a session
  environment variable during `list-sessions`, so one round-trip carries every
  session's metadata. Also *less* code (one parse, no per-session loop).
- **Pros:** Faster discovery; fewer connections; simpler orchestration.
- **Cons:** Needs tmux ≥ 3.0 — on older tmux the metadata is silently empty
  (session still `Live`, a graceful version cliff). The exact `#{E:}` format
  syntax must be verified against the target tmux in the e2e before adopting.
- **Context:** Stage-6 eng review Perf P1. Stage 6 deliberately ships the
  portable per-session `show-environment` (consistent with stage-5's
  `set-environment`-over-`new-session -e` portability choice). Fold this into
  the ControlMaster work, which is when discovery's round-trip cost gets
  optimized anyway. See `docs/superpowers/specs/2026-06-15-session-discovery-design.md`.
- **Depends on:** stage 6 (`list`) merged; pairs with ControlMaster.

## `LC_ALL=C` locale hardening for remote-command stderr classification

- **What:** Force a C locale on the remote tmux/git commands whose stderr we
  pattern-match (`tmux new-session` "duplicate session", `tmux list-sessions`
  "no server running" / "no sessions"), so the English-substring matches are
  reliable regardless of the sandbox's `LC_MESSAGES`.
- **Why:** Today classification matches English diagnostics case-insensitively.
  A non-English remote locale prints e.g. "kein Server", so a "no sessions"
  state would be misclassified as a `Transport` error (scary error where the
  truth is "zero live sessions"), and a duplicate-session race could slip the
  fail-closed lock.
- **Pros:** Robust classification independent of remote locale; one consistent
  rule across spawn (stage 5) and discovery (stage 6).
- **Cons:** Threading `LC_ALL=C` through the remote command (a shell-assignment
  prefix vs argv token) needs care so it survives ssh's remote-shell parse;
  small cross-cutting change to `ssh_base_argv`/command construction.
- **Context:** Stage-6 eng review (decision 9 / Codex #9). Accepted the
  English-match fragility for stage 6 to keep the diff small; retroactively
  covers stage-5's `classify_new_session_failure`.
- **Depends on:** none; cheapest done alongside ControlMaster's `ssh_base_argv`
  rework.

## Bound captured output at the `SshExec` seam (memory)

- **What:** Cap the bytes `RealSshExec::run` reads from a remote command.
  Today it uses `std::process::Command::output()` (an unbounded `Vec<u8>`),
  then `String::from_utf8_lossy(...).into_owned()` makes a second full copy.
- **Why:** `MAX_METADATA_LEN` bounds individual *parsed* env/path values, not
  the aggregate command output. A host with thousands of tmux sessions or
  worktrees (`list-sessions`, `git worktree list --porcelain`) — or a hostile
  sandbox echoing megabytes to stdout — buffers and double-copies it all on
  the `spawn_blocking` thread before any bound applies. This is a memory axis,
  distinct from the latency/round-trip items below and the execution watchdog.
- **How:** Pipe stdout and read at most a fixed cap (e.g. a few hundred KiB)
  via `Read::take`, treating overflow as a `Transport` error or a documented
  truncation; read straight into the capped `String` to drop the double copy.
  Apply at the `RealSshExec`/`SshExec` seam so the fake is unaffected.
- **Context:** Stage-6 `/review` (performance + adversarial passes,
  multi-specialist confirmed). Scoped out to keep the discovery diff small.
- **Depends on:** none.

## Parallelize discovery's `N + M` independent remote calls

- **What:** `SshSource::list()` issues `1` (`list-sessions`) + `N`
  (`show-environment` per live session) + `M` (`git worktree list` per
  worktree project) blocking ssh calls strictly sequentially inside one
  `spawn_blocking`. Every one is mutually independent; fan them out.
- **Why:** Even after ControlMaster makes each handshake cheap, the calls
  still serialize one command-round-trip of RTT each, so discovery latency
  stays `~(1 + N + M) × RTT`. The tmux `#{E:}` item collapses the `N` env
  reads to one; neither it nor ControlMaster consolidates the `M` worktree
  scans — those only get cheaper by running concurrently.
- **Pros:** Discovery latency approaches `~RTT` (fan-out) instead of the sum;
  pairs naturally with ControlMaster (concurrent ssh invocations share the
  master). **Cons:** needs a bounded concurrency limit so a host with many
  sessions/projects doesn't open an unbounded burst; interacts with the
  execution watchdog (each fanned call still needs its own deadline).
- **Context:** Stage-6 `/review` (performance pass). Distinct from the
  ControlMaster and `#{E:}` items, which reduce per-call cost, not the
  serial dispatch.
- **Depends on:** best after ControlMaster (cheap concurrent connections) and
  the execution watchdog (per-call deadlines).

## Respawn should preserve the session's agent, not silently default

- **What:** `SshSource::respawn` rebuilds the spawn with `agent: None`, so a
  session originally launched with a non-default agent (e.g. an override of
  the project default) comes back running the project default after a
  stop→respawn.
- **Why:** The original `REMORA_AGENT` died with the tmux session, so the
  transport alone can't recover it — but discovery *did* surface the live
  agent via `REMORA_AGENT` before the stop. A silent substitution means an
  in-progress session resumes under a different agent in the same worktree
  with no warning, which is surprising and potentially wrong.
- **How:** Either (a) the client persists the last-known agent from the
  pre-stop `list()` and passes it as `SpawnSpec.agent` on respawn, or (b) the
  UI explicitly warns that respawn falls back to the project default. (a) is
  more correct once a client exists to hold the state.
- **Context:** Stage-6 `/review` (adversarial pass). The `respawn` doc is
  honest that the original metadata is unrecoverable; this tracks closing the
  UX gap once a client/UI exists to carry the last-known agent.
- **Depends on:** a client/UI that retains pre-stop discovery state.
