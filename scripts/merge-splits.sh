#!/usr/bin/env bash
# Add the splits a re-analysed pass found to the live runs that lack them,
# and nothing else: no run is added, changed or deleted, no day replaced.
#
#   LIVE=<db> scripts/merge-splits.sh <pass.db> [--dry-run]
#
# A pass over an old day (scripts/reanalyse-archive.sh) reads the splits a
# newer binary can (a split that ties its comparison, recorded at the
# run's end), but over a whole day it can also lose what the live capture
# had: a finish on an older theme, a marathon the same afternoon from
# another VOD. import-vod.sh would take the losses with the gains;
# compare-pass.sh says which. This takes only the gain: for each live run
# with fewer splits than the pass's run of it (same game, category and
# outcome, its last timer reading within a second, its start within 15 s:
# the clocks differ by a steady few seconds, and he resets several runs
# inside half a minute, so the start alone matched one pass run to several
# live ones), the pass's splits replace its own. LIVE defaults to the
# live database: rehearse with LIVE=<a copy> first.
set -euo pipefail
cd "$(dirname "$0")/.."
pass=$1
live=${LIVE:-ninja-gaiden.db}
dry=${2:-}
[ -f "$pass" ] || { echo "no pass at $pass" >&2; exit 1; }
# One pass run per live run and one live run per pass run: the nearest
# start of the candidates either way (quick resets alike to the second
# otherwise pair crosswise).
pairs="
  SELECT live_id, pass_id, have, get FROM (
    SELECT l.id AS live_id, q.id AS pass_id,
           (SELECT COUNT(*) FROM main.splits s WHERE s.run_id = l.id) AS have,
           (SELECT COUNT(*) FROM p.splits s WHERE s.run_id = q.id) AS get,
           ROW_NUMBER() OVER (PARTITION BY l.id ORDER BY abs(l.started_at_ms - q.started_at_ms)) AS rl,
           ROW_NUMBER() OVER (PARTITION BY q.id ORDER BY abs(l.started_at_ms - q.started_at_ms)) AS rq
      FROM main.runs l JOIN p.runs q
        ON abs(l.started_at_ms - q.started_at_ms) < 15000
       AND abs(l.last_timer_ms - q.last_timer_ms) <= 1000
       AND l.outcome = q.outcome AND l.game = q.game AND l.category = q.category)
   WHERE rl = 1 AND rq = 1"
sqlite3 "$live" "ATTACH '$pass' AS p;
  SELECT COUNT(*) || ' run(s) gain splits: ' || IFNULL(SUM(get - have), 0) || ' split(s) in all'
    FROM ($pairs) WHERE get > have;"
[ "$dry" = "--dry-run" ] && exit 0
sqlite3 "$live" "ATTACH '$pass' AS p;
  BEGIN;
  CREATE TEMP TABLE gain AS SELECT live_id, pass_id FROM ($pairs) WHERE get > have;
  DELETE FROM main.splits WHERE run_id IN (SELECT live_id FROM gain);
  INSERT INTO main.splits (run_id, act_index, act_name, cumulative_ms, segment_ms)
    SELECT g.live_id, s.act_index, s.act_name, s.cumulative_ms, s.segment_ms
      FROM gain g JOIN p.splits s ON s.run_id = g.pass_id;
  COMMIT;
  SELECT 'merged into ' || (SELECT COUNT(*) FROM gain) || ' run(s)';"
