#!/usr/bin/env bash
# Wait for every backfill chain to finish and the streamer to be offline, then
# install the full chronological rebuild: stop the tmux-supervised live bot,
# merge-backfill --swap (keeps a ninja-gaiden-pre-merge-<ts>.db backup),
# restart the bot, deploy the site. Run detached:
#
#   nohup ./scripts/merge-when-done.sh /tmp/ng-backfill-a.log /tmp/ng-backfill-b.log ... > logs/merge-when-done.log 2>&1 &
set -uo pipefail
cd "$(dirname "$0")/.."
logs=("$@")
[ ${#logs[@]} -gt 0 ] || { echo "usage: $0 <chain log>..."; exit 1; }

all_done() {
  for l in "${logs[@]}"; do grep -q '=== backfill complete' "$l" 2>/dev/null || return 1; done
}
until all_done; do sleep 120; done
echo "all ${#logs[@]} chains complete at $(date -Is)"

# Offline = the live bot's last log line says so, or no frame for 15 minutes.
offline() {
  tail -1 logs/live.log 2>/dev/null | grep -q 'is offline' && return 0
  local last; last=$(stat -c %Y obs-live.jsonl 2>/dev/null || echo 0)
  [ $(( $(date +%s) - last )) -gt 900 ]
}
until offline; do sleep 120; done
echo "streamer offline at $(date -Is); merging"

tmux kill-session -t ngtimer 2>/dev/null && echo "live bot stopped"
sleep 3
if ./scripts/merge-backfill.sh --swap; then
  echo "merge installed"
else
  echo "!!! merge failed; live db untouched"
fi
tmux new-session -d -s ngtimer -c "$PWD" ./scripts/run-live.sh && echo "live bot restarted"
./scripts/deploy-site.sh
echo "done at $(date -Is)"
