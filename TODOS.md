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
- **Priority note (stage-11 eng review):** the desktop app now drives real ssh
  (PR A wires the bridge off the fake) under the sidebar's 4s discovery poll,
  and stage-11 `reconnectAll` opens one ssh connection per open tab on wake.
  Every one is a fresh TCP+auth handshake today. Bumps this up: real-transport
  dogfooding will feel the churn immediately. See
  `docs/superpowers/specs/2026-06-17-reconnect-respawn-design.md` (P1).

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
- **kubectl note (stage-12 eng review):** the kubectl transport shares this
  gap and deliberately does NOT use `--request-timeout` to bound execution
  (it would sever a legitimately-slow `git worktree add`, since for `kubectl
  exec` the streamed command is one long API request — unlike ssh's
  `ConnectTimeout`, which bounds only the handshake). Design this watchdog at
  the shared `RemoteExec`/`capture` seam so both transports get it at once.

## kubectl exec connection reuse

- **What:** Avoid a fresh API-server round-trip (TLS + auth + SPDY/websocket
  upgrade to the kubelet) for every `kubectl exec`. A worktree-mode spawn fires
  ~5-6 execs (worktree-add, new-session, 3× set-environment, attach); `list()`
  fires `1 + N + M`.
- **Why:** On a high-RTT cluster every op pays full connection setup, so spawns
  and the discovery poll feel sluggish — the kubectl analog of the ssh
  ControlMaster cost, multiplied by the same `list()` fan-out.
- **How:** kubectl has no `ControlMaster` equivalent. Options: a persistent
  `kubectl port-forward` + reused channel, or drop the `kubectl` binary and
  drive the client-go/SPDY streaming exec API directly (heavier, pulls in a
  k8s client). Investigate which fits the `RemoteExec` seam.
- **Pros:** Faster kubectl spawns and discovery. **Cons:** Substantial
  transport-specific machinery (port-forward lifecycle or a k8s client dep);
  breaks parity with the unoptimized ssh path until ControlMaster also lands.
- **Context:** Stage-12 eng review (perf section). kubectl shipped without it
  to keep parity with how ssh ships (no multiplexing). See
  `docs/superpowers/specs/2026-06-20-kubectl-transport-design.md`.
- **Depends on:** none; pairs conceptually with the ssh ControlMaster item.

## App-level liveness heartbeat (kubectl idle dead-link detection)

- **What:** Detect a half-open/dead channel for an *idle* session at the app
  (or future relay) layer, independent of transport keepalive.
- **Why:** ssh surfaces a dead link as channel death in ~45s via
  `ServerAliveInterval/CountMax`. `kubectl exec` exposes no keepalive knob, so
  an idle kubectl tab only notices death on OS TCP timeout (minutes). The
  stage-11 reconnect-on-focus path already re-attaches on wake, so the hero
  scenario works; this only tightens sub-minute idle detection.
- **How:** A periodic app-side liveness probe per open channel (or a relay
  heartbeat) that tears down and flips the tab to reconnect on miss. Benefits
  both transports but only kubectl *needs* it.
- **Pros:** Uniform, transport-independent dead-link detection. **Cons:** New
  app/bridge plumbing + a probe cadence to tune; ssh already covers its case.
- **Context:** Stage-12 eng review (architecture, Finding 3). Accepted as a
  documented kubectl limitation for the MVP. See
  `docs/superpowers/specs/2026-06-20-kubectl-transport-design.md`.
- **Depends on:** none.

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
- **Update (stage-11 PR-A `/review`, F3+F5):** the bridge's `list()` now also
  loops over **hosts** sequentially (`Bridge::list` awaits each host's
  `SshSource::list()` one at a time). With N ssh hosts a slow/unreachable one
  serializes the rest (~N×ConnectTimeout worst case). Parallelize this
  cross-host loop (`JoinSet`/`join_all`) alongside the per-source `N+M` fan-out.
  Also (F5): the all-hosts-down error currently surfaces only the *last*
  failing host's cause — aggregate all per-host errors when you touch this.
  Zero impact at one host (hermes); revisit when host #2 is added.

## Coalesce bridge output + binary byte codec (PTY firehose backpressure)

- **What:** In the Tauri bridge's PTY→frontend forward task, batch bytes per
  render tick before sending, and/or swap the JSON-number-array byte encoding
  for a binary codec. Today the forward task pushes each `ChannelOutput` chunk
  through `ipc::Channel::send` 1:1.
