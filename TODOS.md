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
