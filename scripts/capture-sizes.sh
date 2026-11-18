#!/usr/bin/env bash
# Capture the ui-demo at reduced window sizes, every main view per size.
# The demo window is floated and resized only while it is the active window
# (AGENTS.md rules 24, 55); shots use shot.sh (rule 108).
# Usage: scripts/capture-sizes.sh <outdir> [binary] [WxH ...]
#   sizes are logical (compositor) pixels; default: 1600x1000 1280x800 781x950 1576x468 781x468
set -euo pipefail
OUT=${1:?usage: capture-sizes.sh <outdir> [binary] [WxH ...]}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${2:-$HERE/../target/debug/kabelsalat}
shift $(( $# >= 2 ? 2 : 1 ))
SIZES=("$@")
[ ${#SIZES[@]} -gt 0 ] || SIZES=(1600x1000 1280x800 781x950 1576x468 781x468)
mkdir -p "$OUT"
export WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-1} XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/1000}
export HYPRLAND_INSTANCE_SIGNATURE=$(ls "$XDG_RUNTIME_DIR/hypr/" | head -1)

DEMO=
trap '[ -n "$DEMO" ] && kill $DEMO 2>/dev/null || true' EXIT

ours() {
  local active
  active=$(hyprctl activewindow -j | python3 -c 'import json,sys; print(json.load(sys.stdin).get("pid",""))')
  if [ "$active" != "$DEMO" ]; then echo "focus is not on the demo (pid $active), stopping"; exit 1; fi
}
key() { ours; wtype -k "$1"; sleep 0.8; }
shot() { "$HERE/shot.sh" "$OUT/$1.png" "$DEMO" >/dev/null; }

for size in "${SIZES[@]}"; do
  W=${size%x*}; H=${size#*x}
  "$BIN" ui-demo >"$OUT/demo-$size.log" 2>&1 &
  DEMO=$!
  for _ in $(seq 40); do
    sleep 0.25
    a=$(hyprctl activewindow -j | python3 -c 'import json,sys; print(json.load(sys.stdin).get("pid",""))')
    [ "$a" = "$DEMO" ] && break
  done
  sleep 1
  got=
  for _ in $(seq 10); do
    ours
    hyprctl dispatch 'hl.dsp.window.float({ action = "enable" })' >/dev/null
    ours
    hyprctl dispatch "hl.dsp.window.resize({ x = $W, y = $H })" >/dev/null
    ours
    hyprctl dispatch 'hl.dsp.window.center()' >/dev/null
    sleep 0.6
    got=$(hyprctl clients -j | python3 -c 'import json,sys; print(*["%dx%d" % tuple(c["size"]) for c in json.load(sys.stdin) if str(c["pid"])==sys.argv[1]])' "$DEMO")
    [ "$got" = "$size" ] && break
  done
  if [ "$got" != "$size" ]; then echo "window is $got, not $size, stopping"; exit 1; fi
  sleep 2
  p="$size"
  shot "$p-rack"
  key Tab; sleep 1; shot "$p-back"; key Tab
  key m; shot "$p-mixer"; key m
  key n; shot "$p-matrix"; key n
  key b; shot "$p-plugins"; key b
  key comma; shot "$p-settings"; key comma
  key v; shot "$p-keyboard"; key v
  key F12; shot "$p-assistant"; key F12
  kill $DEMO; wait $DEMO 2>/dev/null || true; DEMO=
  echo "captured $size: $(ls "$OUT"/"$p"-*.png | wc -l) frames"
done
