#!/usr/bin/env bash
# Screenshot of one kabelsalat window, never of the screen (AGENTS.md rule 108).
# `grim -T` copies the window's own buffer, so other windows, notifications and
# bars cannot end up in the image, even when they lie on top of it.
# Usage: scripts/shot.sh <out.png> [pid]   (pid: required when several run)
set -euo pipefail
OUT=${1:?usage: shot.sh <out.png> [pid]}
PID=${2:-}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
export WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-1}
if [ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]; then
  HYPRLAND_INSTANCE_SIGNATURE=$(ls "$XDG_RUNTIME_DIR/hypr/" | head -1)
  export HYPRLAND_INSTANCE_SIGNATURE
fi

ID=$(hyprctl clients -j | python3 -c '
import json, sys
pid = sys.argv[1]
wins = [c for c in json.load(sys.stdin)
        if c["class"] == "kabelsalat" and (not pid or str(c["pid"]) == pid)]
if len(wins) != 1:
    sys.exit(f"shot.sh: need exactly one kabelsalat window (pid={pid or "any"}), found {len(wins)}")
print(wins[0]["stableId"])
' "$PID")

grim -T "$ID" "$OUT"
echo "shot.sh: $OUT ($(magick identify -format '%wx%h' "$OUT" 2>/dev/null || echo '?'))"
