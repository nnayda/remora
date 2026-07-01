# Agent activity hooks

Remora reads agent activity from an in-band OSC-7366 marker the agent's own
lifecycle hooks emit (ADR-0010, ADR-0013). Nothing extra runs on the sandbox —
a hook just `printf`s a string Remora defined.

## Claude Code: zero-setup via the desktop template

Creating a Claude Code agent from the desktop's **"Claude Code (activity
markers)"** template (new-agent form, #196/ADR-0020) is the primary path —
markers work with no manual sandbox setup:

1. In the new-agent form, click **Claude Code (activity markers)**. It fills
   `command` with `claude --settings '{…}'` (composed onto any base command you
   already typed — it won't clobber flags like `--continue`) and sets
   `provision` to the notify script at `~/.remora/hooks/claude-notify.sh`,
   mode `0o755`.
2. Save the agent. On every spawn, Remora writes the provisioned script to the
   sandbox (base64-decoded, non-fatal `StepId::Provision` batch step) *before*
   `tmux new-session`, then launches `claude` with the inline `--settings`
   flag whose **Notification** hook runs `$HOME/.remora/hooks/claude-notify.sh`.
   Claude Code's Notification event is its "needs your attention" signal, so
   Remora maps it to `awaiting_input` and carries the notification text as the
   preview.

Both fields are plain, editable `Agent` config (`command` / `provision`) — you
can hand-edit them in TOML the same way, or clear/replace them after applying
the template.

**Caveat (D6, ADR-0020):** Remora launches the agent with
`claude --settings '{…}'` carrying a Notification hook. `--settings` layers on
top of the user's `~/.claude/settings.json` (model/permissions/MCP survive),
but whether a `--settings` `hooks` object shadows a user's OWN Notification
hook is undocumented upstream — verified empirically in the hermes dogfood.
Low risk in a coding sandbox, but if you have your own Notification hook in
`~/.claude/settings.json`, don't assume both run.

## Fallback: manual install

If you're not using Claude Code, are using a version of the template that
doesn't fit your setup, or want to install the recipe by hand instead of
through `provision`, install it directly:

1. Copy `contrib/agent-hooks/claude-code/remora-notify.sh` into the sandbox and
   make it executable (`chmod +x`). It needs `jq` and `base64` on PATH. This
   file is the single source of truth — the desktop template embeds a copy
   pinned to it by a drift-guard test.
2. Add to the sandbox's `~/.claude/settings.json`:

   ```json
   {
     "hooks": {
       "Notification": [
         { "hooks": [ { "type": "command", "command": "/path/to/remora-notify.sh" } ] }
       ]
     }
   }
   ```

## Why the recipe looks the way it does

- **tmux passthrough wrapping is mandatory.** Remora spawns the agent in its own
  tmux session with `allow-passthrough` on. tmux *consumes* a bare OSC; only the
  passthrough-wrapped form (`ESC P tmux ; ESC ESC ] … BEL ESC \`) is forwarded to
  clients (and tmux strips the envelope on the way out). See ADR-0010.
- **Write to `/dev/tty`, not stdout.** Claude Code captures a hook's stdout, so a
  `printf` to stdout never reaches the PTY byte stream Remora reads. The recipe
  writes to `/dev/tty` (override with `REMORA_MARKER_OUT` for local testing).
- **One window / one pane.** Markers from a background tmux window are dropped by
  tmux. Remora's current one-session = one-window = one-pane topology means every
  byte the reader sees is from the foreground pane (ADR-0010/0013).

## Known limitations (tracked as follow-ups)

- The Notification event's text is often generic ("Claude needs your permission
  to use Bash") rather than the literal question, and it also fires for the idle
  reminder. Richer/structured prompt text is future work.
- Markers are now installed automatically for Claude Code agents created from
  the template (no more silent "did the hook even get configured" gap for the
  common case). A "did a marker ever arrive" diagnostic — for hand-configured
  agents, or a provisioned script that fails to land (best-effort write) — is
  still a follow-up; its trigger signal needs its own design (see ADR-0020).

## Security

The marker payload is **untrusted** — anything in the sandbox (the agent, a
dependency, a build/test subprocess) can emit it. Remora core strips control/
format characters and length-caps the text, and the UI renders it as
*sandbox-claimed* ("the session says: …"), never as an authoritative message.
Trusted facts (which host/session) come from Remora's own state, never the
payload.

`provision` itself is a generic, opaque-bytes-to-a-path capability — core never
parses or interprets the content. Remora does not restrict the destination
path beyond basic validation (no `..` traversal, no empty path); the property
that Remora only ever writes a Remora-owned path is a guarantee of the shipped
template, not an enforced config constraint (the same trust boundary `command`
already crosses).
