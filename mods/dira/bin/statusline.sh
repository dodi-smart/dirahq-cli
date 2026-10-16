#!/bin/sh
# Claude Code statusLine command for the dira mod. The mod renders the line
# (24-bit, dira palette) into a per-session cache file; this only prints it,
# so the status line costs one `cat`. Reads Claude Code's JSON on stdin.
# The cache directories and the bare mark's colours mirror writeStatusCache
# (hooks/register.tsx) and statusAnsi (hooks/lib.ts); change them together.
input=$(cat)
sid=$(printf '%s' "$input" | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
if [ -n "$sid" ]; then
  for d in "${XDG_CACHE_HOME:-$HOME/.cache}/dira/mod" "/tmp/claude-$(id -u)/dira-mod"; do
    if [ -f "$d/status-$sid.ansi" ]; then
      cat "$d/status-$sid.ansi"
      exit 0
    fi
  done
fi
printf '\033[38;2;144;121;255m◆\033[0m \033[38;2;139;135;153mdira\033[0m'
