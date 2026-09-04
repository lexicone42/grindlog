#!/usr/bin/env bash
# Import ONE completed per-VOD backfill database into the live database in
# place: the live db's rows for the broadcast day(s) that VOD covers (live
# capture or an earlier pass) are replaced by the VOD's sessions, runs and
# splits. Runs inside a transaction, so the live bot can keep the db open.
#
#   ./scripts/import-vod.sh <vod_id> [--deploy]
#
# This is the path the backfill lands through: import-when-done.sh calls it
# per VOD as each chain finishes. merge-backfill.sh is the full chronological
# rebuild into a fresh database; it copies splits verbatim and does not apply
# the final-act normalisation below, so a rebuild from per-VOD databases
# written by an older binary brings back the misread final-act gold this
# script removes.
#
# Not everything it touches is confined to those days: every finished run's
# final-act split is set to its finish time and last-row splits on runs that
# never finished are dropped; attempt_number is renumbered chronologically
# across the whole database; then fill-run-numbers.sh runs on it. Days are
# the machine's local dates, so a VOD that crosses midnight replaces both.
# LIVE=<path> targets another database (a copy, for a dry run). Exits 1 when
# the per-VOD database is missing, 2 while its session is still open.
set -euo pipefail
cd "$(dirname "$0")/.."
LIVE="${LIVE:-ninja-gaiden.db}"   # override for a dry run on a copy
id="${1:?vod id}"
f="backfill-db/vod-$id.db"
[ -f "$f" ] || { echo "no $f"; exit 1; }

open=$(sqlite3 "$f" "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL")
done_=$(sqlite3 "$f" "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NOT NULL")
if [ "$open" != "0" ] || [ "$done_" = "0" ]; then
  echo "vod $id is not complete yet ($done_ closed, $open open session(s))"; exit 2
fi
days=$(sqlite3 "$f" "SELECT GROUP_CONCAT(DISTINCT quote(date(started_at_ms/1000,'unixepoch','localtime'))) FROM sessions")
echo "vod $id covers $days: $(sqlite3 "$f" "SELECT COUNT(*)||' runs, '||SUM(outcome='finished')||' finished, '||COUNT(ls_attempt)||' numbered' FROM runs")"
echo "replacing in $LIVE: $(sqlite3 "$LIVE" "SELECT COUNT(*)||' runs' FROM runs WHERE date(started_at_ms/1000,'unixepoch','localtime') IN ($days)")"

# Capture-health columns exist only in databases written by newer binaries.
have=$(sqlite3 "$f" "SELECT GROUP_CONCAT(name) FROM pragma_table_info('sessions')")
hc=""
for c in frames parsed probing relocks counter_reads events; do
  case ",$have," in *",$c,"*) hc+="$c, ";; *) hc+="NULL AS $c, ";; esac
done
hc="${hc%, }"

# SQL on STDIN with -bail, never as an argument: given SQL as an argument the
# CLI keeps going after a runtime error (reaching the COMMIT, making the
# day-wipe DELETEs permanent) and still exits 0. On stdin with -bail it stops
# at the error, the transaction rolls back, and the exit code is non-zero.
sqlite3 -bail "$LIVE" <<SQL
  PRAGMA busy_timeout = 20000;
  BEGIN IMMEDIATE;
  ATTACH '$f' AS s;
  DELETE FROM splits WHERE run_id IN (SELECT id FROM runs WHERE date(started_at_ms/1000,'unixepoch','localtime') IN ($days));
  DELETE FROM runs WHERE date(started_at_ms/1000,'unixepoch','localtime') IN ($days);
  DELETE FROM sessions WHERE date(started_at_ms/1000,'unixepoch','localtime') IN ($days);
  INSERT INTO sessions (started_at_ms, ended_at_ms, source, label, tag, frames, parsed, probing, relocks, counter_reads, events)
    SELECT started_at_ms, ended_at_ms, source, label, tag, $hc FROM s.sessions ORDER BY id;
  -- Sessions/runs are matched back by start time (unique within one VOD).
  INSERT INTO runs (game, category, attempt_number, started_at_ms, ended_at_ms, outcome, reset_reason,
                    final_time_ms, last_timer_ms, ls_attempt, session_id)
    SELECT r.game, r.category, 0, r.started_at_ms, r.ended_at_ms, r.outcome, r.reset_reason,
           r.final_time_ms, r.last_timer_ms, r.ls_attempt,
           (SELECT m.id FROM main.sessions m JOIN s.sessions ss ON ss.id = r.session_id
             WHERE m.started_at_ms = ss.started_at_ms AND m.source = ss.source)
    FROM s.runs r ORDER BY r.started_at_ms, r.id;
  INSERT INTO splits (run_id, act_index, act_name, cumulative_ms, segment_ms)
    SELECT (SELECT m.id FROM main.runs m
             WHERE m.started_at_ms = sr.started_at_ms
               AND m.session_id = (SELECT m2.id FROM main.sessions m2 JOIN s.sessions ss ON ss.id = sr.session_id
                                    WHERE m2.started_at_ms = ss.started_at_ms AND m2.source = ss.source)),
           sp.act_index, sp.act_name, sp.cumulative_ms, sp.segment_ms
    FROM s.splits sp JOIN s.runs sr ON sr.id = sp.run_id;
  -- The final act's split IS the finish. Older per-VOD databases carry the
  -- column's reading for that row (a misread comparison value), which made
  -- an impossible gold; normalise every finished run's last row to its
  -- finish time, and drop last-row splits on runs that never finished.
  UPDATE splits SET
    cumulative_ms = (SELECT r.final_time_ms FROM runs r WHERE r.id = splits.run_id),
    segment_ms = (SELECT r.final_time_ms FROM runs r WHERE r.id = splits.run_id)
               - COALESCE((SELECT p.cumulative_ms FROM splits p WHERE p.run_id = splits.run_id
                           AND p.act_index = (SELECT MAX(act_index) FROM splits) - 1), 0)
  WHERE act_index = (SELECT MAX(act_index) FROM splits)
    AND run_id IN (SELECT id FROM runs WHERE outcome = 'finished' AND final_time_ms IS NOT NULL);
  DELETE FROM splits WHERE id IN (
    SELECT s.id FROM splits s JOIN runs r ON r.id = s.run_id
    WHERE s.act_index = (SELECT MAX(act_index) FROM splits) AND r.outcome != 'finished');
  -- chronological attempt numbers across the whole db
  UPDATE runs SET attempt_number = (
    SELECT COUNT(*) FROM runs r2 WHERE r2.game = runs.game AND r2.category = runs.category
      AND (r2.started_at_ms < runs.started_at_ms OR (r2.started_at_ms = runs.started_at_ms AND r2.id <= runs.id)));
  COMMIT;
  DETACH s;
SQL

./scripts/fill-run-numbers.sh "$LIVE"
echo "now in $LIVE for $days: $(sqlite3 "$LIVE" "SELECT COUNT(*)||' runs, '||SUM(outcome='finished')||' finished, '||COUNT(ls_attempt)||' numbered, best '||IFNULL(printf('%d:%05.2f', MIN(final_time_ms)/60000, (MIN(final_time_ms)%60000)/1000.0), '-') FROM runs WHERE date(started_at_ms/1000,'unixepoch','localtime') IN ($days)")"

if [ "${2:-}" = "--deploy" ]; then
  ./scripts/deploy-site.sh
fi
