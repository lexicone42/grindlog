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
# One OpenMP thread: tesseract's threads only spin-wait on crops this small.
export OMP_THREAD_LIMIT=1 OMP_NUM_THREADS=1
mkdir -p logs
LOG=logs/live.log
# Rotate the unbounded logs when they grow past ~100MB — checked before every
# bot start, so a rollout or a crash is enough to rotate; the supervisor
# itself runs for months. (An earlier version checked once, at supervisor
# start, and never again.)
rotate() {
  for f in obs-live.jsonl "$LOG"; do
    if [ -f "$f" ] && [ "$(stat -c%s "$f")" -gt 104857600 ]; then
      old="$f.$(date +%Y%m%d-%H%M).old"
      mv "$f" "$old" && gzip -f "$old" || true
    fi
  done
}
while true; do
  rotate
  echo "[wrapper] starting bot $(date -Is)" >> "$LOG"
  RUST_LOG=info ./target/release/ngtwitchtimer --config live.toml run >> "$LOG" 2>&1
  code=$?
  echo "[wrapper] bot exited (code $code) $(date -Is); restarting in 15s" >> "$LOG"
  sleep 15
done
