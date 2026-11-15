#!/usr/bin/env bash
# Capture kabelsalat ui-demo screenshots (front/back/playing/help/transport)
# without disturbing a running real app instance. Lead-side verification harness
# (PLAN.md §0.1). Requires: grim, Hyprland, python3, ImageMagick for analysis.
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

grim -o eDP-1 "$OUT/front.png"
python3 "$HERE/xtest_key.py" 0xff09            # Tab -> back
sleep 2
grim -o eDP-1 "$OUT/back.png"
python3 "$HERE/xtest_key.py" 0x20              # Space -> playing
sleep 1
grim -o eDP-1 "$OUT/playing.png"
python3 "$HERE/xtest_key.py" 0x3f --shift      # ? -> help
sleep 1
grim -o eDP-1 "$OUT/help.png"
python3 "$HERE/xtest_key.py" 0xff1b            # Esc -> close help
sleep 0.5
python3 "$HERE/xtest_key.py" 0xffc7            # F10 -> transport window
sleep 1
grim -o eDP-1 "$OUT/transport.png"

echo "--- demo log ---"; cat "$OUT/demo.log"
echo "captured: $(ls "$OUT"/*.png | wc -l) frames in $OUT"
