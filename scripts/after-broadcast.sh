#!/usr/bin/env bash
# After a Big 20 practice broadcast: wait for the live session to close,
# optionally roll `main` out while he is off the air, then replay the day's
# VOD once Twitch has finished writing it, and say how to land it.
#
#   scripts/after-broadcast.sh [--rollout] [--now] [--out <dir>] [--channel <login>]
#
# The wait is for the SESSION to close, not for the run to end: on 2026-09-25
# a watcher keyed on the tracker's "marathon over" line replayed the VOD
# while he was still live practising Moon Crystal, six hours before the VOD
# was finished, and got an empty pass. A VOD replay needs the whole
# recording, so this also waits until the VOD Twitch lists for today stops
# growing (its length the same on two polls two minutes apart).
#
#   --rollout      run scripts/rollout.sh once the session has closed (it
#                  builds a clean main, so merge first)
#   --now          do not wait for a session to close (the stream is over)
#   --out <dir>    the replay's output directory (default big20-db-<today>);
#                  the import command printed at the end uses it
#   --channel <c>  the channel to look the VOD up on (default: [stream]
#                  channel in live.toml)
#   --dry-run      stop before the replay, having said which VOD it would be
#
# Environment: NG_DB (default ninja-gaiden.db); AFTER_POLL_SECS (default 120)
# is how often the VOD listing is polled. Everything it prints is worth
# keeping: run it detached with its output to a file.
set -u
cd "$(dirname "$0")/.." || exit 1
DB=${NG_DB:-ninja-gaiden.db}
rollout=0 now=0 dry=0 out="" channel=""
poll=${AFTER_POLL_SECS:-120}
while [ $# -gt 0 ]; do
  case "$1" in
    --rollout) rollout=1;;
    --now) now=1;;
    --dry-run) dry=1;;
    --out) out=$2; shift;;
    --channel) channel=$2; shift;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
  shift
done
[ -n "$channel" ] || channel=$(grep -E '^[[:space:]]*channel[[:space:]]*=' live.toml | head -1 | sed 's/.*=[[:space:]]*"\([^"]*\)".*/\1/')
[ -n "$channel" ] || { echo "no channel: pass --channel or set [stream] channel in live.toml" >&2; exit 2; }
today=$(date +%Y-%m-%d)
[ -n "$out" ] || out="big20-db-$today"
open() { sqlite3 -readonly -cmd '.timeout 10000' "$DB" "select count(*) from sessions where source='hls' and ended_at_ms is null" 2>/dev/null || echo 1; }
say() { echo "=== $(date -Is) $*"; }

if [ "$now" = 0 ]; then
  say "waiting for the live session to close"
  while [ "$(open)" != 0 ]; do sleep 60; done
fi
say "no live session open"

if [ "$rollout" = 1 ]; then
  say "rollout from $(git branch --show-current) $(git log --oneline -1 | cut -c1-70)"
  ./scripts/rollout.sh 2>&1 | grep -E '^==|test result|rollout-smoke|new pid|error|refus|smoke' | cut -c1-160
fi

# Today's VOD, and only once its length has stopped growing. list-vods
# prints "id  date  hours  title"; a broadcast still being archived grows
# by a tenth of an hour every six minutes.
say "waiting for today's VOD on $channel to be listed and finished"
id="" last="" n=0
while :; do
  line=$(./scripts/list-vods.sh "$channel" 2>/dev/null | awk -v d="$today" '$2 == d {print; exit}')
  if [ -n "$line" ]; then
    id=$(echo "$line" | awk '{print $1}')
    hours=$(echo "$line" | awk '{print $3}')
    if [ "$hours" = "$last" ]; then
      break
    fi
    last=$hours
  fi
  n=$((n + 1))
  if [ "$n" -gt 120 ]; then
    say "gave up: no finished VOD for $today after four hours"
    exit 1
  fi
  sleep "$poll"
done
say "vod $id ($last h): replaying into $out"
[ "$dry" = 0 ] || { echo "=== dry run: BIG20_OUT=$out ./scripts/replay-big20.sh $id"; exit 0; }
export OMP_THREAD_LIMIT=1
BIG20_OUT=$out ./scripts/replay-big20.sh "$id" 2>&1 | tail -3
rows=$(sqlite3 -readonly "$out/vod-$id.db" "select count(*) from runs where category like '%run%'" 2>/dev/null)
say "replayed: ${rows:-?} race-board row(s)"
echo "=== import with: BIG20_OUT=$out ./scripts/import-big20.sh --replace-live --deploy $id"
