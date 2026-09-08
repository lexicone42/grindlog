#!/usr/bin/env bash
# Import one marathon broadcast's games into the live database, from the
# per-VOD database scripts/replay-arcathlon.sh wrote.
#
#   ./scripts/import-arcathlon.sh <vod_id>... [--deploy]
#
# This is NOT import-vod.sh. That one replaces a broadcast DAY, which is
# right for a Ninja Gaiden VOD and wrong here: he has streamed Ninja Gaiden
# in the morning and a marathon the same afternoon (2026-07-23, two VODs,
# and the live database already holds 45 Ninja Gaiden runs for that date),
# so replacing the day would delete them. A marathon import is additive and
# scoped to its own VOD: it removes only what a previous import of THIS VOD
# left behind — matched on the session's vod_id, not on the date — and
# inserts that VOD's session and its completed games beside whatever else
# the day holds.
#
# What lands: one session (source "vod", tagged with the event, carrying the
# VOD id so every run can be linked to its moment) and one run per completed
# board row, each filed under the game the board named it and category
# "Arcathlon". Nothing else in the database is read or written.
#
# Runs inside one transaction per VOD, so the live bot can keep the database
# open. Re-running is safe and idempotent. `LIVE=copy.db` targets another
# database, which is how to rehearse this. `--deploy` rebuilds and uploads
# the site afterwards.
#
# Exit 1 when a per-VOD database is missing or holds no marathon runs, 2
# when its session is still open (the replay is unfinished or died).
set -euo pipefail
cd "$(dirname "$0")/.."

LIVE=${LIVE:-ninja-gaiden.db}
ARCA_OUT=${ARCA_OUT:-arcathlon-db}
deploy=false ids=()
for a in "$@"; do
  case "$a" in
    --deploy) deploy=true;;
    -*) echo "unknown option $a (usage: $0 <vod_id>... [--deploy])" >&2; exit 1;;
    *) ids+=("$a");;
  esac
done
[ ${#ids[@]} -gt 0 ] || { echo "usage: $0 <vod_id>... [--deploy]" >&2; exit 1; }
[ -f "$LIVE" ] || { echo "no live database at $LIVE" >&2; exit 1; }

q() { sqlite3 -cmd '.timeout 10000' "$1" "$2"; }

for id in "${ids[@]}"; do
  src="$ARCA_OUT/vod-$id.db"
  [ -f "$src" ] || { echo "!!! $src is missing — replay it first (scripts/replay-arcathlon.sh $id)" >&2; exit 1; }
  open=$(q "$src" "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL")
  [ "$open" = 0 ] || { echo "!!! $src still has an open session; the replay did not finish" >&2; exit 2; }
  runs=$(q "$src" "SELECT COUNT(*) FROM runs WHERE category = 'Arcathlon'")
  [ "${runs:-0}" -gt 0 ] || { echo "!!! $src holds no Arcathlon runs" >&2; exit 1; }

  # One field per query: sqlite3 separates columns with "|", which `read`
  # would hand to the first variable whole — and a $day of
  # "2026-07-23|10|Arcathlon" made the guard below compare 0 with 0 and
  # check nothing, on the one day that actually needed it.
  day=$(q "$src" "SELECT date(MIN(started_at_ms)/1000,'unixepoch','localtime') FROM runs WHERE category = 'Arcathlon'")
  games=$(q "$src" "SELECT COUNT(*) FROM runs WHERE category = 'Arcathlon'")
  tag=$(q "$src" "SELECT COALESCE(tag,'Arcathlon') FROM sessions LIMIT 1")
  had=$(q "$LIVE" "SELECT COUNT(*) FROM runs r JOIN sessions s ON r.session_id = s.id WHERE s.vod_id = '$id'")
  other=$(q "$LIVE" "SELECT COUNT(*) FROM runs WHERE date(started_at_ms/1000,'unixepoch','localtime') = '$day' AND category != 'Arcathlon'")
  echo "vod $id ($day, $tag): $games game(s) to import; $had already from this VOD; $other run(s) of another category that day are left alone"

  # One transaction: drop what an earlier import of THIS VOD left, then
  # insert its session and its games. Row ids are the live database's own,
  # so nothing here depends on the per-VOD database's numbering.
  q "$LIVE" "
    ATTACH DATABASE '$(pwd)/$src' AS src;
    BEGIN IMMEDIATE;
    DELETE FROM splits WHERE run_id IN
      (SELECT r.id FROM runs r JOIN sessions s ON r.session_id = s.id WHERE s.vod_id = '$id');
    DELETE FROM runs WHERE session_id IN (SELECT id FROM sessions WHERE vod_id = '$id');
    DELETE FROM sessions WHERE vod_id = '$id';
    INSERT INTO sessions (started_at_ms, ended_at_ms, source, label, tag, frames, parsed,
                          probing, relocks, counter_reads, events, vod_id, vod_created_at_ms)
      SELECT started_at_ms, ended_at_ms, source, label, tag, frames, parsed,
             probing, relocks, counter_reads, events, '$id', vod_created_at_ms
      FROM src.sessions ORDER BY started_at_ms;
    INSERT INTO runs (game, category, attempt_number, started_at_ms, ended_at_ms, outcome,
                      reset_reason, final_time_ms, last_timer_ms, session_id, ls_attempt)
      SELECT r.game, r.category,
             (SELECT COUNT(*) FROM runs x WHERE x.game = r.game AND x.category = r.category
                AND x.started_at_ms <= r.started_at_ms) + 1,
             r.started_at_ms, r.ended_at_ms, r.outcome, r.reset_reason, r.final_time_ms,
             r.last_timer_ms, (SELECT id FROM sessions WHERE vod_id = '$id' LIMIT 1), r.ls_attempt
      FROM src.runs r WHERE r.category = 'Arcathlon' ORDER BY r.started_at_ms;
    COMMIT;
    DETACH DATABASE src;"

  now=$(q "$LIVE" "SELECT COUNT(*) FROM runs r JOIN sessions s ON r.session_id = s.id WHERE s.vod_id = '$id'")
  still=$(q "$LIVE" "SELECT COUNT(*) FROM runs WHERE date(started_at_ms/1000,'unixepoch','localtime') = '$day' AND category != 'Arcathlon'")
  echo "  imported: $now run(s) from this VOD; $still of another category that day (was $other)"
  [ "$still" = "$other" ] || { echo "!!! that day lost runs of another category — investigate before importing more" >&2; exit 1; }
done

echo "games now in the database: $(q "$LIVE" "SELECT COUNT(DISTINCT game) FROM runs WHERE category = 'Arcathlon'") across $(q "$LIVE" "SELECT COUNT(*) FROM runs WHERE category = 'Arcathlon'") run(s)"
if $deploy; then
  echo "--- deploying the site"
  ./scripts/deploy-site.sh
fi
