#!/usr/bin/env bash
# Start the live bot's supervisor (scripts/run-live.sh) in tmux on a server
# of its own, the socket "grindlog". A plain `tmux` or `tmux attach` talks
# to the default server and never lands in the bot's session; after a
# reboot the bot's session was the only one on the default server, every
# new terminal attached to it, and the bot's window got closed by hand.
#
#   ./scripts/start-supervisor.sh    start it, or say it is running
#
# Watch:  tmux -L grindlog attach -t ngtimer    (Ctrl-b d detaches)
# Stop:   tmux -L grindlog kill-session -t ngtimer
#
# A session named ngtimer on the DEFAULT server (an older crontab) is
# retired first, bot and all, or two supervisors fight over the database:
# run this between runs, or after the stream.
set -u
cd "$(dirname "$0")/.." || exit 1
if tmux -L grindlog has-session -t =ngtimer 2>/dev/null && pgrep -f 'scripts/run-live\.s[h]' >/dev/null; then
  echo "supervisor already running: tmux -L grindlog attach -t ngtimer"
  exit 0
fi
if tmux has-session -t =ngtimer 2>/dev/null; then
  echo "retiring the ngtimer session on the default tmux server"
  tmux kill-session -t =ngtimer
  for _ in $(seq 1 20); do
    pgrep -f 'scripts/run-live\.s[h]' >/dev/null || break
    sleep 0.5
  done
  pkill -f 'scripts/run-live\.s[h]' 2>/dev/null || true
  pkill -f 'ngtwitchtimer --config live\.toml ru[n]' 2>/dev/null || true
fi
tmux -L grindlog new-session -d -s ngtimer -c "$PWD" "$PWD/scripts/run-live.sh"
echo "started: tmux -L grindlog attach -t ngtimer"
