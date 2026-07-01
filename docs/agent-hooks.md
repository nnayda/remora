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

## Claude Code: confirm the hook install is actually working

The Notification hook above is silent until the agent asks you something, so
there's nothing to prove a fresh install is wired correctly before that first
question. Install the liveness recipe and point it at **both**
`SessionStart` and `UserPromptSubmit` — these fire early and often, so a
correct install confirms itself without the agent having to ask anything.

1. Copy `contrib/agent-hooks/claude-code/remora-ping.sh` into the sandbox and
   make it executable (`chmod +x`). It has no dependencies (a plain `printf`,
   no `jq`/`base64`).
2. Add the `SessionStart` and `UserPromptSubmit` entries to the sandbox's
   `~/.claude/settings.json` **alongside** the `Notification` entry from the
   section above — they live under the same `hooks` object, so keep all three
   rather than replacing it (dropping `Notification` would regress the preview):

   ```json
   {
     "hooks": {
       "Notification": [
         { "hooks": [ { "type": "command", "command": "/path/to/remora-notify.sh" } ] }
       ],
       "SessionStart": [
         { "hooks": [ { "type": "command", "command": "/path/to/remora-ping.sh" } ] }
       ],
       "UserPromptSubmit": [
         { "hooks": [ { "type": "command", "command": "/path/to/remora-ping.sh" } ] }
       ]
     }
   }
   ```

Once Remora sees any marker on this session — a ping or a Notification — the
sidebar row's hover tooltip shows **"Activity hook active."** `SessionStart`
proves a fresh session's hook before the agent asks anything; `UserPromptSubmit`
re-earns the affirmation on the next interaction after a reconnect (`SessionStart`
fires on Claude *process* start, not on re-attaching a terminal, so it doesn't
fire again just because you reconnected).

**`/dev/tty`, never stdout, is doubly important here.** As with
`remora-notify.sh`, Claude Code captures a hook's stdout so a stdout `printf`
never reaches the PTY byte stream Remora reads — but for `SessionStart` and
`UserPromptSubmit` specifically, stdout is also **injected directly into the
model's context.** Printing raw OSC escape bytes there on every prompt would
corrupt the agent's own context window, not just fail to reach Remora. The
recipe writes only to `/dev/tty` (override with `REMORA_MARKER_OUT` for local
testing) and fails silently if no controlling terminal is present (some
`kubectl exec`/`ssh` paths lack one), exactly like `remora-notify.sh`.

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
- The sidebar row's tooltip shows a positive **"Activity hook active"**
  affirmation once Remora has seen any marker (a ping or a Notification) on
  that session's current attach — see ADR-0019. This is per-attach: it clears
  on detach and is re-earned by the next marker after a reconnect. Absence is
  *not* framed as broken — a healthy, freshly reconnected session that hasn't
  said anything yet also shows no affirmation, so silence never means "hook is
  misconfigured," only "not confirmed yet this attach."

## Security

The marker payload is **untrusted** — anything in the sandbox (the agent, a
dependency, a build/test subprocess) can emit it. Remora core strips control/
format characters and length-caps the text, and the UI renders it as
*sandbox-claimed* ("the session says: …"), never as an authoritative message.
Trusted facts (which host/session) come from Remora's own state, never the
payload.
