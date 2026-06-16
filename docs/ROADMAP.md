# MVP roadmap

One stage = one PR, in build order. Scope notes say *what* each PR delivers,
not how. MVP = the desktop hero scenario in direct mode: spawn sessions from
the app, run them as tabs, survive sleep/close, browse files/diffs/PRs —
no relay, no mobile.

Status markers: ✅ done · ☐ open.

Already done: repo scaffold (#1–#10) and the spine spike (roadmap step 1 in
[VISION.md](VISION.md)). **Phase 1 is complete** (#14–#19); the Tauri bridge
(stage 7) and the Embedded terminal component (stage 8) are also complete;
the next open stage is Tabs + one-click session spawn (stage 9).

## Phase 1 — Protocol & core (the seam) — ✅ complete

1. ✅ **Protocol wire types** (`remora-protocol`) — #14
   Session metadata, spawn spec, session state (live/stopped), and the
   channel messages: bytes in, bytes out, resize. This is the contract
   everything else builds against, so it goes first.

2. ✅ **`SessionSource` trait + test double** (`remora-core`) — #16
   The trait per the spike's implications: spawn → channel, attach → channel,
   list/liveness without attaching; channels are byte-stream + resize,
   nothing smarter. Ship with an in-process fake implementation so every
   later layer can be tested without a sandbox.

3. ✅ **Host/project config model** (`remora-core`) — #15
   The per-device TOML: hosts (transport + connection details), projects
   (path, workspace mode, default agent), and per-agent adapter data
   (launch command). Parsing, validation, good error messages.

4. ✅ **ssh transport: attach + channel** — #17
   First real `SessionSource` backend. Open a PTY channel to an existing
   tmux session over ssh (`attach -d` to evict zombies), stream bytes both
   ways, propagate resize.

5. ✅ **ssh transport: session spawn** — #18
   Create a session end-to-end: worktree + branch (or shared dir), named
   tmux session, agent launched from adapter config. Commands built only
   from local config, never from discovered state.

6. ✅ **Session discovery & join** — #19
   List tmux sessions, parse `remora_<project>_<session>` names, join
   against local config; detect surviving worktrees with no tmux session
   as *stopped*, and support respawn (ADR-0004). Core logic + ssh impl.

## Phase 2 — Desktop shell (the hero scenario) — ☐ open

7. ✅ **Tauri bridge**
   The src-tauri layer: own `SessionSource` instances, expose
   spawn/attach/list/write/resize as Tauri commands, stream PTY output to
   the frontend as events. The UI talks only to this layer.

8. ✅ **Embedded terminal component**
   xterm.js wired to the bridge: render the byte stream, send keystrokes,
   drive resize from the DOM. Never parse bytes — the emulator owns screen
   state (spike lesson).

9. ☐ **Tabs + one-click session spawn**
   Tabbed window, one tab per session; a "new session" flow that picks
   host/project and spawns. This is the first point where the app is
   actually usable.

10. ☐ **Sidebar with live session state**
    Hosts → projects → sessions from the config-and-discovery join,
    refreshed live; click a discovered session to attach it as a tab.

11. ☐ **Reconnect & respawn UX**
    Detect dead channels, re-attach on app focus/restart, re-attach all
    open tabs after sleep; one-click respawn for *stopped* sessions. This
    PR closes the hero scenario: sleep the laptop, reopen, everything is
    live.

12. ☐ **kubectl exec transport**
    Second `SessionSource` backend (same trait, new impl). Validates the
    seam isn't ssh-shaped, and disconnect/zombie semantics get checked
    per-transport as the spike warned.

## Phase 3 — Panels around the terminal — ☐ open

13. ☐ **Repo file browser** — read-only tree + file viewer for the session's
    worktree, via the same transport seam.

14. ☐ **Git diff viewer** — working-tree/branch diff for the session's
    worktree.

15. ☐ **PR review panel** — view the session branch's pull request and its
    review state.

## Phase 4 — Ship it — ☐ open

16. ☐ **Desktop CI & packaging**
    Build matrix on tag: signed/notarized macOS `.dmg`, signed Windows
    installer, Tauri auto-update, GitHub Releases.

17. ☐ **Getting-started docs**
    The "new user gets the hero working in ~2 minutes" path: install,
    sandbox prerequisites (`tmux` + `git` + agent CLI), config example,
    first session.

## Post-MVP (next, but not MVP)

- **Second agent adapter (`codex`)** — proves the agent-agnostic seam with
  config only, no code changes (ADR-0003).
- **`docker exec` transport** — third backend, fast-follow.
- **Relay mode**, then **mobile + push notifications** — VISION.md steps
  6–7.

## Dependencies & parallelism

Hard dependencies per stage (a stage needs these merged first):

| Stage | Depends on |
| --- | --- |
| ✅ 1 Protocol wire types | — |
| ✅ 2 Trait + test double | 1 |
| ✅ 3 Config model | — |
| ✅ 4 ssh attach + channel | 2, 3 |
| ✅ 5 ssh spawn | 3, 4 |
| ✅ 6 Discovery & join | 3, 4 |
| ✅ 7 Tauri bridge | 2 (runs on the test double) |
| ✅ 8 Terminal component | 7 |
| ☐ 9 Tabs + spawn | 5, 8 |
| ☐ 10 Sidebar | 6, 9 |
| ☐ 11 Reconnect & respawn | 6, 10 |
| ☐ 12 kubectl transport | 2, 3 (lessons from 4–6 help, not required) |
| ☐ 13–15 Panels | 4, 9 (independent of each other) |
| ☐ 16 CI & packaging | — |
| ☐ 17 Getting-started docs | 11, 16 |

Critical path to the first usable build: **1 → 2 → 4 → 5 → 8 → 9**.
Then 9 → 10 → 11 closes the hero scenario.

Parallel tracks (the stage-2 test double is what makes track C possible —
the UI never waits on a real transport):

- **Track A — transport:** 2 → 4 → 5 → 6. The critical path; staff it first.
- **Track B — config:** 3 is dependency-free; build alongside 1–2, land
  before 4.
- **Track C — UI:** 7 → 8 starts as soon as 2 lands, driven by the fake
  `SessionSource`; rejoins track A at stage 9.
- **Track D — CI:** 16 anytime. Signing/notarization is slow to debug —
  start it early against the scaffold app rather than at the end.

After stage 9, up to four independent streams: the three panels (13, 14,
15) and kubectl (12). kubectl is worth doing early anyway — a second
transport is the cheapest check that the trait isn't ssh-shaped before
more code piles onto it.
