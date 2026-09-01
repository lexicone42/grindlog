#!/usr/bin/env bash
# Generates a synthetic "speedrun session" video for end-to-end testing of
# the OCR + run-detection pipeline, using only ffmpeg.
#
# Timeline (109s total, 640x360, timer top-left):
#    0-5s    timer at 0:00:00.000            (idle)
#    5-55s   timer counting up 0 -> 50s      (run 1)
#   55-63s   frozen at 0:00:50.000           (run 1 finishes, t=50s)
#   63-67s   back to zero                    (idle)
#   67-102s  counting up 0 -> 35s            (run 2)
#  102-109s  back to zero                    (run 2 resets)
#
# Expected detection: Started, Finished(50.0s), Started, Reset(zeroed).
#
# Test config for this video:
#   [stream]
#   channel = "test"
#   source = "file"
#   input = "test-run.mp4"
#   canvas_w = 640
#   canvas_h = 360
#   [timer]
#   crop_x = 30
#   crop_y = 30
#   crop_w = 460
#   crop_h = 100
set -euo pipefail

out="${1:-test-run.mp4}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

bg="color=c=0x202020:s=640x360:r=30"
style="font=monospace:fontsize=64:fontcolor=white:x=40:y=40"

seg() { # text duration outfile
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i "$bg" -t "$2" \
    -vf "drawtext=$style:text='$1'" -pix_fmt yuv420p "$tmp/$3"
}

seg '0\:00\:00.000'  5 a.mp4
seg '%{pts\:hms}'   50 b.mp4   # counts 0 -> 50s
seg '0\:00\:50.000'  8 c.mp4   # frozen: the "final time"
seg '0\:00\:00.000'  4 d.mp4
seg '%{pts\:hms}'   35 e.mp4   # counts 0 -> 35s
seg '0\:00\:00.000'  7 f.mp4

for f in a b c d e f; do echo "file '$tmp/$f.mp4'"; done > "$tmp/list.txt"
ffmpeg -hide_banner -loglevel error -y -f concat -safe 0 -i "$tmp/list.txt" -c copy "$out"
echo "wrote $out"
