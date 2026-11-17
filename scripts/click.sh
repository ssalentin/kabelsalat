#!/usr/bin/env bash
# Left-click inside one kabelsalat window (AGENTS.md rule 24: focus-guarded).
# X Y are window-relative logical pixels, which equal egui points while the
# UI scale is the monitor's. Requires wlrctl (Wayland virtual pointer).
# Usage: scripts/click.sh <pid> <x> <y>
set -euo pipefail
PID=${1:?usage: click.sh <pid> <x> <y>}
X=${2:?}
Y=${3:?}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
export WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-1}
if [ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]; then
  HYPRLAND_INSTANCE_SIGNATURE=$(ls "$XDG_RUNTIME_DIR/hypr/" | head -1)
  export HYPRLAND_INSTANCE_SIGNATURE
fi

active=$(hyprctl activewindow -j | python3 -c 'import json,sys; print(json.load(sys.stdin).get("pid",""))')
if [ "$active" != "$PID" ]; then
  echo "click.sh: focus is on pid $active, not $PID; not clicking" >&2
  exit 1
fi
read -r AX AY < <(hyprctl activewindow -j | python3 -c 'import json,sys; print(*json.load(sys.stdin)["at"])')
read -r CX CY < <(hyprctl cursorpos | tr -d ,)
wlrctl pointer move $((AX + X - CX)) $((AY + Y - CY))
read -r NX NY < <(hyprctl cursorpos | tr -d ,)
if [ "$NX" != $((AX + X)) ] || [ "$NY" != $((AY + Y)) ]; then
  echo "click.sh: cursor at $NX,$NY, wanted $((AX + X)),$((AY + Y)); not clicking" >&2
  exit 1
fi
wlrctl pointer click left
