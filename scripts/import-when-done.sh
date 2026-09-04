#!/usr/bin/env bash
# Incrementally rebuild the live database from the backfill: every couple of
# minutes, import each per-VOD database that has completed since the last
# pass (see import-vod.sh) and redeploy the site after new imports. Exits
# once every VOD listed has been imported. Run detached:
#
#   nohup ./scripts/import-when-done.sh <vod_id>... > logs/import-when-done.log 2>&1 &
#
# Imported ids are remembered in backfill-db/imported.txt, so it can be
# restarted freely. But "complete" means any per-VOD database with a closed
# session and no open one, whatever its age, and backfill-vods.sh only
# deletes a VOD's old database when its chain reaches that VOD. So before
# re-running VODs already imported, strip their ids from imported.txt AND
# delete their backfill-db/vod-<id>.db*; otherwise the stale pass is imported
# and marked before the new chain gets there. A VOD whose analysis failed or
# was killed leaves its session open, never counts as complete, and keeps
# this loop waiting until it is re-run or the script is stopped.
set -uo pipefail
cd "$(dirname "$0")/.."
ids=("$@")
[ ${#ids[@]} -gt 0 ] || { echo "usage: $0 <vod_id>..."; exit 1; }
mark=backfill-db/imported.txt
touch "$mark"

complete() { # vod_id -> 0 if its db has a closed session and no open one
  local f="backfill-db/vod-$1.db"
  [ -f "$f" ] || return 1
  local open done_
  open=$(sqlite3 "$f" "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL" 2>/dev/null) || return 1
  done_=$(sqlite3 "$f" "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NOT NULL" 2>/dev/null) || return 1
  [ "$open" = "0" ] && [ "${done_:-0}" -gt 0 ]
}

while :; do
  imported=0 pending=0
  for id in "${ids[@]}"; do
    grep -qx "$id" "$mark" && continue
    pending=$((pending+1))
    complete "$id" || continue
    echo "=== $(date -Is) importing vod $id"
    if ./scripts/import-vod.sh "$id"; then
      echo "$id" >> "$mark"
      imported=$((imported+1))
    else
      echo "!!! import of $id failed; will retry"
    fi
  done
  if [ "$imported" -gt 0 ]; then
    ./scripts/deploy-site.sh && echo "=== $(date -Is) site deployed ($(wc -l < "$mark") of ${#ids[@]} VODs imported)"
  fi
  [ "$pending" -eq 0 ] && { echo "=== $(date -Is) all ${#ids[@]} VODs imported"; exit 0; }
  sleep 120
done