- **Why:** Past the bridge's 256-message core mpsc, `ipc::Channel::send` is
  fire-and-forget — no webview backpressure. A real PTY firehose (a big build
  log, `yes`) can pile messages into the IPC/webview layer unbounded and spike
  memory. The number-array encoding also bloats each chunk ~3-4x over the wire.
  Neither matters on the stage-7 fake (echo, hand-driven); both bite once a
  real terminal (stage 8) and real transports (stage 9) land.
- **Pros:** Firehose-safe; smaller, faster output payloads. **Cons:** can't be
  exercised or tuned without a real transport + xterm, so it's premature now.
- **Context:** Stage-7 eng review (Architecture finding 2 / D3-2A). Deliberately
  deferred to keep the fake-only bridge small; lands with its first real
  firehose consumer.
- **Depends on:** stage 8 (xterm terminal) + stage 9 (real transports) so it
  can be measured.

## Define multiple-attach / one-channel-per-session policy (stage 9-10 UI)

- **What:** Decide and implement what happens when the UI opens more than one
  output channel to the same live session. The bridge is deliberately a dumb
  multiplexer (it permits N handles per session); the *policy* (one tab per
  session? surface eviction?) belongs to the spawn/sidebar UI.
- **Why:** Behavior diverges by transport and the fake hides it: the
  `FakeSessionSource` gives each attach an independent echo channel, but real
  ssh evicts the prior client (`tmux attach -d`, per the `SessionSource::attach`
  doc). Without a defined policy the stage-9/10 UI can ship two tabs fighting
  over one session, or rediscover the fake-vs-ssh divergence cold.
- **Pros:** Avoids a confusing multi-attach UX bug; pins down the fake-vs-ssh
  divergence in one place. **Cons:** none beyond a backlog entry; enforcing it
  in the bridge now would be premature (eviction is already the transport's
  job, so bridge-side policy could fight it).
- **Context:** Stage-7 eng review (outside-voice finding / D8-8A). Same
  `fake-as-contract-overspecification` shape as the list-ordering call.
- **Depends on:** stage 9 (spawn UI) / stage 10 (sidebar attach).
- **Update (stage-9 eng review):** stage 9 implements the *single-window*
  slice — tabs are deduped by `project/session`, so one window cannot open two
  tabs to the same session. What remains: the *cross-window* case (two app
  windows attaching the same session) and surfacing ssh's `attach -d` eviction
  to the user. Cross-window is gated on the per-webview handle work below.

## Per-webview handle ownership (stage 8+ multi-window)

