#!/usr/bin/env bash
# End-to-end dictation check.
#
# Drives the full pipeline the way a user does - into a real focused window -
# and asserts the text arrives. Uses a recorded WAV rather than the microphone
# so it is reproducible; microphone capture is covered separately by
# `freeflow devices` and the overlay's level meter.
#
# Usage: scripts/e2e.sh path/to/speech.wav [path/to/freeflow]
set -uo pipefail

WAV="${1:?usage: e2e.sh <speech.wav> [freeflow-binary]}"
FREEFLOW="${2:-freeflow}"
OUT=$(mktemp /tmp/freeflow-e2e-XXXXXX.txt)
TITLE="freeflow-e2e-$$"

command -v hyprctl >/dev/null || { echo "hyprctl not found; this check needs Hyprland"; exit 1; }
command -v foot    >/dev/null || { echo "foot not found; used as the target window"; exit 1; }

"$FREEFLOW" status >/dev/null || { echo "freeflowd is not running"; exit 1; }

original=$(hyprctl activewindow -j | python3 -c 'import sys,json;print(json.load(sys.stdin).get("address",""))')
restore() {
  [ -n "$original" ] && hyprctl dispatch focuswindow "address:$original" >/dev/null 2>&1
}
trap restore EXIT

foot --title "$TITLE" -e sh -c "cat > $OUT" &
foot_pid=$!
sleep 1.5
hyprctl dispatch focuswindow "title:$TITLE" >/dev/null
sleep 0.4

"$FREEFLOW" bench "$WAV" --inject
sleep 0.5
# `cat` line-buffers; flush before tearing the window down.
wtype -k Return
sleep 0.5
kill "$foot_pid" 2>/dev/null
sleep 0.3

typed=$(cat "$OUT" 2>/dev/null)
rm -f "$OUT"

if [ -z "$typed" ]; then
  echo "FAIL: nothing was typed into the target window"
  exit 1
fi
echo "typed: $typed"
echo "PASS"
