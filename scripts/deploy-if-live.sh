#!/usr/bin/env bash
# Deploy the site while a live session is open, and once more after one
# closes — run from cron every 10 minutes to make ng.lexicone.com
# near-real-time during streams.
#
# The last game of a day (a Big 20 race's final among them) is filed at the
# broadcast's end, after the last tick that saw the session open. So a tick
# that finds no open hls session still deploys when an hls session closed
# within the last CLOSE_WINDOW_MIN minutes and no deploy has covered that
# close yet. And it deploys when a run has been filed since the last deploy
# it made, whenever that is: a race tracker set aside when the stream went
# offline is closed, and a finish on its last pass filed, only when its
# board has stayed away for 15 minutes (app.rs, MarathonKeep), after the
# tick that deployed the close. Run ids only grow, so the newest one a
# deploy covered says what is new. The newest close and the newest run a
# successful deploy covered are kept in STATE ("<close ms> <run id>"), so
# each is deployed once. A failed deploy leaves STATE alone and the next
# tick tries again.
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
last_run=$(q "SELECT COALESCE(MAX(id), 0) FROM runs")
[[ $open =~ ^[0-9]+$ ]] || open=0
[[ $last_close =~ ^[0-9]+$ ]] || last_close=0
[[ $last_run =~ ^[0-9]+$ ]] || last_run=0
read -r handled handled_run <"$STATE" 2>/dev/null || true
[[ ${handled:-} =~ ^[0-9]+$ ]] || handled=0
# A state file from before runs were tracked: what it covered is unknown,
# and the newest run is taken as covered rather than deploying for it.
[[ ${handled_run:-} =~ ^[0-9]+$ ]] || handled_run=$last_run

if [ "$open" -gt 0 ]; then
  reason="live session open"
elif [ "$last_close" -gt "$handled" ] &&
  [ "$last_close" -ge $((NOW_MS - CLOSE_WINDOW_MIN * 60000)) ]; then
  reason="hls session closed $(((NOW_MS - last_close) / 60000)) min ago"
elif [ "$last_run" -gt "$handled_run" ]; then
  reason="$((last_run - handled_run)) run(s) filed since the last deploy"
else
  exit 0
fi

# shellcheck disable=SC2086 # DEPLOY may carry arguments
$DEPLOY
mkdir -p "$(dirname "$STATE")"
echo "$last_close $last_run" >"$STATE"
# After the deploy's own "=== ... deploy start" marker, so healthcheck.sh
# reads it as part of this deploy's entry.
echo "deploy-if-live: deployed ($reason)"