- **What:** Scope `ChannelHandle`s to the webview/window that opened them, so
  one window cannot drive (`session_write`/`session_resize`/`session_close`)
  another window's channel. Today the bridge registry is process-global and a
  handle is just a `u64`; any caller that knows (or guesses) an integer hits the
  same registry. As part of this, tighten `ChannelHandle`'s inner field from
  `pub` to `pub(crate)` (serde/specta derive don't need the field public) so
  handles can't be forged outside the bridge crate.
- **Why:** Harmless while the app is single-window/single-user (stage 7), but
  once stage 8 (terminal) and a tabbed/multi-window UI land, cross-window handle
  access is a real trust-boundary slip: window A could write keystrokes into or
  close window B's session. Handles are sequential `AtomicU64` values, so they
  are trivially guessable.
- **Pros:** Closes a multi-window trust boundary before it can be exploited;
  the field-visibility tightening is free. **Cons:** needs a per-webview
  identity to key on (Tauri `WebviewWindow` label or similar), which only exists
  once there's a real window/tab model — premature to build now.
- **Context:** Stage-7 `/review` (adversarial pass, INFORMATIONAL — not
  exploitable at single-window stage 7). Flagged as the one forward-looking item
  worth tracking; the `pub`-field tightening can be done anytime.
- **Depends on:** stage 8 (terminal) / a multi-window or tabbed UI model.

## Terminal eviction / virtualization for large tab counts

- **What:** Cap or virtualize the number of simultaneously mounted terminals.
  Stage 9 keeps every open tab's xterm instance, `ResizeObserver`, and bridge
  channel mounted for the tab's lifetime (so scrollback survives tab switches).
- **Why:** Resource use grows linearly with open tabs. Fine at the expected
  scale (a handful of live sessions), but unbounded — many tabs accumulate
  xterm buffers + observers + open channels with no ceiling.
- **How:** Cap mounted terminals (e.g. LRU); dispose the least-recently-used
  terminal and reattach on focus. Note the snag: disposing loses xterm
  scrollback, and the `connection.ts` buffer is a one-shot drain, not durable
  history — so eviction needs a scrollback-handoff story (or accepts losing
  scrollback for evicted tabs). Tie the decision to that.
- **Pros:** Bounded memory/observer/channel footprint. **Cons:** disposal +
  scrollback handoff complexity for a problem that doesn't exist at current
  scale; premature now.
- **Context:** Stage-9 eng review (Codex outside voice). Keep-all-mounted is the
  deliberate stage-9 model (`docs/superpowers/specs/2026-06-16-stage-9-tabs-spawn-design.md`,
  "Scaling assumption").
- **Depends on:** real usage data on typical tab counts; not blocking.

## Complete the WAI-ARIA tabs pattern (tab↔panel wiring + roving tabindex)

- **What:** Finish the ARIA tabs pattern for the tab bar. Stage 9 ships the
  basics (`role="tablist"`/`role="tab"`/`aria-selected`, `role="presentation"`
  on the tab wrapper, labelled close buttons, visible focus ring). Still missing:
  `aria-controls` on each tab → its pane `id`; `role="tabpanel"` +
  `aria-labelledby` on each pane; and roving-tabindex + arrow-key navigation
  between tabs (the standard tab keyboard model, vs the current Tab-key order).
- **Why:** Screen-reader users currently can't jump tab→panel, and keyboard tab
  switching uses Tab instead of arrow keys. Full compliance is the complete a11y
  experience; the stage-9 basics are the floor, not the ceiling.
- **How:** In `App.tsx` give each pane `id={`panel-${t.key}`}`, `role="tabpanel"`,
  `aria-labelledby={`tab-${t.key}`}`; in `TabBar.tsx` give each tab button
  `id={`tab-${t.key}`}`, `aria-controls={`panel-${t.key}`}`, and roving
  `tabIndex` (0 on active, -1 otherwise) with an arrow-key handler on the
  tablist. Do it as one coherent pass.
- **Pros:** Complete keyboard + screen-reader tab UX. **Cons:** touches both
  TabBar and App pane rendering; partial (controls-only) wiring is worse than
  none, so do the whole pattern together.
- **Context:** Stage-9 `/review` (design specialist + Codex adversarial both
  flagged the tab↔panel gap). Deferred (decision 6 scoped "basic a11y", which
  the stage-9 fixes satisfy). See
  `docs/superpowers/specs/2026-06-16-stage-9-tabs-spawn-design.md`.
- **Depends on:** none; pairs naturally with any future tab-reordering UX.

## Webview e2e for the hide/show terminal refit + sidebar click-to-attach

- **What:** A webview end-to-end test harness covering the UI behaviours that
  vitest's node env can't: (a) spawn two session tabs, switch between them, and
  assert each terminal refits to the window and scrollback persists; (b) **stage
  10 sidebar** — click a live session row attaches/focuses the right tab, a
  stopped row is inert (not clickable), and the active-tab highlight tracks the
  selected row.
- **Why:** vitest runs in `environment: "node"` with no jsdom/RTL, so neither
  the real `ResizeObserver` + xterm fit path (stage 9) nor the sidebar's DOM
  click-wiring / disabled-row / highlight behaviour (stage 10) executes under
  unit tests. Both stages cover the logic in plain node-testable modules
  (`TerminalController`/`DiscoveryStore`/`buildTree`) + manual QA; an e2e closes
  the render-layer gap.
- **Pros:** Real verification of the refit + scrollback contract and the
  click-to-attach interaction (the exact bug class pure-logic tests miss:
  stopped-row-still-clickable, wrong session opened, highlight mismatch).
  **Cons:** needs a webview e2e harness that doesn't exist yet; standing one up
  is out of scope for a frontend-only stage. (Alternative considered for stage
  10: add jsdom + @testing-library/react for one Sidebar test — declined to keep
  the stage dep-free, consistent with stage 9.)
- **Context:** Stage-9 eng review (Codex: riskiest UI behaviour unverified) +
  stage-10 eng review T1 (Codex #12: click-to-attach coverage gap; user chose
  defer-but-track). Manual QA is the stopgap for both; see each spec's
  "Known coverage gap".
- **Depends on:** stage 16 (Desktop CI & packaging), the natural home for an
  e2e harness.

## Session fingerprint on attach (don't trust the tmux name alone)

- **What:** Before/while attaching, verify the target tmux session is the
  Remora session we think it is — match a fingerprint (e.g. the `REMORA_*`
  session-env metadata, or a created-at/worktree marker) rather than trusting
  the `remora_<project>_<session>` name alone.
- **Why:** Attach is name-based. If the tmux server restarted and a same-named
  session was recreated by something else, or a name was manually reused,
  attach (and stage-11 reconnect) could land on the wrong process. Discovered
  state is untrusted input per ADR-0004, so the name is a hint, not proof.
- **How:** On attach, read the session's `REMORA_*` env (already used by
  discovery) and compare against expected; on mismatch, treat as
  stopped/unknown rather than attaching. Pairs with the tmux `#{E:}` inline
  metadata item (one round-trip already carries the fingerprint).
