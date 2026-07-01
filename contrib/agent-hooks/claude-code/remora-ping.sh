#!/usr/bin/env bash
# Remora liveness ping (OSC-7366 `ping`, ADR-0010/0019). Wire it to Claude Code's
# SessionStart AND UserPromptSubmit hooks: it emits a payload-free marker so
# Remora can confirm the activity-hook pipeline is wired even before the agent
# asks anything. It asserts NO activity state — its only job is "a marker arrived".
#
# Two non-obvious, load-bearing requirements (same as remora-notify.sh):
#   1. The marker MUST be tmux-passthrough WRAPPED. Remora runs the agent in its
#      own tmux session with allow-passthrough; tmux silently consumes a bare OSC.
#   2. It MUST go to the terminal, not stdout. Claude Code captures a hook's
#      stdout, AND injects SessionStart/UserPromptSubmit stdout into the model's
#      context — so a stdout printf both fails to reach Remora and corrupts the
#      agent's context. Default to /dev/tty; REMORA_MARKER_OUT overrides (tests).
#
# This printf MUST stay byte-for-byte in sync with the wire contract asserted by
# remora_ping_recipe_round_trip in crates/remora-core/src/activity/marker.rs.
set -euo pipefail

out="${REMORA_MARKER_OUT:-/dev/tty}"

# on-wire (tmux passthrough envelope, inner ESC doubled):
#   ESC P tmux ; ESC ESC ] 7366 ; remora ; 1 ; ping BEL ESC \
printf '\033Ptmux;\033\033]7366;remora;1;ping\007\033\\' > "$out" 2>/dev/null || exit 0
