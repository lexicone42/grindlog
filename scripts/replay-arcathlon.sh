#!/usr/bin/env bash
# Replay a marathon broadcast into a scratch database, one VOD per chain.
#
#   ./scripts/replay-arcathlon.sh <vod_id> [vod_id...]
#
# A marathon day ("Arcathlon": ten NES games back to back, one LiveSplit
# split row per game) is not a Ninja Gaiden day: the pane's big timer is the
# event's running total, it pauses between games and never resets, so the run
# state machine has nothing to read. The board does. The config below turns
# on `[[games]] mode = "board"` (see docs/detection.md, "Marathon days"), and every
# row that gains a time is recorded as a finished run of that game — game
# "Astyanax", category "Arcathlon" — in its own database, under the name
# assets/arcathlon-rosters.toml gives that game rather than under whatever OCR
# read off the row.
#
# Per VOD it writes, under $ARCA_OUT (default arcathlon-db/):
#   vod-<id>.db          runs, one per completed row, and the session
#   boards-<id>.jsonl    the board as read at each pane pass (debug.board_log)
#   obs-<id>.jsonl       the per-frame observation log
#   log-<id>.txt         the bot's own log, with a line per completion
# A rerun replaces an earlier pass over the same VOD. Nothing here touches
# the live database or the running bot.
#
# What each row of the board turns into:
#   sqlite3 -header -column "$ARCA_OUT/vod-<id>.db" \
#     "select game, final_time_ms/1000 secs, last_timer_ms/1000 total \
#      from runs order by last_timer_ms"
#
# Environment: ARCA_OUT (output directory), ARCA_FPS (default 1 — the board
# is read once a minute, so more frames buy only a sharper marathon total),
# ARCA_BIN (default target/release/ngtwitchtimer), ARCA_START (seconds into
# the VOD; a marathon has to be watched from its start, because a row that
# already carries its time when the board first comes into view was finished
# before the bot looked and is not recorded), ARCA_NICE (default 15, so the
# live bot keeps the box; lower it when nothing is live and the run has to
# finish, raise it never).
set -uo pipefail
cd "$(dirname "$0")/.."
out=${ARCA_OUT:-arcathlon-db}
fps=${ARCA_FPS:-1}
bin=${ARCA_BIN:-./target/release/ngtwitchtimer}
start=${ARCA_START:-0}
prio=${ARCA_NICE:-15}
mkdir -p "$out"
# One OpenMP thread per worker: tesseract's threads only spin-wait on crops
# this small, and several workers on one box otherwise starve each other.
export OMP_THREAD_LIMIT=1 OMP_NUM_THREADS=1

for id in "$@"; do
  cfg=$(mktemp /tmp/ngarca-XXXXXX.toml)
  cat > "$cfg" <<EOF
[stream]
channel = "arcus"
source = "vod"
vod_id = "$id"
start_secs = $start
quality = "480p30"
fps = $fps

[ocr]
engine = "auto"
tessdata_path = "$HOME/.local/opt/tesseract-appimage/usr/share/tesseract-ocr/5/tessdata"

[timer]
# The marathon total, not the segment timer one line below it. Its digits are
# 17 real pixels tall at 480p against the segment timer's 10, and the segment
# timer measured illegible at every threshold tried; the segment times come
# back anyway, from the board's own columns.
crop_x = 371
crop_y = 947
crop_w = 241
crop_h = 48
# Wide enough for both scene variants: the numbered events put the whole pane
# 10-15 px further right than the randomized ones, and a narrower crop clipped
# their last digit, so every reading read as clipped and the layout never
# locked.
#
# threshold 120, not the usual 60: the segment timer sits one blank pixel
# under the marathon's hundredths, and at 60 the two merge into one ink band
# that reaches the crop's bottom edge.
threshold = 120
retry_thresholds = [150, 90, 60]
# The glyph templates are trained on his Ninja Gaiden scenes and decline every
# frame of this one; retraining on this font was tried and measured worse than
# useless (0% right held out), because at 480p its glyphs touch and the
# segmenter cannot cut them. Tesseract reads this timer at 80-100%.
reader = "tesseract"

[splits]
enabled = true
# The whole pane, title row included. The names sit left of both time columns,
# and the title identifies the event: without the top 55 px the board reader
# takes the first game's name for the title and no [[games]] entry matches.
crop_x = 75
crop_y = 435
crop_w = 545
crop_h = 460
# 100, against the default 150: at 150 tesseract reads "26.12" for "26:12" and
# "12933" for "1:29:33" — the colon lost, the value wrong by an order of
# magnitude. At 100 the same frames read them exactly.
threshold = 100

[layout_search]
# Off, and this is the single most important line in the file. 36 px below the
# marathon total is the segment timer, which parses and advances with the
# clock, so between games — the total frozen, the segment running from zero —
# the offset probe takes it for the timer and re-anchors onto it.
drift_px = 0

[detection]
# His camera and "be right back" scenes run a couple of minutes and the whole
# frame, pane included, is blurred; three minutes of that is not a reset.
illegible_reset_count = 180

[game]
# The base game is what the TIMER would be recording, and on a marathon day it
# must record nothing: the title gate suspends it (the pane never says "Ninja
# Gaiden" on these days) and the board board-mode entry below does the
# recording. Ten acts because the pane geometry anchors its block over that
# many rows, and a marathon board has ten.
name = "Ninja Gaiden (NES)"
category = "Any%"
require_title_match = true
follow_title = "log"
acts = [
  { name = "Row 1" }, { name = "Row 2" }, { name = "Row 3" },
  { name = "Row 4" }, { name = "Row 5" }, { name = "Row 6" },
  { name = "Row 7" }, { name = "Row 8" }, { name = "Row 9" },
  { name = "Row 10" },
]

[[games]]
# Both words of his marathon title: on this board the second comes back under
# the OCR confidence gate often enough that an alias on "arcath" alone would
# miss frames. "Randomized Arcathlon" and "Arcathlon #6" file as one event.
name = "Arcathlon"
category = "10 games"
match = ["arcath", "randomized"]
mode = "board"
# One canonical name per game. Without it a row is filed under whatever OCR
# read — "nax" for Astyanax, "Castlevania Il", "SMB2" — and each spelling is
# its own history in runs.game. Path is from the repo root, which this script
# has already cd'd to.
roster = "assets/arcathlon-rosters.toml"

[attempts_counter]
enabled = false

[lifetime_sob]
enabled = false

[chat]
enabled = false

[database]
path = "$out/vod-$id.db"

[debug]
obs_log = "$out/obs-$id.jsonl"
board_log = "$out/boards-$id.jsonl"
EOF
  echo "=== Arcathlon VOD $id — $(date -Is) ==="
  rm -f "$out/vod-$id.db" "$out/vod-$id.db-wal" "$out/vod-$id.db-shm" \
        "$out/obs-$id.jsonl" "$out/boards-$id.jsonl" "$out/log-$id.txt"
  if nice -n "$prio" "$bin" --config "$cfg" run > "$out/log-$id.txt" 2>&1; then
    sed -i 's/\x1b\[[0-9;]*m//g' "$out/log-$id.txt"
    grep -c 'marathon row' "$out/log-$id.txt" | sed "s/^/VOD $id: /;s/$/ completed rows recorded/"
  else
    echo "!!! VOD $id failed; see $out/log-$id.txt"
  fi
  rm -f "$cfg"
done
echo "=== arcathlon replay complete $(date -Is) ==="
