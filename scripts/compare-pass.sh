#!/usr/bin/env bash
# What a re-analysed or backfilled pass over a VOD would change in the live
# database, day by day, before import-vod.sh replaces those days with it.
#
#   scripts/compare-pass.sh <pass.db> [live.db]
#
# For the broadcast day(s) the pass covers, side by side: runs, finished
# runs, the finished runs whose final time differs (and by how much), splits
# recorded, resets whose clock went past the slowest Act 1 ever finished
# without a split for it (the gap re-analysis exists to close), and rows of
# board-tracked events. Read-only on both databases.
set -uo pipefail
pass=$1
live=${2:-ninja-gaiden.db}
[ -f "$pass" ] || { echo "no pass at $pass" >&2; exit 1; }
days=$(sqlite3 -readonly "$pass" "SELECT GROUP_CONCAT(DISTINCT quote(date(started_at_ms/1000,'unixepoch','localtime'))) FROM sessions")
[ -n "$days" ] || { echo "$pass: no sessions" >&2; exit 1; }
in_day() { echo "date(${1:+$1.}started_at_ms/1000,'unixepoch','localtime') IN ($days)"; }
act1=$(sqlite3 -readonly "$live" "SELECT MAX(s.cumulative_ms) FROM splits s JOIN runs r ON r.id = s.run_id WHERE r.outcome = 'finished' AND s.act_index = 0")
stats() {
  sqlite3 -readonly -separator ' ' "$1" "
    SELECT COUNT(*), SUM(outcome = 'finished'),
           (SELECT COUNT(*) FROM splits s JOIN runs r ON r.id = s.run_id WHERE $(in_day r)),
           SUM(outcome = 'reset' AND category = 'Any%' AND last_timer_ms > ${act1:-60000} + 5000),
           SUM(outcome = 'reset' AND category = 'Any%' AND last_timer_ms > ${act1:-60000} + 5000
               AND NOT EXISTS (SELECT 1 FROM splits s WHERE s.run_id = runs.id)),
           SUM(category NOT IN ('Any%'))
    FROM runs WHERE $(in_day)"
}
read -r lr lf ls lp lm lb <<<"$(stats "$live")"
read -r pr pf ps pp pm pb <<<"$(stats "$pass")"
printf '%-34s %8s %8s\n' "day(s) $days" live pass
printf '%-34s %8s %8s\n' runs "${lr:-0}" "${pr:-0}"
printf '%-34s %8s %8s\n' finished "${lf:-0}" "${pf:-0}"
printf '%-34s %8s %8s\n' splits "${ls:-0}" "${ps:-0}"
printf '%-34s %8s %8s\n' "resets past Act 1" "${lp:-0}" "${pp:-0}"
printf '%-34s %8s %8s\n' "  of them with no split" "${lm:-0}" "${pm:-0}"
printf '%-34s %8s %8s\n' "rows of other categories" "${lb:-0}" "${pb:-0}"
# Finished runs matched by start time (within 30 s): the final times that
# differ by more than a hundredth, and finishes only one side has.
sqlite3 -readonly "$live" "ATTACH '$pass' AS p;
  SELECT 'final differs: '||datetime(l.started_at_ms/1000,'unixepoch','localtime')||'  live '||l.final_time_ms||'  pass '||q.final_time_ms
    FROM runs l JOIN p.runs q ON abs(l.started_at_ms - q.started_at_ms) < 30000 AND l.category = q.category
   WHERE $(in_day l) AND l.outcome = 'finished' AND q.outcome = 'finished' AND abs(l.final_time_ms - q.final_time_ms) > 10;
  SELECT 'finished in live only: '||datetime(l.started_at_ms/1000,'unixepoch','localtime')||' '||l.category||' '||l.final_time_ms
    FROM runs l WHERE $(in_day l) AND l.outcome = 'finished'
     AND NOT EXISTS (SELECT 1 FROM p.runs q WHERE abs(l.started_at_ms - q.started_at_ms) < 30000 AND q.outcome = 'finished');
  SELECT 'finished in pass only: '||datetime(q.started_at_ms/1000,'unixepoch','localtime')||' '||q.category||' '||q.final_time_ms
    FROM p.runs q WHERE $(in_day q) AND q.outcome = 'finished'
     AND NOT EXISTS (SELECT 1 FROM main.runs l WHERE abs(l.started_at_ms - q.started_at_ms) < 30000 AND l.outcome = 'finished');"
# The verdict, said plainly: import-vod.sh replaces the whole day, so
# anything only the live database has for these days is deleted by it.
lost_fin=$(sqlite3 -readonly "$live" "ATTACH '$pass' AS p;
  SELECT COUNT(*) FROM runs l WHERE $(in_day l) AND l.outcome = 'finished'
     AND NOT EXISTS (SELECT 1 FROM p.runs q WHERE abs(l.started_at_ms - q.started_at_ms) < 30000 AND q.outcome = 'finished')")
lost_other=$(( ${lb:-0} - ${pb:-0} ))
if [ "${lost_fin:-0}" -gt 0 ] || [ "$lost_other" -gt 0 ]; then
  echo "VERDICT: do not import as is: it would delete ${lost_fin:-0} finished run(s) and $(( lost_other > 0 ? lost_other : 0 )) row(s) of other categories (a marathon or race the same day, from another VOD) that only the live database has"
elif [ "${pm:-0}" -lt "${lm:-0}" ] || [ "${ps:-0}" -gt "${ls:-0}" ]; then
  echo "VERDICT: a gain: $(( ${lm:-0} - ${pm:-0} )) fewer resets past Act 1 without a split, $(( ${ps:-0} - ${ls:-0} )) more split(s), nothing lost; rehearse with LIVE=<a copy> first"
else
  echo "VERDICT: no gain; nothing to import"
fi
