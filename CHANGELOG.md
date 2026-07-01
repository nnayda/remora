# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Agent prompt preview** (#61): when an agent is waiting on you, the sidebar
  session row now shows what it asked ("the session says: …") on hover. A
  Claude Code Notification hook (`contrib/agent-hooks/claude-code/`) emits the
  text over the existing in-band activity marker; Remora core sanitizes and
  length-caps the untrusted payload and the UI renders it as sandbox-claimed.
  See `docs/agent-hooks.md` and ADR-0018. The richer Activity panel and a phone
  push (with the relay) are follow-ups.
- **Connecting spinner in the sidebar session row** (#170): clicking a session
  to open or attach it now spins a small marine indicator in the row's status
  slot within a frame of the click, instead of reading as a dead click while a
  slow `ssh`/`kubectl` spawn-or-attach runs for several seconds. The store
  publishes the in-flight key the moment an open starts (deriving it from the
  existing double-open `pending` guard, so it can't drift) and clears it when the
  open resolves to a live tab, fails, or is cancelled. Covers both the attach
  (`openSession`) and spawn-from-stopped (`openViaRespawn`) paths. The spinner
  fills the activity-pulse footprint rather than animating beside it, staying
  within the design system's one-expressive-animation rule.
- **ADR-0017: reduce kubectl exec round-trips instead of reusing connections**
  (#106): a throwaway benchmark against a live high-RTT k8s 1.36 cluster showed
  operation latency is dominated by sequential round-trip *count* × RTT, not
  per-connection setup. Batching the dependent `spawn` exec chain (~83% faster)
  and parallelizing the independent `list()` fan-out (~82%) each beat a heavy
  in-process `kube-rs` reused client (~47%, and 3× slower in absolute terms),
  with no new dependency. The ADR records the decision to pursue the no-dep
  round-trip reduction and reject `kube-rs`; implementation is tracked in #182.
- **Batched `spawn`/`respawn` exec chains** (#182): the spawn-side
  implementation of ADR-0017. The dependent `spawn` chain (fetch →
  base-resolution → worktree-add → new-session → passthrough → metadata) and the
  `respawn` stamp tail now run as a single in-pod `sh` script behind the
  existing `RemoteExec` seam — steps framed by ASCII control bytes and parsed
  back in Rust, with per-step failure classification, the worktree-cleanup gate,
  and the #105 fingerprint all preserved. This cuts kubectl worktree-spawn from
  ~11 round-trips to 3 (the kubectl-only binary probe + one script + the
  interactive attach) and ssh to 2, with no new dependency and no change to the
  pod contract (`sh` + `tmux` + `git`). The git base cascade (`origin/HEAD` →
  `origin/main` → `origin/master`, each peeled with `^{commit}`) moves into the
  shell script as the single source of truth — the Rust `resolve_base` is
  removed — and is covered by real-`sh` tests against a live local git repo.
  Discovery (`list()`) batching is the follow-up half of #182.
- **Batched `list()` discovery fan-out** (#182): the second and final half of
  ADR-0017. The discovery poll's independent fan-out — the trusted session
  names, the inline `#{E:}` metadata enrichment, `$HOME`, and one `git worktree
  list` per configured project — now runs as a single `RunAll` batched `sh`
  script instead of `3 + M` separate round-trips, parsed back into the same
  discovery streams. Every property is preserved: the #108 trusted-names /
  untrusted-metadata split, the #190 stale-socket tolerance (a dead tmux server
  still scans worktrees), the #124 `$HOME` canonicalization and its degradation,
  and the per-project scan tolerance. The shared frame builder now strips the
  RS/US delimiter bytes from each step's captured output, so an attacker-set
  tmux `#{E:}` value can never forge a record boundary — making the framing
  unforgeable by construction (this also retroactively hardens the spawn path).
- **Design system documented in `DESIGN.md`** (#150): the shipped token system
  (`apps/desktop/src/styles/tokens/*.css`) now has a written home in the
  [Google design.md](https://github.com/google-labs-code/design.md) format —
  machine-readable token front matter mirroring the CSS, plus prose for the
  decisions that the tokens alone don't capture (host as a bare muted label vs a
  chip, `Tag` for machine values vs `Badge` for status, the single marine
  accent, the activity pulse as the one signature motion, soft depth over heavy
  cards). CI lints it (`pnpm design:lint`, pinned `@google/design.md@0.3.0`) so
  the doc can't silently drift from the CSS. The pulse's canonical color is
  recorded as marine; the stray lavender glow is flagged for retirement (#180).
- **Resizable, collapsible left sidebar** (#156): drag the sidebar's right edge
  to resize it (clamped 180-480px, and never more than ~40% of the window so the
  terminal stays usable), double-click the divider to reset to 240px, or nudge
  it with the keyboard (arrows ±8px, Shift ±32px, Home/End to the bounds). A
  collapse toggle shrinks it to a 56px rail showing one avatar per host with an
  aggregate activity dot (needs > working > idle) and a tooltip, keeping
  quick session access while reclaiming space for the terminal. The chosen width
  and collapsed state persist across launches in `localStorage`. The ≤680px
  mobile single-pane layout is unchanged (the rail never renders there). The
  resize hook and pointer-math handle are written to be reused by the
  right-panel resize (#157).
- **Desktop UI for spawn-time branch and worktree overrides** (#124): the New
  Session dialog replaces the generated "Session id" field with a **Branch
  name** field and an optional **Worktree root** field — the user-chosen branch
  is the session's identity, and the internal `session_id` is minted on the
  client from that branch via a TypeScript port of the core FNV-1a/32
  `derive_session_id`, kept byte-identical to Rust by a shared test-vector
  fixture (core rejects any spawn whose `session_id` doesn't match
  `derive(branch)`, so a hash mismatch surfaces immediately). The sidebar now
  renders that branch as the session's name instead of the internal slug; the
  config editor gains a `worktree_root` field on both hosts and projects
  (feeding the host → project → session cascade); and a primary-checkout
  (Shared) session's removal is relabelled **"Close session"** with
  non-destructive copy, since tearing it down only ends the tmux session and
  never deletes a worktree or branch. This **completes #124** — the discovery
  foundation landed in #155 and the core spawn engine in #160; follow-ups #153
  (`$HOME`-fallback hardening) and #145 (user docs) remain open.
- **New-project "+" on the sidebar section header** (#161): a `+` button next to
  the sidebar "Projects" header opens Settings deep-linked to the new-project
  create form (first field focused), instead of landing on the entity list and
  making you hunt for the projects section. The footer gear still opens Settings
  on the list as before. `SettingsDialog` gains an optional `initialView` prop
  (default `{ kind: "list" }`) that seeds its view state, and `App` routes the
  gear to the list and the new `+` to the project-create form. The button stays
  visible in the empty "No projects yet" state so a first-time user can reach
  the form in one click.
- **Spawn-time worktree path and branch overrides** (#124): when spawning a
  session, you can now specify a custom worktree root path and branch name. The
  `worktree_root` overrides the project and host defaults (which cascade to the
  convention `~/.remora/worktrees/<project-id>`); the `branch` is chosen
  per-session at spawn with no project or host default. The branch can be any
  valid git ref — feature branch, device label, personal naming scheme —
  without the fixed `remora/<session-id>` prefix; when not specified, the
  worktree is created at the back-compat path `~/.remora/worktrees/<project>/<session_id>`
  with branch `remora/<session_id>`, so existing workflows are unchanged. The `session_id` derivation moved from `DefaultHasher` to FNV-1a/32,
  making ids stable across Rust rebuilds and reproducible in TypeScript (closes
  #153's hasher item); client-side session id minting and the "Branch name" +
  `worktree_root` form fields land in a follow-up desktop PR.
- **Branch-identified session discovery** (#124, ADR-0015): the discovery
  foundation that will let a future release place worktrees at custom paths and
  name their branches freely. Discovery no longer parses the
  `~/.remora/worktrees/<project>/<session>` path convention; it joins live tmux
  sessions to their worktrees on the canonical worktree *path*
  (`REMORA_WORKSPACE` ↔ `git worktree list --porcelain`) and reads each
  worktree's *branch* as the session's display identity, so a `git branch -m`
  rename is reflected on the row while reconnect still resolves by the unchanged
  tmux name. Two observable shifts: every worktree of a configured project now
  surfaces — including ones created by hand outside Remora (a whole-workspace
  view) — and a project's primary checkout appears as a Shared session so you can
  work on `main` directly (its remove only closes the session, never touches the
  repo). `SessionMeta` gains an optional `branch`; the internal `session_id` is
  derived from the branch (Mechanism A) so the transport and respawn/teardown
  signatures are unchanged. Spawn behaviour and the path/branch defaults are
  unchanged here — the user-facing `worktree_root`/`branch` override fields land
  in the follow-up PR. Supersedes ADR-0004's path/branch discovery contract;
  hardening and extra test coverage tracked in #153 and #154.
- **Flatten the session sidebar to project rows with a host label**: the sidebar
  was a three-level `host → project → session` tree; it is now a flat
  `project → session` list with the host demoted to a small muted label on each
  project row (carrying an ssh/kubectl transport glyph), in the same visual
  family as the per-session agent label. Host stays only where it earns its keep
  — disambiguating two projects that share a name across hosts — without the
  extra nesting and collapse target. `buildTree` now returns a flat
  `ProjectNode[]` grouped by host via a bucket pass (config-host order;
  dangling-host then synthetic projects trail last), so same-host projects stay
  adjacent and the label confirms identity rather than carrying it; each project
  carries `hostLabel` / `transport` / `unconfigured`. The project name claims the
  top-level weight (600), the host is folded into the row's accessible name so a
  screen reader can tell colliding names apart, and the per-project "+" is now
  gated on `agent !== null && !unconfigured` so it can't spawn into a host that
  isn't in config. `filterTree` moved to its own pure, unit-tested module and
  matches project label, host label, and session id. A configured host with no
  projects no longer renders a row (the empty state reads "No projects yet").
- **Drag to reorder tabs; horizontal-only tab bar** (#86): open session tabs can
  now be dragged to a new position in the tab bar, browser/editor style, instead
  of being fixed in creation order. Reordering is pure frontend — a `reorderTab`
  method on the app-scoped session store moves a tab to the drop target's slot
  (landing after when dragged rightward, before when leftward, so a drop onto a
  neighbour swaps the two), and the order is held in the store array, so it
  persists for the session across React remounts. The dragged tab dims and an
  accent bar marks where it will land; a reset effect clears drag state if the
  dragged tab is closed mid-drag. The tab bar no longer scrolls vertically
  (`overflow-y: hidden`); tabs keep their natural width (`flex: none`) and a thin
  horizontal scrollbar scrolls the row when there are more tabs than fit. Native
  HTML5 drag-and-drop, no new dependency. Keyboard reordering and the full
  WAI-ARIA tab keyboard model are deferred to #111.
- **Config file watcher for live sidebar reload** (#112): the desktop app now
  watches the per-device `config.toml` on disk and auto-refreshes the read-only
  sidebar when it changes, instead of waiting for the manual refresh button or an
  app restart — an external edit (hand-adding a host or project) shows up live.
  The Tauri shell watches the config file's *parent directory* (so editor
  atomic-rename saves are caught, not just inode writes) via `notify` +
  `notify-debouncer-full`, debounces bursty writes over 500ms, and emits a typed
  `ConfigChanged` event — the first Rust→frontend push channel, via
  `tauri-specta` `collect_events`. The event is a no-payload ping: the frontend
  re-reads through the existing `config_get`/discovery-refresh path rather than
  the event carrying a config snapshot. Watcher startup is non-fatal (a failure
  is logged and the app still runs with manual refresh), and the watch attaches
  even on a fresh device whose `remora/` config dir does not exist yet. Scoped to
  the read-only sidebar; the Settings editor stays manual so a disk edit can't
  clobber an in-progress form edit. See
  [ADR-0014](docs/adr/0014-config-file-watcher-and-typed-event-channel.md).
- **Structured per-session status + event channel** (#69, core foundation): the
  session channel now carries typed activity events alongside raw PTY bytes.
  `ChannelOutput` gains `StatusChange(SessionStatus)` (`working` / `idle` /
  `awaiting` / `unknown`) and `PreviewUpdate(String)` on the same ordered stream
  (`PROTOCOL_VERSION` 0 → 1). A core-side detector — a clock-free quiescence
  state machine plus a `vte`-based OSC-7366 marker parser (the Rust port of #55's
  client units) — runs in the PTY bridge: a single detector thread is the sole
  sender to the channel and uses `recv_timeout` as its settle clock, preserving
  byte→status ordering, the `recv()→None` teardown, and no false `idle` under
  backpressure. `awaiting` is marker-only (never inferred); the untrusted marker
  payload is base64-decoded, control- and format-stripped, and length-capped
  before becoming a preview. Recorded in ADR-0013. The desktop consumes these
  events to drive the per-session activity indicator and the client-side
  OSC/quiescence detector is retired — detection now lives solely in core, and
  the UI renders the events it is handed. Shipped as two stacked PRs (core
  foundation, then the desktop migration). Net visible behavior is unchanged:
  working/idle is live; `awaiting` (red) and the preview stay dormant until an
  agent emits the marker (#61).

- **Desktop design system + app redesign**: the desktop frontend now renders
  through a token-driven design system instead of the placeholder stylesheet —
  dark-first with an OS-following light theme (the terminal stays dark in both),
  one marine-blue accent reserved for the active session/tab, focus rings,
  primary actions, and the agent-activity pulse, Inter for chrome and JetBrains
  Mono for machine values. Adds committed CSS tokens (color/type/space/radius/
  elevation/motion), an inline Lucide-geometry icon set, and a TSX component
  library at `apps/desktop/src/ui/` (Button, IconButton, Tag, Badge, Avatar,
  Input, Select, Switch, Checkbox, Dialog, Toast, Tooltip, plus the session hero
  surfaces StatusIndicator/ActivityPulse/SessionRow/SessionTab). The
  shell becomes sidebar → tabs → session bar → first-class xterm terminal → a
  working-status strip, with a Files & diff peek panel (`⌘\\` toggle, empty-state
  shell until a diff backend lands) and a desktop→mobile single-pane fold. The
  signature breathing pulse maps onto the real activity model — working / needs
  you / idle — and the xterm is themed to the design's ANSI palette. Pure
  presentation: no transport, store, or protocol changes, and the terminal stays
  the sole input. Authored from the claude.ai/design "Remora Design System".
- **ssh connection multiplexing** (#63): the direct ssh transport now shares one
  authenticated master across the many short-lived per-session ops (discovery,
  spawn, has-session, attach) via OpenSSH `ControlMaster=auto`,
  `ControlPath=~/.ssh/remora-%C`, and `ControlPersist=60s`. The user
  authenticates once (one hardware-key touch / bastion hop) and subsequent ops
  skip the handshake, helping the "<5s spawn" and live-sidebar goals against
  FIDO keys, jump hosts, and high-latency links. `auto` degrades gracefully to a
  fresh connection if a master socket is stale, so a dead master never wedges. A
  warm authenticated socket lingers up to 60s after the last connection — a
  small, bounded security trade-off. Scoped to direct ssh; the kubectl transport
  is untouched and relay-mode multiplexing is deferred. See
  [ADR-0011](docs/adr/0011-ssh-connection-multiplexing-direct-mode.md).
- **Terminal clipboard** (#87): selecting text and pressing the copy chord —
  Cmd+C on macOS, Ctrl+Shift+C on Linux/Windows — now writes the selection to
  the host system clipboard, and the agent's OSC 52 clipboard-set sequence is
  honored so copy-on-select reaches the OS clipboard. A bare Ctrl-C is left
  untouched and still sends SIGINT. Writes go through the Tauri
  clipboard-manager plugin behind a small `writeClipboard` seam; the capability
  is **write-only** and OSC 52 *read* requests (`?`) are ignored so a remote can
  never exfiltrate the local clipboard. The OSC 52 decoder is fatal on invalid
  UTF-8 and ignores empty payloads so a malformed sequence can't clobber the
  clipboard. Follow-up hardening (opt-in gate for remote-driven writes) tracked
  in #94.
  `namespace`, `context`, or `container` may be a literal string *or* a
  `{ command = "…" }` table whose shell command is resolved **locally** at
  connect time (its trimmed stdout becomes the argv token) — so a sandbox pod
  that gets renamed/recreated is picked up with no config edit, e.g.
  `pod = { command = "kubectl -n sb get pods -l app=dev -o name | head -n1" }`.
  This is the single, opt-in crossing of ADR-0004's "config is never
  shell-evaluated" line: only `{ command }` fields are evaluated, the resolved
  value is re-validated against the literal-field guard (no control chars,
  newlines, leading `-`) before entering the argv, resolution runs locally
  behind the `SessionSource`/`remote.rs` seam (never in the pod), is re-run
  every connect (never persisted), and is bounded by a 10s timeout + 64 KiB
  output cap with a process-group kill so a hung selector can't leak. A
  `{ command }` selector that resolves to more than one value (newline-per-pod
  from `-o name`, or a space-separated jsonpath list) now fails closed with a
  clear `selector matched N values, expected exactly 1` error — naming the count
  and a sample, and suggesting `head -n1` or a tighter selector — instead of the
  opaque "must not contain control characters" rejection (#115). The desktop
  config editor gains a per-field "resolve via command" toggle. See
  [ADR-0009](docs/adr/0009-dynamic-kubectl-field-resolution.md).
- **Per-session workspace mode** (#34): the new-session dialog gains a
  Worktree/Shared picker that overrides the project's default for that one
  session (`SpawnSpec.workspace`). To keep the choice coherent past spawn, a
  session's *effective* mode is now discovered from real sandbox state (a
  surviving git worktree ⇒ worktree) rather than re-derived from project config:
  discovery scans worktrees for every project and stamps `SessionMeta.workspace`,
  and teardown/respawn re-probe the worktree (`test -d`) instead of trusting
  config or discovered metadata. This closes a silent leak where a worktree
  session spawned on a shared-default project was undiscoverable and unremovable
  through the UI. `WorkspaceMode` moved to `remora-protocol`; shared sessions
  show no Respawn affordance (they have no worktree to revive). See
  [ADR-0008](docs/adr/0008-per-session-workspace-override.md).
- **No-agent / plain-shell sessions** (#35): an agent configured with an empty
  command (`command = []`) opens a session that is just a login shell
  (`${SHELL:-/bin/sh} -l`), no agent launched — over both ssh and kubectl. The
  agent form gains a "No command (plain shell)" toggle. Config validation now
  accepts an empty command (still rejecting blank elements in a non-empty
  command). See [ADR-0007](docs/adr/0007-no-agent-plain-shell.md).
- **kubectl exec transport** (core): a second `SessionSource` backend that runs
  sessions in a Kubernetes pod over `kubectl exec`, alongside ssh (stage 12).
  To prove the transport seam isn't ssh-shaped, the transport-neutral logic —
  quoting, the tmux/git command token builders, error classification, and the
  spawn/respawn/list orchestration — was extracted from `ssh.rs` into a shared
  `remote.rs` behind a `RemoteExec` seam; ssh and kubectl are now thin
  connection adapters that compose their argv and delegate to shared
  `capture`/`open_pty` tails (ssh's composed argv stays byte-identical). The
  kubectl adapter builds `kubectl [--context X] [-n NS] exec [-c C] [-i -t]
  <pod> -- sh -c '…'`, joining the same logical tokens ssh feeds its remote
  shell. Per-transport differences are deliberate: no `--request-timeout` (it
  would sever a slow `git worktree add`), an in-container `env TERM=…` wrap (so
  the pod's tmux renders correctly), and a pod preflight probe that fails closed
  with a clear error when the pod lacks `sh`/`tmux`/`git`. The desktop bridge now
  resolves kubectl hosts to a live `KubectlSource`. An ignored `kubectl_e2e`
  suite mirrors `ssh_e2e` for on-cluster verification. Known kubectl limitations
  (no keepalive for idle dead-link detection, per-op connection cost, unbounded
  execution) are tracked as issues #107, #106, and #99. Session teardown
  (`stop`/`remove`, added in #50) is implemented for both transports through the
  shared `remote.rs` orchestration, so kubectl gets it for free.
- Session teardown (desktop): **Stop** a session (kills its tmux, keeps the
  worktree, respawnable) or **Remove** it for good (kills tmux and, in worktree
  mode, removes the worktree + deletes the local branch) — from a sidebar row
  menu or the stopped/disconnected pane. Two new `SessionSource` methods
  (`stop`/`remove`, no default impls) carry teardown through the seam; the ssh
  transport derives paths from config + naming helpers (never `plan_spawn`) and
  makes each step idempotent so a retried remove converges. `remove` refuses a
  worktree with uncommitted work or commits not on any remote (typed
  `WorkspaceDirty { reason }`, probed with `git status --porcelain` +
  `rev-list --not --remotes`, fail-safe to `Transport` on an unreadable probe)
  unless the user force-confirms in a two-stage dialog; the remote branch is
  never deleted. The store drives the tab transition explicitly so a removed
  session never leaves a dead Respawn screen. Closes #33.
- Config management UI (desktop): a Settings modal (⚙ in the sidebar header)
  lets you create, edit, and remove **hosts** (ssh + kubectl), **projects**, and
  **agents** without hand-editing `config.toml`. Forms reuse the new-session
  dialog's shell (focus trap, inline errors); ids are create-only (immutable
  join keys, ADR-0004) and host/agent references are dropdowns of existing
  entries so a dangling reference can't be made. A rejected save (dup/used id,
  validation) shows inline; every successful mutation re-reads the config and
  refreshes the sidebar. When the base file is **semantically invalid**, the
  modal opens in degraded-recovery mode — it lists the problems and the entries
  present so you can delete the offending ones in place (the bridge now reads
  leniently so a recovering delete can run; the core still refuses to persist an
  invalid result). Form-state logic lives in a node-tested `config-editor-model`.
  Final slice of #32 (ADR-0006).
- Config editor channel (desktop bridge): a **local-only** counterpart to the
  redacted display path lets the app manage the per-device config. A new
  un-redacted `EditorConfigDto` (its own module, with a guard test that it ships
  the full connection values and does **not** share a `From<Config>` with the
  display `ConfigDto`) plus `config_get_editable` and nine
  `config_{insert,update,remove}_{host,project,agent}` commands. Mutations run
  the core's `ConfigDocument` (load → mutate → atomic save) behind a serialization
  `Mutex`, reading fresh each time so no update is lost; a rejected edit (dup id,
  missing id, dangling/used reference, save IO) surfaces as a new
  `BridgeError::ConfigEdit` carrying the sanitized reason for inline display,
  distinct from `Config` (whole-file load failure → banner). TS bindings
  regenerated. Second slice of #32 (ADR-0006); the management UI lands in a
  follow-up.
- Config write-back foundation (core): `remora-core` can now create, edit, and
  remove hosts, projects, and agents in the per-device `config.toml` via a new
  `ConfigDocument` (toml_edit) that preserves comments and formatting. Every
  mutation re-validates through the existing load-time checks, so referential
  integrity and validate-before-save come for free; explicit insert/update
  guard against silently overwriting an existing entry; and saves are atomic
  (temp + rename, `0600` on unix, symlink-preserving, never persisting a file
  the app could not reload). A degraded `parse_lenient` mode recovers a
  hand-broken file in place. First slice of #32 (ADR-0006); the bridge editor
  channel and management UI land in follow-ups.
- Config-driven new-session dialog (desktop): the "New session" dialog now
  picks a **project** and **agent** from the per-device config instead of
  free-text fields that could silently mismatch the TOML. The project dropdown
  shows each project's host (so you choose the host by choosing the project),
  and the agent dropdown lists configured agents, defaulting to the project's
  default. A new `agents` field on the config DTO carries the agent ids to the
  frontend (the launch argv stays server-side, test-enforced). The selection
  self-heals if config changes while the dialog is open, and an empty config
  shows a clear "no projects configured" state. Addresses #31; refines roadmap
  stage 9.
- Reconnect & respawn UX (desktop, roadmap stage 11): the app now heals broken
  sessions automatically. On window focus after a sleep/wake cycle,
  `reconnectAll` re-attaches every open tab; mid-session drops (ssh keepalive
  detects them within ~45 s) trigger the same reconnect path without user
  action. Sessions whose tmux died show a "Stopped" status and a one-click
  **Respawn** button — the bridge's new `agent` param carries the original
  agent id (surfaced by pre-stop discovery, D6) so the fresh session runs the
  same agent the user launched. A permanent failure (bad config, auth error,
  missing host) classifies as "Disconnected" and shows the cause instead of
  spinning forever (D5 error classification, exponential backoff with a cap).
  Remote `TERM` is now pinned to `xterm-256color` on every PTY spawn so real
  terminal sessions render colour and box-drawing correctly (closes #26). The
  sidebar's stopped-session rows gain a Respawn action that opens a new tab
  directly.
- Real ssh transport wiring (desktop, roadmap stage 11 precursor): the Tauri
  bridge now drives real ssh sessions instead of the in-process fake. It
  resolves the transport per call from the per-device config — a new
  `SourceResolver` trait + `ConfigResolver` (`bridge/resolve.rs`) map a
  project's host to an `SshSource`, and the bridge looks up
  `project → host → transport` on each `spawn`/`attach`/`respawn`/`list` (no
  command-signature change; `SshSource::new` opens no connection until used, so
  building one per call is free). `list()` aggregates across all configured ssh
  hosts, tolerates one host being unreachable (partial result), and errors only
  when every host fails — carrying the failing host's cause. A `kubectl` host
  surfaces a clear "unsupported (stage 12)" config error. The fake
  `SessionSource` is now test-only; an empty config shows the empty-state
  sidebar. `docs/DEVELOPMENT.md` documents the per-device `config.toml` for
  pointing the app at a real host.
- Sidebar with live session state (desktop, roadmap stage 10): a left sidebar
  renders the Host → Project → Session tree from the config-and-discovery join,
  refreshed live; clicking a live session attaches it as a tab. A new
  `config_get` Tauri command reads the per-device TOML (`remora-core` owns the
  `remora/config.toml` suffix, the shell supplies the OS config dir) and
  projects it through display-only DTOs in `bridge/dto.rs` — connection secrets
  (ssh user/host/port, kube pod/namespace/context) never cross the boundary, and
  a test enforces it. A missing config file is an empty sidebar (a fresh device
  is valid); only a genuinely unreadable file (permissions, parse error)
  surfaces as an error banner. The join itself is a pure, node-tested
  `buildTree`: configured hosts render even when empty, and discovered sessions
  whose project is not in config bucket under a synthetic "Unconfigured" host.
  Live polling lives in a plain `DiscoveryStore` (poll + in-flight guard,
  paused while the window is hidden and refreshed on focus, last-known tree
  retained on a failed poll); a thin `useSyncExternalStore` hook binds it to
  React. Frontend-only logic is node-tested; component rendering is covered by
  manual QA + a tracked e2e TODO. Runs on the in-process fake `SessionSource`.
- Tabs + one-click session spawn (desktop, roadmap stage 9): the app is usable
  for the first time. A tabbed window runs one session per tab, and a free-form
  "New session" dialog spawns sessions against the bridge. State and the
  connection lifecycle live in a plain, node-tested `SessionStore`: tabs are
  deduped by `project/session`, an in-flight-connect guard closes the orphaned
  connection if a tab is closed (or the store torn down) before it resolves, and
  a spawn-first `openSession` reports whether it spawned a fresh session or
  attached an existing one. A thin `useSyncExternalStore` hook binds the store
  to React; every terminal stays mounted (inactive panes hidden via inline
  `display:none`) so scrollback survives tab switches. The dialog owns the
  connecting/error UI (focus trap, Esc/Enter, restore-focus) and the tab bar
  carries `tablist`/`tab` ARIA roles with a visible focus ring. Frontend-only,
  on the in-process fake `SessionSource`.
- Embedded terminal (desktop, roadmap stage 8): xterm.js wired to the Tauri
  bridge. A `SessionConnection` seam (`connection.ts`) buffers PTY output until
  the terminal subscribes, then streams it and delegates write/resize/close to
  the bridge handle; `connectSession` attaches an existing session or spawns a
  new one (attach→spawn→attach), surviving React StrictMode's double-mount and
  page reloads. A framework-agnostic `TerminalController` owns the xterm
  `Terminal` + `FitAddon`: raw bytes go to `term.write` (the emulator owns
  screen state — bytes are never parsed), keystrokes are UTF-8 encoded back, and
  DOM-driven resize is debounced and zero/unchanged-guarded, with a
  `[session closed]` notice on transport death. A thin `<Terminal>` React
  wrapper plus a one-session dev harness make the loop interactive against the
  fake `SessionSource`. Adds `@xterm/xterm` + `@xterm/addon-fit`.
- Tauri bridge (desktop, roadmap stage 7): the `src-tauri` layer now owns a
  `SessionSource` and exposes `session_{list,spawn,attach,respawn,write,resize,
  close}` Tauri commands, streaming PTY output to the React frontend over
  `tauri::ipc::Channel`. Runs on the in-process fake `SessionSource` this stage
  (real ssh/kubectl transports wire in at later roadmap stages). The forward-task lifecycle is
  register-before-spawn with a `oneshot` cancel and a biased `select!`, so
  `close()` is strictly silent; the handle-keyed registry recovers from mutex
  poisoning. Command-arg ids cross the IPC boundary as strings and are
  validated by constructing the protocol newtype (fail-closed on forged ids);
  frontend-facing error/metadata values stay display-only. TypeScript bindings
  are generated from Rust by tauri-specta (`apps/desktop/src/bindings.ts`,
  guarded by a drift test) and wrapped by a typed `bridge.ts` client. The UI
  talks only to this layer (ARCHITECTURE.md "one rule").
- ssh session discovery + respawn: `SshSource::list` discovers sessions on a
  host by joining live tmux sessions (`list-sessions` + per-session
  `show-environment` metadata) with stopped worktrees (`git worktree list`),
  scoped to configured projects. `attach` now runs a `has-session` liveness
  preflight and returns `SessionNotFound` for a missing session instead of
  optimistically attaching. `respawn` re-creates a stopped session's tmux
  session without re-adding its surviving worktree, fail-closed on a vanished
  worktree directory (`test -d` preflight) and attaching to the live session
  if a concurrent respawner won the `new-session` race. New transport-agnostic
  `discovery` module (pure parse + join of untrusted sandbox output, ADR-0004)
  and `naming` inverses (`parse_tmux_session_name`, `parse_worktree_path`).
- ssh transport (attach): `SshSource` opens a PTY channel to an existing
  remote tmux session over ssh (`tmux attach-session -d`), streaming bytes
  both ways and propagating resize, on a reusable `transport::pty_process`
  bridge. `naming::tmux_session_name` centralizes the
  `remora_<project>_<session>` convention. `spawn`/`list` are stubbed for
  later stages. Adds the `portable-pty` dependency.
- Project scaffold: pnpm + Cargo workspaces, Tauri 2 desktop app skeleton,
  `remora-core` / `remora-protocol` crates, CI, and community docs.

### Changed

- **Activity-pulse glow unified on marine** (#180): the pulse halo now uses the
  signature marine accent end to end instead of a legacy lavender. `--glow-pulse`
  (dark + light) and the `remora-pulse-glow` keyframe derive from `--marine-pulse`
  (`#6ea4ff`, deepening to `--marine-500` `#1e6ff5` in light), matching the pulse
  core so the one signature moment reads as a single hue. The old
  `rgba(169, 156, 255, …)` / `rgba(124, 108, 240, …)` values are gone; DESIGN.md
  records marine as canonical and its Do's-and-Don'ts guard against reintroducing
  the lavender. Token-only change; no logic, transport, or component changes.

- **Quieter, more honest sidebar chrome**: dropped the per-session "open in a
  tab" indicator dot and the session-count badges on host and project rows —
  visual noise that didn't carry enough signal. The sidebar now reads as host →
  project → session name plus the activity pulse, with the row actions menu
  revealed on hover. The activity dot now shows **only for connected sessions**
  (the ones open as a tab, where we have a live terminal and actually know the
  status); sessions we aren't attached to show no dot and a muted name, so the
  row no longer implies an "idle" state we can't actually observe. Pure
  presentation: no transport, store, or protocol changes.

- **Faster session discovery** (#108): listing remote sessions now reads each
  session's `REMORA_*` metadata inline via tmux 3.0's `#{E:VAR}` expansion,
  replacing the old one-`show-environment`-per-session fan-out — the bulk of
  discovery latency on a high-RTT link. Discovery runs two `list-sessions`
  passes: a trusted names-only pass (env-free, so unforgeable) decides the live
  set, and an inline-metadata pass enriches each session by name. That collapses
  the old `1 + N` round-trips to two cheap ones over the shared ControlMaster
  (#63).
  Because the live set is decided by the names listing, a forged `REMORA_*` value
  containing a newline can no longer fabricate a phantom session row. Hosts on
  tmux < 3.0 degrade gracefully: the session still lists Live, just without
  agent/created-at metadata.

### Fixed

- **Clicking a session in the sidebar now revives it when the local tab is
  dead but the session is reachable** (#189, inverse of #178). Two dropped-intent
  gaps: the live-attach path short-circuited on `key === activeKey` alone, so
  re-clicking a `stopped`/`disconnected` *active* tab focused an empty pane and
  returned without reconnecting; and "reopen" skipped the respawn for a
  `reconnecting` tab whose retry loop was doomed once discovery reported the
  server session gone. The store now revives any non-live sidebar dedupe — the
  live-attach path re-attaches in place (new `reconnectTab`, guarded so it can't
  orphan an in-flight respawn), the respawn path respawns a `reconnecting` tab —
  and the click's focus-intent gate is liveness-aware. A revive that terminally
  fails now clears the armed focus flag so it can't steal focus onto the next
  tab.
- **Removing a session no longer resurrects it as an orphaned worktree row.**
  Opening a discovered *live* session whose local tab was gone (e.g. after an
  app restart or reconnect — the common persistent-session flow) went through
  the spawn-first path. Because the sidebar open passes no branch, the backend
  planned the *back-compat* convention worktree (`remora/<session_id>` at
  `~/.remora/worktrees/<project>/<session_id>`), which differs from a session
  created with an explicit branch. `git worktree add` therefore *succeeded*,
  creating a second worktree; the following `tmux new-session` failed
  `SessionExists` (the live original held the name) and the just-added worktree
  was left on disk (cleanup only removes it when the session is *absent*). The
  duplicate stayed masked in discovery (same `session_id` as the live original),
  then `remove` deleted only the first-matched worktree and un-masked the orphan
  as a "new" stopped row named `remora/<slug>-<hash>`. Two fixes: the sidebar now
  **attaches** to a live session instead of spawn-first (falling back to respawn
  if it died since the poll), so no duplicate is ever created; and `run_remove`
  now tears down **every** worktree+branch whose branch derives to the target
  `session_id` (not just the first), cleaning up any pre-existing orphan. The
  dirty-gate ignores a back-compat orphan twin's inherited unpushed-commit count
  (base commits it never used), so a phantom twin can't soft-lock removal of an
  otherwise-clean session; the twin's own uncommitted files still block, and
  single-worktree sessions keep the full gate.
- **Discovery no longer drops a host's worktrees after a Kubernetes pod
  restart** (#190). After a pod restart on a persistent volume the tmux server
  dies but its socket file survives, so `tmux list-sessions` fails with `error
  connecting to <sock> (No such file or directory)`. The discovery (`list`)
  classifier only treated `no server running` / `no sessions` as the benign cold
  state, so it mapped the stale socket to a transport error and aborted
  `run_list` before the worktree scan — the host went `available:false` and its
  rows were pruned after the reconnect grace (ADR-0016), reappearing only once a
  new spawn restarted tmux. The list path now shares the attach path's "server
  gone" tolerance through a single `stderr_signals_server_absent` helper: a
  stale-socket failure is the cold state (empty live set), so discovery proceeds
  to surface the surviving stopped/respawnable sessions. The stale-socket arm
  requires both `error connecting to` and the `No such file or directory` detail,
  so a live-but-unreachable socket (`Permission denied`) and genuine ssh/kubectl
  connection failures still surface as transport errors rather than wrongly
  clearing a host that is merely unreachable.
- **Re-clicking the already-active live session in the sidebar now focuses its
  terminal** (#141). Clicking a sidebar session whose local tab was already the
  active, live tab did nothing: `openSession`'s dedupe set `activeKey` to its
  current value, so the `activeKey`-gated focus effect never re-fired — the
  terminal wasn't focused and the `focusOnSelect` intent flag stayed armed,
  later stealing focus into the terminal on the next unrelated status change
  (the same focus-steal class as #133/#136). `openFromSidebar` now mirrors the
  tab bar's path: when the clicked session is already the active tab, it focuses
  the terminal directly and disarms the flag. (The symmetric leak on the respawn
  path is tracked separately in #178.)
- **Sidebar clicks on a stopped-discovered session no longer leak focus on the
  respawn path** (#178). The symmetric twin of #141: when discovery reported a
  session `stopped` (server reaped it) but its local tab was still `live`,
  clicking it routed to `openViaRespawn`, which deduped to the live tab, did no
  respawn, and left `activeKey`/`activeStatus` unchanged — so the focus effect
  never fired to consume the armed `focusOnSelect`, which then stole focus on the
  next unrelated change. `openFromSidebar` now short-circuits the active,
  locally-live re-click (focus the terminal directly and disarm), gating on
  liveness so a genuinely-stopped active tab still respawns, and reads the tab's
  status fresh from the store to close a stale-render window. The remaining
  respawn-dedupe cases route through a new `shouldDisarmAfterSidebarRespawn`
  predicate so a `reconnecting`/vanished dedupe also disarms (the reconnecting
  twin surfaced in the #141 review), while a `stopped`/`disconnected` tab that
  `openViaRespawn` respawns keeps the arm to focus once it goes live. The
  predicate is extracted and unit-tested to close the recurring focus-steal
  class at the root. The inverse revival gaps on the live-attach path are tracked
  in #189.
- **Attach fingerprints the session instead of trusting the tmux name alone**
  (#105). Attach was name-only: `tmux has-session -t remora_<project>_<session>`
  then attach. If something else held a session with that name — a tmux server
  restart with a foreign recreation, or a manually reused name — Remora would
  pipe the client's keystrokes (and the agent-control bytes) straight into an
  unknown process. The name is a hint, not proof (ADR-0004). Attach now uses a
  single `tmux show-environment` round-trip as both the liveness preflight and an
  identity fingerprint: a live session that carries no `REMORA_*` env is a
  same-named impostor and is refused as `SessionNotFound` rather than attached.
  The preflight, fingerprint gate, and error mapping moved into one transport-
  neutral `run_attach` shared by the ssh and kubectl transports (replacing the
  duplicated has-session preflight each carried). Spawn was hardened so the
  fingerprint is reliable: the metadata write now reports success and re-stamps
  once if every write failed, so a transient blip can't leave a live-but-
  permanently-unreconnectable session (the happy path pays no extra round-trip).
  Attach also maps tmux's torn-down-server stderr (`error connecting to <sock>`)
  to `SessionNotFound` via an attach-only classifier, without loosening the
  conservative shared classifier the spawn cleanup gate relies on.
- **A transient host connection drop no longer blanks that host's sessions in
  the sidebar** (#159, ADR-0016). A momentary blip to one host (network hiccup,
  ssh stall) made its session rows vanish for a few seconds, then reappear — the
  whole sidebar reshuffled on a one-off drop. The discovery `list` swallowed a
  per-host error and returned a *successful* shorter list, so the frontend's
  retain-last-good safety net (which only fired when *every* host was down) never
  ran. `Bridge::list` now returns one bucket per attempted host
  (`{ hostId, available, sessions }`) instead of a flat list, resolving the
  long-standing silent-drop `TODO`. The desktop `DiscoveryStore` retains a
  transiently-down host's last-good rows, marks them "reconnecting" (dimmed in
  the sidebar), and prunes only after 15s of continuous unavailability measured
  from the first failed poll — so even a slow-timeout transport keeps a full
  reconnecting window. A reachable host (including one that returns zero
  sessions) stays authoritative, a host removed from config is dropped at once,
  and all-hosts-down still surfaces the existing "discovery unavailable" banner.
- **Open-vs-teardown race that orphaned a session.** Clicking Remove on a session
  and then clicking its row (which respawns) could interleave: the remove killed
  tmux and deleted the worktree + branch while the respawn re-created tmux in that
  same worktree, leaving a live tmux with no worktree or branch behind it. The
  orphan then showed no Stop affordance (discovery classifies a backing-less live
  session as Shared) and was awkward to clear. `SessionStore` had two independent
  in-flight guards — `pending` (open-vs-open) and `teardownPending`
  (teardown-vs-teardown) — with nothing serializing an open against a teardown,
  and `respawnTab` checked neither. Added a per-key `busy()` cross-lock (opens use
  `pending`, respawns a new `respawning` count, teardowns `teardownPending`):
  `openTab`/`stop`/`remove` refuse while busy, and `respawnTab` bails if an open
  or teardown is in flight while still allowing a concurrent respawn (newest-wins
  via the reconnect token). The `respawning` map is a count, not a set, so two
  overlapping respawns keep the key locked until both settle.
- **Remove dialog showed a generic error instead of the real reason.** A failed
  remove surfaced "Could not remove the session." even when the backend returned
  a specific cause, because the dialog only rendered `Error` instances and
  strings — a tauri-specta `BridgeError` is a typed plain object, so its message
  was dropped. A new `removeErrorMessage` helper surfaces the `BridgeError`
  message (falling back to the friendly copy only for a bare `{ok:false}`).
- A kubectl `{ command }` field accidentally wrapped in command substitution —
  `pod = { command = "$(kubectl … | head -n1)" }` — is now rejected at config
  time with a targeted message ("must be the command itself, not wrapped in
  $(...) or backticks") instead of failing cryptically at resolve time with a
  misleading `sh: pod/…: No such file or directory` (closes #127). The field
  holds the command itself, so wrapping the whole thing is double-evaluation:
  `sh -c` substitutes the inner pipeline and then tries to run its output (a pod
  name) as a command. The guard catches both `$(...)` and backtick wraps while
  leaving a legitimate *interior* substitution (e.g. `kubectl -n $(cat ns) …`)
  alone. As a secondary aid, the resolve-time error now includes the command
  that ran so any failure reads as a command problem rather than a cluster one,
  and the host form's command-mode hint documents the bare-pipeline format.
  Follow-up to #84 (command-form resolution) and #121 (clearer multi-match
  errors).
- Opening the New Session dialog from a project **+** now lands focus on the
  session **name** field instead of the project picker, so you can name and
  create a session keyboard-only (closes #125). The project is already implied
  by which **+** was clicked, so the picker no longer needs focus; the global
  **+ New session** entry point is unchanged and still focuses the project
  select first. The on-open focus effect consults a small pure helper
  (`shouldFocusNameField`) that keys off whether the dialog was pre-scoped to a
  project, falling back to the prior first-focusable behavior when it wasn't
  (e.g. the no-projects state). This is the focus half of the per-project **+**
  (#98).
- Creating a session through the New Session dialog now focuses its terminal, so
  you can type the first prompt without clicking into the pane first (closes #78,
  #126). A successful spawn previously restored focus to the **+ New session**
  button; it now routes into the same focus path a tab/sidebar selection uses
  (arming `focusOnSelect` so the activeKey effect focuses the terminal once it's
  live). The spawn path arms that flag only after `openSession` has already
  flipped `activeKey`, so it also bumps a `focusRequest` counter to re-run the
  focus effect and claim the flag — without that nudge the effect never re-fired
  and focus never landed. Attach, cancel, and open-failure keep focus on the +
  button — there's no fresh terminal to type into in those cases.
- Session, project, host, and agent ids now accept uppercase letters and
  canonicalize them to lowercase as you type, instead of rejecting them (closes
  #80). Ids must be lowercase `[a-z0-9-]` slugs (ADR-0004), so a keyboard that
  autocapitalizes the first character (mobile/touch, macOS autocaps) forced a
  manual correction on every creation. The new-session dialog and the
  host/project/agent config forms now run typed id input through a
  `normalizeSlugInput` lowercaser at the field's `onChange`, so `MyApp` becomes
  `myapp` and the input visibly shows the canonical form. Only case is
  canonicalized — other out-of-grammar characters still surface validation
  feedback, and the protocol grammar (`ProjectId`/`SessionId::new`) and tmux
  `remora_<project>_<session>` name format stay strict and unchanged, with the
  Rust bridge remaining the authority.
- Terminal rendering is no longer corrupted when an agent draws box-drawing or
  other multibyte UTF-8 (e.g. claude-code's logo rendered as underscores/garbage
  over a kubectl pod). The session's tmux was running in non-UTF-8 mode: kubectl
  forwards none of the client's environment and our `exec`s run a non-login,
  non-interactive shell that sources no profile, so the pod shell had no locale
  and tmux fell back to mangling the agent's bytes — every attached client then
  saw garbage, while a directly-run agent (no tmux to misparse it) looked fine,
  which is what made it baffling. Both transports now pin a UTF-8 locale on every
  remote command: the kubectl adapter prepends an `export LANG=C.UTF-8
  LC_ALL=C.UTF-8 TERM=xterm-256color;` preamble inside its `sh -c` body (replacing
  the narrower `env TERM=…` wrap, and using `export …;` so it composes with shell
  constructs like the pod preflight's `for` loop), and the ssh adapter prepends an
  `env LANG=C.UTF-8 LC_ALL=C.UTF-8` prefix (ssh's non-interactive `$SHELL -c`
  sources no profile and `SendEnv`/`AcceptEnv` is a fragile default). `C.UTF-8` is
  present without `locale-gen` on glibc and musl and keeps diagnostics in English.
- Mouse-wheel scrolling now drives tmux's scrollback instead of being translated
  into arrow keys, and scrollback is deepened to 50000 lines for long agent
  output (closes #53). The session-creation command sets `mouse on` and a global
  `history-limit 50000` in the same atomic `tmux` invocation as `new-session`
  (history-limit leads, because tmux applies it only to windows created after it
  is set). `remain-on-exit` is ordered ahead of `mouse on` so that a `mouse`
  failure on tmux < 2.1 (where the option didn't exist) can't abort the chain and
  strip the load-bearing #28 self-destruct guard. Mouse mode applies to
  newly-created sessions only.
- Agent launch arguments that use a Unicode dash (e.g.
  `—dangerously-skip-permissions`, where autocorrect or a "prettifying" paste
  turned `--` into an em-dash) are now rejected at config time instead of
  silently misbehaving at runtime. An agent CLI only recognizes ASCII
  hyphen-minus, so a leading Unicode dash is parsed as a positional prompt
  rather than a flag — the flag text ends up typed into the agent's prompt box.
  Config validation in `remora-core` now flags any argv element whose first
  non-whitespace character is a Unicode `Dash_Punctuation` code point (ASCII `-`
  excluded), and the desktop agent form mirrors the same guard so the mistake is
  caught before save. The launch path itself was already correct (the flag was
  passed verbatim); this closes the silent-acceptance gap that made the failure
  baffling.
- The stopped-session screen now names the cause (closes #28). When a tab lands
  on the in-tab Respawn screen — e.g. an agent that exited immediately and whose
  tmux session is then gone — the overlay reads "Session stopped: claude:
  command not found" instead of a bare "Session stopped." The connection keeps a
  small rolling tail of the PTY output it received; on the stopped transition the
  dead connection's last meaningful line (escape sequences stripped) becomes the
  cause. Frontend-only, so the core transport stays agent-opaque; when no usable
  output was seen the overlay falls back to the bare message. Completes part (b)
  of the atomic-`remain-on-exit` fix below.
- A fast-exiting agent now surfaces its error instead of a bare "Stopped"
  (closes #28). `remain-on-exit` is applied **atomically** with `tmux
  new-session`, in the same tmux invocation via tmux's own argv `;` command
  separator (shell-quoted so the remote login shell passes it through), rather
  than as a separate follow-up `set-option`. Previously an agent that exited
  immediately — a bad flag, an auth error, or the binary missing from the
  non-login PATH — could die in the window between `new-session` returning and
  the option landing, destroying the session before its pane (and the real
  error, e.g. `claude: command not found`) could be retained; the follow-up
  attach then hit "no sessions" and the failure surfaced as an opaque "Session
  Stopped." With the option set atomically, the dead pane is retained, the
  session lists as live, and the attach shows the agent's actual error.
- Quitting the agent drops to a usable shell instead of a dead pane (closes
  #30). The agent is no longer the tmux pane's top-level process: it is wrapped
  so a clean or user-interrupted exit (status 0, 130 SIGINT/Ctrl-C, 143
  SIGTERM) execs an interactive login shell in the same worktree — the pane
  stays alive with a real prompt and the full login PATH, so the agent is
  re-runnable. Any other non-zero exit (a real crash, bad flag, or
  `command not found`) still propagates so `remain-on-exit` keeps the dead pane
  and its error inspectable (preserving #28).
- The sidebar Settings control is now a properly sized icon-button instead of a
  bare gear glyph (closes #77). It previously borrowed the `.sidebar-refresh`
  text-button style of the adjacent Refresh button, so it rendered tiny and was
  hard to tell apart from Refresh. It now has a dedicated `.sidebar-settings`
  class — a 28×28 bordered, rounded hit area with a larger gear and a hover
  state, wired into the existing focus-visible outline. Visual-only; the button
  kept its `aria-label`/`title="Settings"`, so accessibility is unchanged.

### Security

- **Bound combining marks in the activity-preview sanitizer** (#197): a sandbox
  payload could stack Unicode combining marks (Zalgo) on a base glyph to garble
  the preview tooltip — the marks pass `char::is_control()` and survived the
  earlier control/bidi/zero-width scrub (#193). The sanitizer now caps stacking
  marks (Mn/Me) to four per grapheme cluster and drops orphan marks with no
  base. Bounding per grapheme cluster (not a per-char run) means interleaved
  spacing marks or other extenders can't reset the budget to smuggle an
  unbounded stack back in, while legitimate decomposed accents (e.g. NFD `é`) and
  3–4-mark scripts like Biblical Hebrew stay intact.