- **Pros:** Closes a wrong-process attach hole that reconnect widens (it
  re-attaches automatically). **Cons:** an extra check on the hot attach path;
  needs a stable fingerprint definition; low likelihood at single-user scale.
- **Context:** Stage-11 eng review, outside voice (Codex N4). Out of stage-11
  scope (name-based attach is what stages 4–6 already ship); tracked as a
  hardening. See `docs/superpowers/specs/2026-06-17-reconnect-respawn-design.md`.
- **Depends on:** pairs with the tmux 3.0 `#{E:}` metadata item.

## Config file watcher for live sidebar reload

- **What:** Watch the per-device `config.toml` and auto-refresh the sidebar tree
  when it changes on disk, instead of requiring the manual refresh button or an
  app restart.
- **Why:** Stage 10 fetches config once and re-reads it only on manual refresh.
  An external edit (adding a host/project) won't appear until the user clicks
  refresh. A watcher makes config edits feel live, matching the read-only
  sidebar's "reflects your config" intent.
- **Pros:** Better config-editing UX; natural pairing with the read-only tree.
  **Cons:** file-watching has cross-platform nuance — editors save via
  atomic-rename (watch the dir, not the inode), and events need debouncing;
  pulls in a watcher dep or Tauri fs-watch plumbing + a `config_get` re-emit
  path to the frontend.
