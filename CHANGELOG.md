# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

- **Faster session discovery** (#108): listing remote sessions now reads each
  session's `REMORA_*` metadata inline via tmux 3.0's `#{E:VAR}` expansion in a
  single `list-sessions`, replacing the old one-`show-environment`-per-session
  fan-out — the bulk of discovery latency on a high-RTT link. The live set comes
  from a trusted names-only `list-sessions` (env-free, so unforgeable) and the
  inline metadata enriches each session by name, so discovery's `1 + N`
  round-trips collapse to two cheap ones over the shared ControlMaster (#63).
  Because the live set is decided by the names listing, a forged `REMORA_*` value
  containing a newline can no longer fabricate a phantom session row. Hosts on
  tmux < 3.0 degrade gracefully: the session still lists Live, just without
  agent/created-at metadata.

### Fixed

- Creating a session through the New Session dialog now focuses its terminal, so
  you can type the first prompt without clicking into the pane first (closes
  #78). A successful spawn previously restored focus to the **+ New session**
  button; it now routes into the same focus path a tab/sidebar selection uses
  (arming `focusOnSelect` so the activeKey effect focuses the terminal once it's
  live). Attach, cancel, and open-failure keep focus on the + button — there's no
  fresh terminal to type into in those cases.
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
