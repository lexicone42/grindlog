#!/usr/bin/env bash
# Import ONE completed per-VOD backfill database into the live database in
# place: the live db's rows for the broadcast day(s) that VOD covers (live
# capture or an earlier pass) are replaced by the VOD's sessions, runs and
# splits. Runs inside a transaction, so the live bot can keep the db open.
#
#   ./scripts/import-vod.sh <vod_id> [--deploy] [--force]
#
# This is the path the backfill lands through: import-when-done.sh calls it
# per VOD as each chain finishes. merge-backfill.sh is the full chronological
# rebuild into a fresh database; it copies splits verbatim and does not apply
# the final-act normalisation below, so a rebuild from per-VOD databases
# written by an older binary brings back the misread final-act gold this
# script removes.
#
# Before replacing anything it compares the incoming pass with what the live
# database holds for those days (runs, numbered runs, the span of the
# sessions) and refuses, exit 3, when the incoming day would have fewer than
# 90% of the existing runs or of the existing numbered runs: a pass that
# died partway, or a binary that stopped reading a theme, must not overwrite
# a fuller day. `--force` replaces anyway (a day whose earlier rows were
# phantom fragments really is thinner when read right); both counts and the
# spans are printed either way.
#
# Not everything it touches is confined to those days: every finished run's
# final-act split is set to its finish time and last-row splits on runs that
# never finished are dropped; attempt_number is renumbered chronologically
# across the whole database; then fill-run-numbers.sh runs on it. Days are
# the machine's local dates, so a VOD that crosses midnight replaces both.
# LIVE=<path> targets another database (a copy, for a dry run). Exits 1 when
# the per-VOD database is missing, 2 while its session is still open, 3 when
# the gate above refuses.
set -euo pipefail
cd "$(dirname "$0")/.."
LIVE="${LIVE:-ninja-gaiden.db}"   # override for a dry run on a copy
id="" deploy=false force=false
for a in "$@"; do
  case "$a" in
    --deploy) deploy=true;;
    --force) force=true;;
    -*) echo "unknown option $a (usage: $0 <vod_id> [--deploy] [--force])"; exit 1;;
    *) id="$a";;
  esac
done
[ -n "$id" ] || { echo "usage: $0 <vod_id> [--deploy] [--force]"; exit 1; }
f="backfill-db/vod-$id.db"
[ -f "$f" ] || { echo "no $f"; exit 1; }

open=$(sqlite3 "$f" "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL")
done_=$(sqlite3 "$f" "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NOT NULL")
if [ "$open" != "0" ] || [ "$done_" = "0" ]; then
  echo "vod $id is not complete yet ($done_ closed, $open open session(s))"; exit 2
fi
days=$(sqlite3 "$f" "SELECT GROUP_CONCAT(DISTINCT quote(date(started_at_ms/1000,'unixepoch','localtime'))) FROM sessions")
in_day="date(started_at_ms/1000,'unixepoch','localtime') IN ($days)"

# The gate: the incoming pass against the live database's rows for the same
# days. Spans are first session start to last session end (an open live
# session counts to now); hours are printed to a tenth.
hours() { printf '%d.%d h' $(($1 / 3600000)) $(($1 % 3600000 * 10 / 3600000)); }
pct() { if [ "$2" -gt 0 ]; then echo "$(($1 * 100 / $2))%"; else echo "n/a"; fi; }
read -r new_runs new_fin new_num new_span < <(sqlite3 -separator ' ' "$f" \
  "SELECT (SELECT COUNT(*) FROM runs), (SELECT IFNULL(SUM(outcome='finished'),0) FROM runs), (SELECT COUNT(ls_attempt) FROM runs),
          (SELECT IFNULL(MAX(ended_at_ms) - MIN(started_at_ms), 0) FROM sessions)")
read -r old_runs old_fin old_num old_span < <(sqlite3 -separator ' ' "$LIVE" \
  "SELECT (SELECT COUNT(*) FROM runs WHERE $in_day), (SELECT IFNULL(SUM(outcome='finished'),0) FROM runs WHERE $in_day),
          (SELECT COUNT(ls_attempt) FROM runs WHERE $in_day),
          (SELECT IFNULL(MAX(COALESCE(ended_at_ms, strftime('%s','now')*1000)) - MIN(started_at_ms), 0) FROM sessions WHERE $in_day)")
echo "vod $id covers $days: $new_runs runs, $new_fin finished, $new_num numbered, sessions span $(hours "$new_span")"
echo "replacing in $LIVE: $old_runs runs, $old_fin finished, $old_num numbered, sessions span $(hours "$old_span")" \
     "(incoming: $(pct "$new_runs" "$old_runs") of the runs, $(pct "$new_num" "$old_num") of the numbered, $(pct "$new_span" "$old_span") of the span)"
if [ $((new_runs * 10)) -lt $((old_runs * 9)) ] || [ $((new_num * 10)) -lt $((old_num * 9)) ]; then
  if $force; then
    echo "gate: the incoming day is thinner than the one in $LIVE; --force given, replacing anyway"
  else
    echo "!!! refusing to replace a fuller day with a thinner pass (under 90% of the runs or numbered runs);" \
         "re-run the VOD, or \`$0 $id --force\` to replace anyway"
    exit 3
  fi
fi

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

if $deploy; then
  ./scripts/deploy-site.sh
fi
