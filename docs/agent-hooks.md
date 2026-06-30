# Agent activity hooks

Remora reads agent activity from an in-band OSC-7366 marker the agent's own
lifecycle hooks emit (ADR-0010, ADR-0013). Nothing extra runs on the sandbox —
a hook just `printf`s a string Remora defined.

## Claude Code: surface "what is the agent waiting for?"

Install the recipe and point a **Notification** hook at it. Claude Code's
Notification event is its "needs your attention" signal, so Remora maps it to
`awaiting_input` and carries the notification text as the preview.

1. Copy `contrib/agent-hooks/claude-code/remora-notify.sh` into the sandbox and
   make it executable (`chmod +x`). It needs `jq` and `base64` on PATH.
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
- A misconfigured or missing hook is a silent no-op (Remora shows no preview;
  activity still works via quiescence). A "did a marker ever arrive" diagnostic
  is a follow-up.

## Security

The marker payload is **untrusted** — anything in the sandbox (the agent, a
dependency, a build/test subprocess) can emit it. Remora core strips control/
format characters and length-caps the text, and the UI renders it as
*sandbox-claimed* ("the session says: …"), never as an authoritative message.
Trusted facts (which host/session) come from Remora's own state, never the
payload.