- **Context:** Stage-10 eng review (Codex outside voice #5). Manual refresh
  already covers the explicit case; this is the live-reload polish, deferred to
  its own focused change.
- **Depends on:** stage 10 `config_get` merged.

## Bound poll-path resolution cost for command-form kubectl fields

- **What:** Avoid re-running a command-form kubectl field's selector on every
  4s discovery poll — e.g. a short-TTL memo of the resolved host shared across
  back-to-back `list()` calls, so a `kubectl get pods` selector isn't a network
  round-trip every tick.
- **Why:** `list()` is polled every 4s while the window is focused
  (`discovery-store.ts:62`), and resolution runs at the top of every
  `SessionSource` method. A command-form pod therefore fires its selector (a k8s
  API call) every ~4s per host. The in-flight guard + pause-while-hidden cap the
  damage but don't eliminate steady network load.
- **Pros:** Cuts repeated API calls; quieter logs/credentials usage. **Cons:**
  reintroduces a (short) staleness window the design deliberately avoided
  ("the pod changes" — caching across connects was rejected); adds memo state +
  invalidation. A TTL memo ≠ the rejected cross-connect cache, but it's adjacent.
- **Context:** Issue #52 eng review, Performance section (Issue 4). Deferred so
  the first cut stays minimal; revisit if dogfooding on hermes shows the poll
  cost biting. See `docs/adr/0008-dynamic-kubectl-field-resolution.md`.
- **Depends on:** the resolution seam (issue #52 PR).

## Enforce the command-field trust boundary for synced/relayed config

- **What:** Reject or strip `{ command }` kubectl fields from any config that did
  not originate as the local user's self-authored file — i.e. when config sync or
  a relay-distributed config lands. The trust model itself is now *documented*
  (ADR-0008 "Trust boundary" section); this is the remaining *enforcement* work.
- **Why:** A command field executes local code with the user's privileges on
  every ~4s discovery poll. Today that's safe because the config is local and
  self-authored (ADR-0004). The moment Remora's relay/multi-device vision lets a
  config cross a device boundary, an attacker-authored command field becomes
  RCE-on-open — so the synced path must drop/reject command fields.
- **Pros:** Closes the RCE-on-sync hole before config distribution ships.
  **Cons:** needs a provenance signal (which configs are "untrusted") and a
  strip/reject path with clear user feedback.
- **Context:** Issue #52 eng review, outside-voice cross-model tension T2. The
  documentation half landed with ADR-0008; only enforcement is deferred.
- **Depends on:** config sync / relay config distribution being designed.

## Re-resolve-on-vanish retry for command-form pods (TOCTOU)

- **What:** When a `kubectl exec` fails because the resolved pod no longer
  exists (resolved at lookup, gone by exec), re-resolve once and retry instead
  of surfacing a hard Transport error.
- **Why:** A pod swap between `resolve_local_command` and the subsequent exec is
  a real race during the exact scenario this feature targets (pod churn). Today
  the connect fails and the user must re-trigger (which re-resolves); a bounded
  auto-retry smooths that.
- **Pros:** Smoother UX during a pod swap. **Cons:** needs an error classifier
  ("which exec failures mean re-resolve"), retry bounds to avoid masking genuine
  failures, and care not to loop. The manual re-trigger path already recovers.
- **Context:** Issue #52 eng review, outside voice (Codex pt 6). Deferred as a
  P3 hardening; ship fail-and-retry first.
- **Depends on:** the resolution seam (issue #52 PR).

## Packaged-app PATH + Windows support for command-form fields

- **What:** Make command-form resolution work from a GUI-launched, packaged
  Tauri app: a GUI process (macOS Finder launch) often lacks the user's shell
  `PATH`, so `kubectl` isn't found even though it works in their terminal; and
  `sh -c` doesn't exist on Windows.
- **Why:** Resolution runs the field via `sh -c` in whatever environment the app
  process inherits. The existing kubectl transport already shells out locally so
  the PATH issue partly pre-exists, but command resolution widens it and adds an
  explicit `sh` dependency.
- **Pros:** Command fields work in shipped builds + on Windows. **Cons:**
  cross-platform process-env handling is fiddly (login-shell PATH hydration, a
  Windows resolution path / `cmd` vs `sh`).
- **Context:** Issue #52 eng review, outside voice (Codex pt 9). Pre-alpha
  dogfood is hermes (Linux/macOS, terminal-launched), so unaffected now; revisit
  at packaging (stage 16) / Windows support.
- **Depends on:** stage 16 (Desktop CI & packaging); Windows support.

## Detect ambiguous selectors (matched N pods, expected 1)

- **What:** Surface a clear "selector matched N pods, expected 1" signal instead
  of silently accepting whatever `head -n1` returns. E.g. detect a multi-line
  raw selector result before the user masks it with `head -n1`, or offer a
  strict non-`head` form.
- **Why:** ADR-0008 documents the single-active-pod assumption, but the code
  still lets a 3-pod match resolve to an arbitrary first pod with no warning —
  the ambiguity is masked, not detected. Worst at multi-replica/HPA.
- **Pros:** Turns a silent footgun into a clear error. **Cons:** fights the
  decided "user pipes `head -n1`" ergonomics; needs a UX path for the error.
- **Context:** Issue #52 eng review, outside voice (Codex pt 5) + semantics
  section (T3, option not taken). ADR note ships now; active detection deferred.
- **Depends on:** the resolution seam (issue #52 PR).
## Guard a project default that points at an empty-command (plain-shell) agent

- **What:** Warn (or block) in config validation when a project's `agent`
  (its default) resolves to an agent whose `command` is `[]` (a plain-shell
  agent, introduced for issue #35).
- **Why:** New sessions in that project intentionally default to a shell, but
  a *stopped* session that lost its tmux env (e.g. after a pod/app restart,
  when discovery carries no `REMORA_AGENT`) respawns as the project default —
  which would now be a plain shell instead of the agent it originally ran.
  Surprising for a project whose real default *should* be an agent.
- **Pros:** Catches an easy-to-miss footgun at config time, not at respawn
  time when the user is confused why their agent didn't come back.
- **Cons:** Extra validation branch + tests; risk of false-positive warnings
  for users who genuinely want a shell-default project.
- **Context:** Raised in the #35 (no-agent / plain shell) eng review as a
  deliberately-deferred edge. The core feature (empty-command agent → plain
  shell) does not need this guard to work; it only affects the respawn-after-
  restart path for stopped sessions, which already loses agent identity for
  every project (stopped sessions carry no env). This guard would make the
  shell-default case explicit rather than silent.
- **Depends on:** the #35 empty-command-agent feature landing first.
