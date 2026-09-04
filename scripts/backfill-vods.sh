#!/usr/bin/env bash
# Analyze Twitch VODs one after another, each into its own database
# (backfill-db/vod-<id>.db) so several chains can run in parallel. Streams
# each VOD directly from Twitch — no downloads.
#
#   ./scripts/backfill-vods.sh <vod_id> [vod_id...]
#
# Afterwards: import-when-done.sh (detached; imports each VOD as its chain
# finishes and redeploys the site) or import-vod.sh (one VOD) land each day
# in the live database. merge-backfill.sh still rebuilds everything
# chronologically, but it does not apply import-vod.sh's final-act split
# normalisation, so only feed it per-VOD databases written by a current
# binary. Broadcast start times are fetched from Twitch automatically, so
# runs land on the original timeline. Workers run under `nice` so the live
# bot keeps priority. With the glyph reader the timer costs ~2 ms a frame;
# tesseract still reads the frames it declines, the splits and counter
# crops, and the pane's title and reference rows at each lock and every 60 s.
#
# Assumes the release binary at target/release/ngtwitchtimer
# (scripts/build-release.sh), the AppImage tessdata under
# ~/.local/opt/tesseract-appimage and the templates in assets/glyphs.json;
# edit the generated config below for a different install. Per VOD it writes
# backfill-db/vod-<id>.db and backfill-logs/obs-<id>.jsonl, replacing an
# earlier pass over the same VOD. A VOD whose analysis fails is reported and
# the chain moves on; its session is left open, so import-vod.sh and
# import-when-done.sh skip it until it is re-run.
set -uo pipefail
cd "$(dirname "$0")/.."
mkdir -p backfill-logs backfill-db
# One OpenMP thread per worker: tesseract's threads only spin-wait on crops
# this small, and several workers on one box otherwise starve each other.
export OMP_THREAD_LIMIT=1 OMP_NUM_THREADS=1

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
# Two reads a second: catches attempts that die within three seconds and the
# run number on far more short ones; halves backfill speed. A chain still runs
# at roughly 5-10x realtime with the glyph reader (about 15x while locked on
# the timer; slower over stretches with no timer on screen, or with other
# chains and the live bot sharing the box).
fps = 2

[ocr]
engine = "auto"
tessdata_path = "$HOME/.local/opt/tesseract-appimage/usr/share/tesseract-ocr/5/tessdata"

[timer]
crop_x = 285
crop_y = 800
crop_w = 390
crop_h = 100
threshold = 60
retry_thresholds = [75, 45]
# The purpose-built digit reader, as live; tesseract reads what it declines.
reader = "glyph"
glyph_templates = "$PWD/assets/glyphs.json"

[splits]
enabled = true
crop_x = 535
crop_y = 498
crop_w = 135
crop_h = 288

[detection]
# Nobody finishes NG Any% under 11:00 (WR 11:32): a "finish" below that is a
# frozen timer (stream stall, pause), not a run.
min_final_ms = 660000

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

[[layouts]]
name = "ng-theme"
timer = { crop_x = 265, crop_y = 765, crop_w = 360, crop_h = 100 }
splits = { crop_x = 515, crop_y = 513, crop_w = 100, crop_h = 252 }
attempts_counter = { crop_x = 540, crop_y = 487, crop_w = 80, crop_h = 30 }
lifetime_sob = { crop_x = 505, crop_y = 897, crop_w = 110, crop_h = 34 }

[database]
# One db per VOD so several chains can run in parallel; merge chronologically
# afterwards (attempt numbers are assigned per db, so the merge renumbers).
path = "backfill-db/vod-$id.db"

[debug]
obs_log = "backfill-logs/obs-$id.jsonl"
EOF
  echo "=== VOD $id — $(date -Is) ==="
  # A rerun must replace, not append to, an earlier pass over the same VOD.
  rm -f "backfill-db/vod-$id.db" "backfill-db/vod-$id.db-wal" "backfill-db/vod-$id.db-shm" \
        "backfill-logs/obs-$id.jsonl"
  if ! nice -n 15 ./target/release/ngtwitchtimer --config "$cfg" run; then
    echo "!!! VOD $id failed; continuing with the next one"
  fi
  rm -f "$cfg"
done
echo "=== backfill complete $(date -Is) ==="
