#!/usr/bin/env bash
# Remora activity marker (OSC-7366, ADR-0010). Claude Code "Notification" hook:
# emit awaiting_input + the agent's message as a preview so Remora can show what
# the agent is waiting for.
#
# Two non-obvious, load-bearing requirements:
#   1. The marker MUST be tmux-passthrough WRAPPED. Remora runs the agent in its
#      own tmux session with allow-passthrough; tmux silently consumes a bare OSC.
#   2. It MUST go to the terminal, not stdout. Claude Code captures a hook's
#      stdout (shown only in Ctrl-R), so stdout never reaches the PTY byte stream
#      Remora reads. Default to /dev/tty; REMORA_MARKER_OUT overrides (tests).
#
# The payload is UNTRUSTED by design; Remora core sanitizes + length-caps it.
# This printf MUST stay byte-for-byte in sync with the wire contract asserted by
# remora_notify_recipe_round_trip in crates/remora-core/src/activity/marker.rs.
set -euo pipefail

out="${REMORA_MARKER_OUT:-/dev/tty}"
msg="$(jq -r '.message // empty' 2>/dev/null || true)"
[ -n "$msg" ] || exit 0

enc="$(printf '%s' "$msg" | base64 | tr -d '\n')"
state="YXdhaXRpbmdfaW5wdXQ="   # base64("awaiting_input")

# on-wire (tmux passthrough envelope, inner ESC doubled):
#   ESC P tmux ; ESC ESC ] 7366 ; remora ; 1 ; state ; <state> ; <msg> BEL ESC \
printf '\033Ptmux;\033\033]7366;remora;1;state;%s;%s\007\033\\' "$state" "$enc" > "$out"
