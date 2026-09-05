#!/usr/bin/env bash
# Re-run VODs through the backfill the way CLAUDE.md's Operating facts tell a
# human to: park each VOD's earlier pass (backfill-db/vod-<id>.db* and
# backfill-logs/obs-<id>.jsonl) in a dated archive directory, strip the ids
# from backfill-db/imported.txt so import-when-done.sh cannot re-import the
# stale pass and mark it, then start detached backfill-vods.sh chains and one
# detached import-when-done.sh over all of the ids.
#
#   ./scripts/rebackfill.sh [--chains N] [--label L] [--dry-run] <vod_id>...
#
# The ids are split into N contiguous slices (default 1) in the order given,
# so list them in the order you want them processed: chronological, or the
# day you are waiting for first. Chain n logs to /tmp/ng-backfill-<label>-<n>.log
# and the importer to logs/import-when-done-<label>.log; the label defaults to
# a YYYYMMDD-HHMM stamp. The earlier passes go to backfill-db/rerun-<stamp>/
# and backfill-logs/rerun-<stamp>/ with a copy of imported.txt as it was, so
# a rerun is reversible. Prints the chain pids and log paths; the processes
# outlive this shell (setsid + nohup). Refuses ids a running backfill-vods.sh
# chain or ngtwitchtimer backfill worker is processing (a rerun would fight
# it for the same database), ids that are not numbers, duplicates, and a
# missing target/release/ngtwitchtimer. `--dry-run` prints every move and
# launch and does none of it. Assumes backfill-vods.sh's layout (backfill-db/,
# backfill-logs/, the release binary) and never touches the live database
# itself: the days land through import-vod.sh, whose gate refuses a pass
# thinner than the day it would replace.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() { echo "usage: $0 [--chains N] [--label L] [--dry-run] <vod_id>..."; }
chains=1 label="" dry=false ids=()
while [ $# -gt 0 ]; do
  case "$1" in
    --chains) chains="${2:?--chains needs a number}"; shift 2;;
    --chains=*) chains="${1#--chains=}"; shift;;
    --label) label="${2:?--label needs a name}"; shift 2;;
    --label=*) label="${1#--label=}"; shift;;
    --dry-run|-n) dry=true; shift;;
    -h|--help) usage; exit 0;;
    -*) echo "unknown option $1" >&2; usage >&2; exit 1;;
    *) ids+=("$1"); shift;;
  esac
