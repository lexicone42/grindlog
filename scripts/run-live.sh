#!/usr/bin/env bash
# Supervised live bot: restarts on crash/exit, survives terminal and Claude
# sessions. Start it detached in tmux:
#
#   tmux new-session -d -s ngtimer ./scripts/run-live.sh
#
# Attach to watch: tmux attach -t ngtimer   (detach with Ctrl-b d)
# Stop for good:   tmux kill-session -t ngtimer
set -u
cd "$(dirname "$0")/.." || exit 1
mkdir -p logs
LOG=logs/live.log
while true; do
  echo "[wrapper] starting bot $(date -Is)" >> "$LOG"
  RUST_LOG=info ./target/release/ngtwitchtimer --config live.toml run >> "$LOG" 2>&1
  code=$?
  echo "[wrapper] bot exited (code $code) $(date -Is); restarting in 15s" >> "$LOG"
  sleep 15
done
