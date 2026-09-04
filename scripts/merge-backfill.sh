#!/usr/bin/env bash
# Merge per-VOD backfill databases (backfill-db/vod-*.db) into one fresh
# database, chronologically, then append any live-tracked sessions whose
# broadcast day is NOT covered by a VOD. Output: ninja-gaiden-merged.db.
#
#   ./scripts/merge-backfill.sh            # build ninja-gaiden-merged.db
#   ./scripts/merge-backfill.sh --swap     # ...and swap it in for ninja-gaiden.db
#                                          #   (stop the live bot first!)
#
# Attempt numbers are renumbered in chronological order. Settings (game,
# category, ls_sob_ms, ...) are copied from the live db.
#
# Day to day the backfill lands through import-vod.sh / import-when-done.sh;
# this is the full rebuild. Unlike import-vod.sh it does not set each finished
# run's final-act split to its finish time (or drop the final-act row on
# unfinished runs), so a rebuild that includes per-VOD databases written by
# an older binary can reintroduce impossible golds and a wrong Sum of Best.
# Re-run those VODs, or apply import-vod.sh's UPDATE/DELETE to the merged db
# before --swap.
set -euo pipefail
cd "$(dirname "$0")/.."
LIVE=ninja-gaiden.db
OUT=ninja-gaiden-merged.db
rm -f "$OUT" "$OUT-wal" "$OUT-shm"

# Fresh schema from the live db (tables only, no data), plus scratch columns
# for id remapping that are dropped at the end.
sqlite3 "$LIVE" ".schema" | grep -vE 'sqlite_autoindex|sqlite_sequence' | sqlite3 "$OUT"
sqlite3 "$OUT" "ALTER TABLE sessions ADD COLUMN src TEXT; ALTER TABLE sessions ADD COLUMN src_id INTEGER;
               ALTER TABLE runs ADD COLUMN src TEXT; ALTER TABLE runs ADD COLUMN src_id INTEGER; ALTER TABLE runs ADD COLUMN src_session INTEGER;"

# Order VOD dbs by their session start. Only COMPLETE VODs are merged (their
# session closed at end of input); in-progress ones are picked up next time.
ordered=$(for f in backfill-db/vod-*.db; do
  s=$(sqlite3 "$f" "SELECT MIN(started_at_ms) FROM sessions WHERE ended_at_ms IS NOT NULL" 2>/dev/null || true)
  # (an if/fi, not &&: a trailing false test would fail the pipeline under pipefail)
  if [ -n "$s" ]; then echo "$s $f"; fi
done | sort -n | awk '{print $2}')
total=$(ls backfill-db/vod-*.db 2>/dev/null | wc -l)
complete=$(printf '%s\n' $ordered | grep -c . || true)
echo "merging $complete complete VOD database(s); skipping $((total - complete)) in progress"

# Capture-health columns exist only in databases written by newer binaries:
# select each one if the source has it, else NULL.
health_cols() { # path -> "expr AS frames, expr AS parsed, ..."
  local have; have=$(sqlite3 "$1" "SELECT GROUP_CONCAT(name) FROM pragma_table_info('sessions')")
  local out="" c
  for c in frames parsed probing relocks counter_reads events; do
    case ",$have," in *",$c,"*) out+="$c, ";; *) out+="NULL AS $c, ";; esac
  done
  echo "${out%, }"
}

import_db() { # path tag [session-filter-sql]
  local f="$1" tag="$2" filt="${3:-1}"
  local hc; hc=$(health_cols "$f")
  sqlite3 "$OUT" "
    ATTACH '$f' AS s;
    INSERT INTO sessions (started_at_ms, ended_at_ms, source, label, tag, frames, parsed, probing, relocks, counter_reads, events, src, src_id)
      SELECT started_at_ms, ended_at_ms, source, label, tag, $hc, '$tag', id FROM s.sessions WHERE $filt ORDER BY id;
    INSERT INTO runs (game, category, attempt_number, started_at_ms, ended_at_ms, outcome, reset_reason,
                      final_time_ms, last_timer_ms, ls_attempt, src, src_id, src_session)
      SELECT game, category, attempt_number, started_at_ms, ended_at_ms, outcome, reset_reason,
             final_time_ms, last_timer_ms, ls_attempt, '$tag', id, session_id
      FROM s.runs WHERE session_id IN (SELECT id FROM s.sessions WHERE $filt) ORDER BY started_at_ms, id;
    INSERT INTO splits (run_id, act_index, act_name, cumulative_ms, segment_ms)
      SELECT m.id, sp.act_index, sp.act_name, sp.cumulative_ms, sp.segment_ms
      FROM s.splits sp JOIN runs m ON m.src = '$tag' AND m.src_id = sp.run_id;
    DETACH s;"
}

n=0
for f in $ordered; do
  import_db "$f" "$(basename "$f" .db)"
  n=$((n+1))
done
echo "imported $n VOD databases"

# Live sessions whose day is not covered by any VOD session (compare local dates).
covered=$(sqlite3 "$OUT" "SELECT GROUP_CONCAT(DISTINCT quote(date(started_at_ms/1000,'unixepoch','localtime'))) FROM sessions")
# (source 'file' = the early hand-run VOD analyses; kept until the streamed VOD version covers that day)
import_db "$LIVE" live "source IN ('hls','file') AND date(started_at_ms/1000,'unixepoch','localtime') NOT IN (${covered:-''})"

sqlite3 "$OUT" "
  -- remap runs.session_id to the new session ids
  UPDATE runs SET session_id = (SELECT id FROM sessions se WHERE se.src = runs.src AND se.src_id = runs.src_session);
  -- chronological attempt numbers
  UPDATE runs SET attempt_number = (
    SELECT COUNT(*) FROM runs r2 WHERE r2.game = runs.game AND r2.category = runs.category
      AND (r2.started_at_ms < runs.started_at_ms OR (r2.started_at_ms = runs.started_at_ms AND r2.id <= runs.id)));
  -- settings from the live db
  ATTACH '$LIVE' AS l; INSERT OR REPLACE INTO settings SELECT * FROM l.settings; DETACH l;
  ALTER TABLE runs DROP COLUMN src; ALTER TABLE runs DROP COLUMN src_id; ALTER TABLE runs DROP COLUMN src_session;
  ALTER TABLE sessions DROP COLUMN src; ALTER TABLE sessions DROP COLUMN src_id;
  VACUUM;"

./scripts/fill-run-numbers.sh "$OUT"
sqlite3 "$OUT" "SELECT 'sessions', COUNT(*) FROM sessions; SELECT 'runs', COUNT(*), SUM(outcome='finished'), SUM(ls_attempt IS NOT NULL) FROM runs; SELECT 'splits', COUNT(*) FROM splits;"

if [ "${1:-}" = "--swap" ]; then
  ts=$(date +%Y%m%d-%H%M%S)
  cp "$LIVE" "ninja-gaiden-pre-merge-$ts.db"
  mv "$OUT" "$LIVE"; rm -f "$LIVE-wal" "$LIVE-shm"
  echo "swapped in; previous live db saved as ninja-gaiden-pre-merge-$ts.db"
fi
