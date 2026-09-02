#!/usr/bin/env bash
# Process Twitch VODs chronologically into the full-history rebuild database
# (ninja-gaiden-full.db). Streams each VOD directly — no downloads.
#
#   ./scripts/backfill-vods.sh <vod_id> [vod_id...]
#
# Broadcast start times are fetched from Twitch automatically, so runs land
# on the original timeline. Run under `nice` so the live bot keeps priority.
set -uo pipefail
cd "$(dirname "$0")/.."
mkdir -p backfill-logs

for id in "$@"; do
  cfg=$(mktemp /tmp/ngbackfill-XXXXXX.toml)
  cat > "$cfg" <<EOF
[stream]
channel = "arcus"
source = "vod"
vod_id = "$id"
# 480p30: timer, splits AND the attempt counter all read cleanly (verified
# against 1080p on the same footage) at ~10x less decode work than 1080p60.
quality = "480p30"

[timer]
crop_x = 285
crop_y = 800
crop_w = 390
crop_h = 100
threshold = 60

[splits]
enabled = true
crop_x = 535
crop_y = 498
crop_w = 135
crop_h = 288

[game]
name = "Ninja Gaiden (NES)"
category = "Any%"
acts = [
  { name = "Act 1", end_ms = 58000 },
  { name = "Act 2", end_ms = 175000 },
  { name = "Act 3", end_ms = 262000 },
  { name = "Act 4", end_ms = 425000 },
  { name = "Act 5", end_ms = 585000 },
  { name = "Act 6" },
]

[attempts_counter]
enabled = true
crop_x = 552
crop_y = 462
crop_w = 120
crop_h = 34

[database]
path = "ninja-gaiden-full.db"

[debug]
obs_log = "backfill-logs/obs-$id.jsonl"
EOF
  echo "=== VOD $id — $(date -Is) ==="
  if ! nice -n 15 ./target/release/ngtwitchtimer --config "$cfg" run; then
    echo "!!! VOD $id failed; continuing with the next one"
  fi
  rm -f "$cfg"
done
echo "=== backfill complete $(date -Is) ==="
