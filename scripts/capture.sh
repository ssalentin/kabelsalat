#!/usr/bin/env bash
# Capture kabelsalat ui-demo screenshots (front/back/playing/help/transport)
# without disturbing a running real app instance. Lead-side verification harness
# (AGENTS.md, "Working on kabelsalat"). Requires: grim >= 1.5 (-T), Hyprland, python3, ImageMagick for analysis.
# Usage: scripts/capture.sh <outdir> [binary]
set -euo pipefail
OUT=${1:?usage: capture.sh <outdir> [binary]}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${2:-$HERE/../target/debug/kabelsalat}
mkdir -p "$OUT"
export WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-1} DISPLAY=${DISPLAY:-:0} XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/1000}
export HYPRLAND_INSTANCE_SIGNATURE=$(ls "$XDG_RUNTIME_DIR/hypr/" | head -1)

"$BIN" ui-demo >"$OUT/demo.log" 2>&1 &
DEMO=$!
trap 'kill $DEMO 2>/dev/null || true' EXIT
sleep 6
if ! kill -0 "$DEMO" 2>/dev/null; then echo "demo died:"; cat "$OUT/demo.log"; exit 1; fi

shot() { "$HERE/shot.sh" "$OUT/$1.png" "$DEMO"; }
key() {
  local active
  active=$(hyprctl activewindow -j | python3 -c 'import json,sys; print(json.load(sys.stdin).get("pid",""))')
  if [ "$active" != "$DEMO" ]; then echo "focus is not on the demo (pid $active), not sending keys"; exit 1; fi
  python3 "$HERE/xtest_key.py" "$@"
}

shot front
key 0xff09            # Tab -> back
sleep 2
shot back
key 0x20              # Space -> playing
sleep 1
shot playing
key 0x3f --shift      # ? -> help
sleep 1
shot help
key 0xff1b            # Esc -> close help
sleep 0.5
# Transport window removed (2026-11-15): BPM lives on the metronome face.
# The old F10 transport shot is dropped; front/back/playing/help remain.

echo "--- demo log ---"; cat "$OUT/demo.log"
echo "captured: $(ls "$OUT"/*.png | wc -l) frames in $OUT"
