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
# left behind and inserts that VOD's session and its completed games beside
# whatever else the day holds.
#
# Scoped to the VOD is still not scoped enough on its own, because the same
# morning-and-afternoon day is often ONE broadcast under ONE vod_id, which
# the Ninja Gaiden backfill's session for that day already carries. Matching
# on vod_id alone therefore deleted those runs — 30 of the 38 captured
# marathons share a VOD with Ninja Gaiden runs, 1888 of them in total. What
# this import owns is narrower: the sessions of the VOD that hold no run of
# another category. Rehearse against a copy anyway; that is how this was
# found.
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

  # Never touch a session that is still capturing. The delete below is
  # scoped to source = 'vod' so this cannot fire on the live row any more,
  # but an open session sharing this VOD means the bot is watching the very
  # broadcast being imported — replacing it underneath itself is not
  # something to do on a technicality.
  live=$(q "$LIVE" "SELECT COUNT(*) FROM sessions WHERE vod_id = '$id' AND ended_at_ms IS NULL")
  if [ "${live:-0}" -gt 0 ]; then
    echo "!!! vod $id is still being captured (an open session carries it)" >&2
    echo "    import it once the broadcast has ended" >&2
    exit 2
  fi

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
  #
  # "What an earlier import left" is not everything carrying this vod_id.
  # A day he spent on Ninja Gaiden in the morning and the marathon in the
  # afternoon is ONE broadcast under ONE vod_id, so the Ninja Gaiden
  # backfill's session for that day shares the id this import is scoped to.
  # Deleting by vod_id alone took those runs with it: 30 of the 38 captured
  # marathons sit in a VOD that already holds Ninja Gaiden runs, 1888 of
  # them altogether, and a rehearsal against a copy stopped at the first one
  # with 57 runs gone. So this owns only the sessions of the VOD that hold
  # no run of another category — the ones it wrote itself, and the empty
  # ones a half-finished import left.
  q "$LIVE" "
    ATTACH DATABASE '$(pwd)/$src' AS src;
    BEGIN IMMEDIATE;
    CREATE TEMP TABLE mine AS
      SELECT s.id FROM sessions s
       WHERE s.vod_id = '$id'
         -- POSITIVE ownership, not a residual test. The first version asked
         -- 'does this session hold a run of another category?' and took
         -- silence for consent -- but db::set_session_vod stamps the
         -- broadcast's VOD id on the LIVE hls session too, and a session
         -- that has recorded nothing yet holds no run of any category. So
         -- the predicate selected the session capturing the stream at that
         -- moment. Deleting it strands every run of the rest of the
         -- broadcast on a dangling session_id, and silences healthcheck.sh
         -- and deploy-if-live.sh, both of which look for an open hls row.
         AND s.source = 'vod'
         AND NOT EXISTS (SELECT 1 FROM runs r
                          WHERE r.session_id = s.id AND r.category <> 'Arcathlon');
    DELETE FROM splits WHERE run_id IN
      (SELECT id FROM runs WHERE session_id IN (SELECT id FROM mine));
    DELETE FROM runs WHERE session_id IN (SELECT id FROM mine);
    DELETE FROM sessions WHERE id IN (SELECT id FROM mine);
    DROP TABLE mine;
    INSERT INTO sessions (started_at_ms, ended_at_ms, source, label, tag, frames, parsed,
                          probing, relocks, counter_reads, events, vod_id, vod_created_at_ms)
      SELECT started_at_ms, ended_at_ms, source, label, tag, frames, parsed,
             probing, relocks, counter_reads, events, '$id', vod_created_at_ms
      FROM src.sessions ORDER BY started_at_ms;
    -- The session just written, held aside: the VOD's other sessions carry
    -- the same vod_id, and last_insert_rowid() moves with every run below.
    CREATE TEMP TABLE target AS SELECT last_insert_rowid() AS id;
    INSERT INTO runs (game, category, attempt_number, started_at_ms, ended_at_ms, outcome,
                      reset_reason, final_time_ms, last_timer_ms, session_id, ls_attempt)
      SELECT r.game, r.category,
             (SELECT COUNT(*) FROM runs x WHERE x.game = r.game AND x.category = r.category
                AND x.started_at_ms <= r.started_at_ms) + 1,
             r.started_at_ms, r.ended_at_ms, r.outcome, r.reset_reason, r.final_time_ms,
             r.last_timer_ms, (SELECT id FROM target), r.ls_attempt
      FROM src.runs r WHERE r.category = 'Arcathlon' ORDER BY r.started_at_ms;
    DROP TABLE target;
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