done
[ ${#ids[@]} -gt 0 ] || { usage >&2; exit 1; }
[[ "$chains" =~ ^[1-9][0-9]*$ ]] || { echo "--chains must be a positive number, not '$chains'" >&2; exit 1; }
stamp=$(date +%Y%m%d-%H%M%S)
label=${label:-${stamp%??}}
[[ "$label" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "--label may only use letters, digits, - and _" >&2; exit 1; }
for id in "${ids[@]}"; do
  [[ "$id" =~ ^[0-9]+$ ]] || { echo "not a VOD id: $id" >&2; exit 1; }
done
dups=$(printf '%s\n' "${ids[@]}" | sort | uniq -d | paste -sd ' ')
[ -z "$dups" ] || { echo "duplicate ids: $dups" >&2; exit 1; }
if [ ! -x target/release/ngtwitchtimer ]; then
  echo "no target/release/ngtwitchtimer (scripts/build-release.sh); every VOD would fail in seconds" >&2
  $dry || exit 1
fi

# The ids running chains and workers are on: a chain is
# `bash <path>/backfill-vods.sh <ids...>` (all of its ids count, the ones it
# has not reached yet included: it wipes each VOD's database when it gets
# there), a worker is `ngtwitchtimer --config /tmp/ngbackfill-XXXXXX.toml run`
# whose config names its vod_id (this also catches a worker whose chain is
# gone). /proc is read directly so a shell that merely mentions the script
# name on its command line is not mistaken for a chain.
busy_ids() {
  local p a args
  for p in $(pgrep -f 'backfill-vods\.s[h]' || true); do
    [ -r "/proc/$p/cmdline" ] || continue
    mapfile -d '' args < "/proc/$p/cmdline"
    [ "$(basename -- "${args[1]:-}")" = backfill-vods.sh ] || continue
    printf '%s\n' "${args[@]:2}"
  done
  for p in $(pgrep -f 'ngtwitchtimer --config /tmp/ngbackfil[l]' || true); do
    [ -r "/proc/$p/cmdline" ] || continue
    mapfile -d '' args < "/proc/$p/cmdline"
    for a in "${args[@]}"; do
      case "$a" in /tmp/ngbackfill-*.toml) grep -oE '^vod_id = "[0-9]+"' "$a" 2>/dev/null | grep -oE '[0-9]+' || true;; esac
    done
  done
}
busy=$(busy_ids | sort -u)
clash=$(comm -12 <(printf '%s\n' "${ids[@]}" | sort -u) <(printf '%s\n' "$busy") | paste -sd ' ')
if [ -n "$clash" ]; then
  echo "refusing: a running backfill chain is already processing $clash" >&2
  echo "(pgrep -af 'backfill-vods\\.s[h]' lists the chains; kill a chain before its workers, or wait)" >&2
  exit 1
fi
# A running importer that lists some of these ids will import them too once
# their new pass completes: harmless (import-vod.sh replaces the same day
# with the same rows) but worth knowing about.
for p in $(pgrep -f 'import-when-done\.s[h]' || true); do
  [ -r "/proc/$p/cmdline" ] || continue
  mapfile -d '' args < "/proc/$p/cmdline"
  [ "$(basename -- "${args[1]:-}")" = import-when-done.sh ] || continue
  shared=$(comm -12 <(printf '%s\n' "${ids[@]}" | sort -u) <(printf '%s\n' "${args[@]:2}" | sort -u) | paste -sd ' ')
  [ -z "$shared" ] || echo "note: import-when-done.sh pid $p also lists $shared and will import the new pass as well"
done

run() { if $dry; then echo "would: $*"; else "$@"; fi; }

# 1. Park the earlier passes.
arch_db=backfill-db/rerun-$stamp arch_log=backfill-logs/rerun-$stamp
mark=backfill-db/imported.txt
old=()
for id in "${ids[@]}"; do
  for f in "backfill-db/vod-$id.db" "backfill-db/vod-$id.db-wal" "backfill-db/vod-$id.db-shm" "backfill-logs/obs-$id.jsonl"; do
    [ -e "$f" ] && old+=("$f")
  done
done
marked=()
if [ -f "$mark" ]; then
  mapfile -t marked < <(grep -xF -f <(printf '%s\n' "${ids[@]}") "$mark" || true)
fi
if [ ${#old[@]} -gt 0 ] || [ ${#marked[@]} -gt 0 ]; then
  run mkdir -p "$arch_db" "$arch_log"
  for f in "${old[@]}"; do
    case "$f" in backfill-db/*) run mv "$f" "$arch_db/";; *) run mv "$f" "$arch_log/";; esac
  done
  if [ ${#marked[@]} -gt 0 ]; then
    run cp "$mark" "$arch_db/imported.txt"
    if $dry; then
      echo "would: strip ${marked[*]} from $mark"
    else
      grep -vxF -f <(printf '%s\n' "${ids[@]}") "$mark" > "$mark.tmp" || true
      mv "$mark.tmp" "$mark"
    fi
  fi
  echo "earlier passes: ${#old[@]} file(s) moved to $arch_db/ and $arch_log/, ${#marked[@]} id(s) stripped from $mark"
else
  echo "earlier passes: none on disk, nothing to archive"
fi

# 2. Launch the chains over contiguous slices of the ids, then the importer.
n=${#ids[@]}
[ "$chains" -le "$n" ] || { echo "only $n id(s): running $n chain(s), not $chains"; chains=$n; }
mkdir -p logs
start=0
for ((i = 1; i <= chains; i++)); do
  size=$(( n / chains + (i <= n % chains ? 1 : 0) ))
  slice=("${ids[@]:start:size}"); start=$((start + size))
  log=/tmp/ng-backfill-$label-$i.log
  if $dry; then
    echo "would: setsid nohup ./scripts/backfill-vods.sh ${slice[*]} > $log"
    continue
  fi
  setsid nohup ./scripts/backfill-vods.sh "${slice[@]}" > "$log" 2>&1 < /dev/null &
  pid=$!
  sleep 0.2
  kill -0 "$pid" 2>/dev/null && state=running || state="EXITED already, see the log"
  echo "chain $i: pid $pid ($state) over ${slice[*]}"
  echo "         log $log"
done
ilog=logs/import-when-done-$label.log
if $dry; then
  echo "would: setsid nohup ./scripts/import-when-done.sh ${ids[*]} > $ilog"
else
  setsid nohup ./scripts/import-when-done.sh "${ids[@]}" > "$ilog" 2>&1 < /dev/null &
  echo "importer: pid $! over all $n id(s), log $ilog"
fi
