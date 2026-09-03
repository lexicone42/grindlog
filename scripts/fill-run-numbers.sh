#!/usr/bin/env bash
# Infer missing LiveSplit run numbers (runs.ls_attempt) from neighbors.
#
# The runner's attempt counter is strictly increasing by exactly one per
# attempt. So for a stretch of runs with unknown numbers sitting between two
# known ones, if (next_known - prev_known) == (runs in the gap + 1), every run
# in the gap gets an unambiguous number. Gaps that don't add up (his counter
# saw double-tap resets we never logged) are left alone.
#
#   ./scripts/fill-run-numbers.sh [db]      # default ninja-gaiden.db
set -euo pipefail
cd "$(dirname "$0")/.."
DB="${1:-ninja-gaiden.db}"
# On stdin with -bail so a failure stops and exits non-zero, and with a busy
# timeout because the live bot writes this database: without one the UPDATE
# fails instantly with "database is locked", prints what looks like a clean
# "filled|0", and takes the deploy down with it.
sqlite3 -bail "$DB" <<'SQL'
PRAGMA busy_timeout = 20000;
CREATE TEMP TABLE ord AS
  SELECT id, game, category, ls_attempt,
         ROW_NUMBER() OVER (PARTITION BY game, category ORDER BY started_at_ms, id) AS rn
  FROM runs;
CREATE TEMP TABLE fill AS
  SELECT o.id,
         p.ls_attempt + (o.rn - p.rn) AS inferred
  FROM ord o
  JOIN ord p ON p.game = o.game AND p.category = o.category AND p.ls_attempt IS NOT NULL
       AND p.rn = (SELECT MAX(rn) FROM ord x WHERE x.game = o.game AND x.category = o.category
                   AND x.ls_attempt IS NOT NULL AND x.rn < o.rn)
  JOIN ord n ON n.game = o.game AND n.category = o.category AND n.ls_attempt IS NOT NULL
       AND n.rn = (SELECT MIN(rn) FROM ord x WHERE x.game = o.game AND x.category = o.category
                   AND x.ls_attempt IS NOT NULL AND x.rn > o.rn)
  WHERE o.ls_attempt IS NULL
    AND n.ls_attempt - p.ls_attempt = n.rn - p.rn;
UPDATE runs SET ls_attempt = (SELECT inferred FROM fill WHERE fill.id = runs.id)
  WHERE id IN (SELECT id FROM fill);
SELECT 'filled', (SELECT COUNT(*) FROM fill);
SELECT 'coverage', SUM(ls_attempt IS NOT NULL) || '/' || COUNT(*) FROM runs;
SQL
