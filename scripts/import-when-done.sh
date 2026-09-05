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
# delete their backfill-db/vod-<id>.db* (rebackfill.sh does both and starts
# the chains and this script); otherwise the stale pass is imported and
# marked before the new chain gets there. A VOD whose analysis failed or was
# killed leaves its session open, never counts as complete, and keeps this
# loop waiting until it is re-run or the script is stopped. A VOD whose pass
# import-vod.sh's gate refuses (exit 3: thinner than the day it would
# replace) is logged once, not marked, and left alone unless its database
# changes (a re-run); it does not hold the loop open, which exits 3 with the
# refused ids once everything else is imported.
set -uo pipefail
cd "$(dirname "$0")/.."
ids=("$@")
[ ${#ids[@]} -gt 0 ] || { echo "usage: $0 <vod_id>..."; exit 1; }
mark=backfill-db/imported.txt
touch "$mark"
declare -A refused=()   # id -> mtime of its database when the gate refused it

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
    if [ -n "${refused[$id]:-}" ]; then
      # Refused by the gate: only a changed database (a re-run) earns a retry.
      [ "$(stat -c %Y "backfill-db/vod-$id.db" 2>/dev/null)" != "${refused[$id]}" ] || continue
      unset "refused[$id]"
    fi
    pending=$((pending+1))
    complete "$id" || continue
    echo "=== $(date -Is) importing vod $id"
    ./scripts/import-vod.sh "$id"; rc=$?
    case $rc in
      0) echo "$id" >> "$mark"; imported=$((imported+1));;
      3) refused[$id]=$(stat -c %Y "backfill-db/vod-$id.db" 2>/dev/null)
         echo "!!! import gate refused $id: thinner than the day in the live db; not marked imported." \
              "Re-run the VOD, or ./scripts/import-vod.sh $id --force; retried here only if its database changes";;
      *) echo "!!! import of $id failed (exit $rc); will retry";;
    esac
  done
  if [ "$imported" -gt 0 ]; then
    ./scripts/deploy-site.sh && echo "=== $(date -Is) site deployed ($(wc -l < "$mark") of ${#ids[@]} VODs imported)"
  fi
  if [ "$pending" -eq 0 ]; then
    if [ ${#refused[@]} -gt 0 ]; then
      echo "=== $(date -Is) $(( ${#ids[@]} - ${#refused[@]} )) of ${#ids[@]} VODs imported; the import gate refused: ${!refused[*]}"
      exit 3
    fi
    echo "=== $(date -Is) all ${#ids[@]} VODs imported"; exit 0
  fi
  sleep 120
done
