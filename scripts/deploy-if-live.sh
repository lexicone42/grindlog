#!/usr/bin/env bash
# Deploy the site while a live session is open, and once more after one
# closes — run from cron every 10 minutes to make ng.lexicone.com
# near-real-time during streams.
#
# The last game of a day (a Big 20 race's final among them) is filed at the
# broadcast's end, after the last tick that saw the session open. So a tick
# that finds no open hls session still deploys when an hls session closed
# within the last CLOSE_WINDOW_MIN minutes and no deploy has covered that
# close yet. The newest close a successful deploy covered is kept in STATE,
# so a close is deployed once: a later tick inside the window sees it
# handled. A failed deploy leaves STATE alone and the next tick tries again.
#
# Overrides, for testing without publishing anything:
#   DB=<sqlite db>          default ninja-gaiden.db
#   DEPLOY=<command>        default ./scripts/deploy-site.sh
#   STATE=<file>            default logs/deploy-if-live.last-close
#   NOW_MS=<epoch ms>       default the current time
#   CLOSE_WINDOW_MIN=<n>    default 20 (two ticks, so a failed deploy retries once)
set -euo pipefail
cd "$(dirname "$0")/.."
DB=${DB:-ninja-gaiden.db}
DEPLOY=${DEPLOY:-./scripts/deploy-site.sh}
STATE=${STATE:-logs/deploy-if-live.last-close}
NOW_MS=${NOW_MS:-$(($(date +%s) * 1000))}
CLOSE_WINDOW_MIN=${CLOSE_WINDOW_MIN:-20}

q() { sqlite3 "$DB" "$1" 2>/dev/null || true; }
open=$(q "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL AND source='hls'")
# The newest close, read before deploying: a session that closes while the
# deploy runs ends later than this and gets its own deploy on the next tick.
last_close=$(q "SELECT COALESCE(MAX(ended_at_ms), 0) FROM sessions WHERE ended_at_ms IS NOT NULL AND source='hls'")
[[ $open =~ ^[0-9]+$ ]] || open=0
[[ $last_close =~ ^[0-9]+$ ]] || last_close=0
handled=$(cat "$STATE" 2>/dev/null || echo 0)
[[ $handled =~ ^[0-9]+$ ]] || handled=0

if [ "$open" -gt 0 ]; then
  reason="live session open"
elif [ "$last_close" -gt "$handled" ] &&
  [ "$last_close" -ge $((NOW_MS - CLOSE_WINDOW_MIN * 60000)) ]; then
  reason="hls session closed $(((NOW_MS - last_close) / 60000)) min ago"
else
  exit 0
fi

# shellcheck disable=SC2086 # DEPLOY may carry arguments
$DEPLOY
mkdir -p "$(dirname "$STATE")"
echo "$last_close" >"$STATE"
# After the deploy's own "=== ... deploy start" marker, so healthcheck.sh
# reads it as part of this deploy's entry.
echo "deploy-if-live: deployed ($reason)"
