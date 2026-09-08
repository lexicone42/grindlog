#!/usr/bin/env bash
# Check every replayed marathon broadcast against the answer key its own board
# derives.
#
#   ./scripts/audit-arcathlon.sh [> before.txt]
#
# A marathon board prints two kinds of time in one column and they look
# identical in a single frame: a game he has finished shows his RESULT, one he
# has not reached shows the COMPARISON. Over a whole broadcast they are not
# identical at all — the row he plays changes exactly once and no other row
# changes at all — so a board log says which of the event's ten games he
# played, and in what time, all by itself. No video, no timer, no title, and
# no answer key read off the stream by hand.
#
# This replays every broadcast under $ARCA_OUT through the tracker's own
# decision sequence and prints, per broadcast: the roster its rows identify,
# every row with the first and last value it settled on, whether the board
# says it was played, whether the tracker recorded it, and the differences in
# both directions — a game played and not recorded, a run recorded the board
# does not account for, a run filed under a game the board gave to another
# row, and two runs of one broadcast under a single name (which an event of
# ten distinct games can never have).
#
# The way to use it is BEFORE and AFTER:
#
#   ./scripts/audit-arcathlon.sh > /tmp/before.txt      # on current main
#   ...make the change, cargo build --release...
#   ./scripts/audit-arcathlon.sh > /tmp/after.txt
#   diff <(grep -E '^(REC|BAD|SUM) ' /tmp/before.txt) <(grep -E '^(REC|BAD|SUM) ' /tmp/after.txt)
#
# The REC, BAD and SUM lines are there to be diffed: one REC per run recorded
# (vod, row, cumulative ms, segment ms, game), one BAD per run the board does
# not account for, and one SUM per broadcast. A change to
# src/marathon.rs, src/roster.rs, src/signature.rs or the gate in
# src/sanity.rs that moves a single line of those has moved a real recording.
#
# The same check runs as a test:
#   ARCATHLON_DB=arcathlon-db cargo test --release \
#     replays_every_captured -- --ignored --nocapture
#
# The key is OCR like everything it checks. It disagrees with the answer keys
# read off the video by hand in one known place (2827296024's Astyanax, where
# the hand-read key is right), so read a row off the board log before
# believing either.
#
# Environment: ARCA_OUT (the capture working set, default arcathlon-db/, as
# scripts/replay-arcathlon.sh leaves it), ARCA_BIN, ARCA_ROSTER.
set -uo pipefail
cd "$(dirname "$0")/.."
out=${ARCA_OUT:-arcathlon-db}
bin=${ARCA_BIN:-./target/release/ngtwitchtimer}
roster=${ARCA_ROSTER:-assets/arcathlon-rosters.toml}
# Nothing here decodes video or runs OCR, but the binary re-execs itself when
# this is unset; save it the trip.
export OMP_THREAD_LIMIT=1 OMP_NUM_THREADS=1

if [ ! -d "$out" ]; then
  echo "no capture working set at $out — run scripts/replay-arcathlon.sh first" >&2
  exit 1
fi
if [ ! -x "$bin" ]; then
  echo "no binary at $bin — cargo build --release" >&2
  exit 1
fi

# The board-mode entry the replays were made with, and nothing else: the audit
# reads logs, so it needs no stream, no crops and no database. Kept here
# rather than pointed at live.toml so this never opens the live deployment's
# configuration by accident.
cfg=$(mktemp /tmp/ngaudit-XXXXXX.toml)
trap 'rm -f "$cfg"' EXIT
cat > "$cfg" <<EOF
[stream]
channel = "arcus"

[game]
name = "Ninja Gaiden (NES)"
category = "Any%"

[[games]]
name = "Arcathlon"
category = "10 games"
match = ["arcath", "randomized"]
mode = "board"
roster = "$roster"
EOF

"$bin" --config "$cfg" audit --dir "$out"
