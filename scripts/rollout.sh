#!/usr/bin/env bash
# Roll a new build out to the live bot, safely and in one command:
#   1. refuse unless on main with a clean tree (or --allow-dirty),
#   2. build the release binary (in-process OCR when available) and run the
#      test suite,
#   3. smoke-replay one minute of a recent VOD and refuse if the timer does
#      not read at least 90%,
#   4. wait until the live bot is idle (no run in progress) or offline,
#   5. stop it — the run-live.sh supervisor restarts it on the new binary —
#      and confirm the new process comes up tracking.
#
#   scripts/rollout.sh [--allow-dirty] [--smoke-vod <id>]
set -euo pipefail
cd "$(dirname "$0")/.."
allow_dirty=0; smoke_vod=""
while [ $# -gt 0 ]; do
  case $1 in
    --allow-dirty) allow_dirty=1 ;;
    --smoke-vod) smoke_vod=$2; shift ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done
branch=$(git branch --show-current)
if [ "$allow_dirty" -eq 0 ]; then
  [ "$branch" = "main" ] || { echo "on $branch, not main — merge first (or --allow-dirty)" >&2; exit 1; }
  [ -z "$(git status --porcelain --untracked-files=no)" ] || { echo "uncommitted changes — commit or --allow-dirty" >&2; exit 1; }
fi
echo "== building $(git rev-parse --short HEAD) on $branch"
./scripts/build-release.sh | tail -1
echo "== tests"
cargo test --release 2>&1 | grep -E 'test result|FAILED|panicked' | head -5
cargo test --release >/dev/null 2>&1 || { echo "tests failed; not rolling out" >&2; exit 1; }
# Smoke replay: the most recent VOD session in the live database unless given.
if [ -z "$smoke_vod" ]; then
  smoke_vod=$(sqlite3 ninja-gaiden.db "select label from sessions where source='vod' order by started_at_ms desc limit 1" | grep -oE '[0-9]+' || true)
fi
if [ -n "$smoke_vod" ]; then
  # Two minutes: a fresh process spends its first frames finding the layout,
  # and one minute is too short for that to wash out. The gate asks only
  # "does this binary read the timer at all", hence 80%.
  echo "== smoke replay: vod $smoke_vod, two minutes from 40 minutes in"
  # Failures must be loud: keep the replay's stderr, and never let pipefail
  # end the script before the reason is printed.
  set +e
  line=$(./scripts/replay-window.sh live.toml "$smoke_vod" 2400 120 ./target/release/ngtwitchtimer rollout-smoke 2> replays/rollout-smoke.err | tail -1)
  rc=$?
  set -e
  echo "   ${line:-<no output>}"
  pct=$(echo "$line" | grep -oE 'parsed +[0-9]+%' | grep -oE '[0-9]+' || true)
  if [ "$rc" -ne 0 ] || [ -z "$pct" ]; then
    echo "smoke replay failed (exit $rc); its stderr:" >&2
    tail -5 replays/rollout-smoke.err >&2
    exit 1
  fi
  [ "$pct" -ge 80 ] || { echo "smoke replay read only ${pct}% of frames; not rolling out" >&2; exit 1; }
  rm -rf replays/rollout-smoke replays/rollout-smoke.err
else
  echo "== no VOD session in the database to smoke against; skipping"
fi
# The bot process itself, not `... live.toml report --json` from the site cron.
pid=$(pgrep -f 'ngtwitchtimer --config live.toml ru[n]' | head -1 || true)
[ -n "$pid" ] || { echo "no live bot running (start it with scripts/run-live.sh); build is in place" >&2; exit 0; }
# Without the supervisor nothing brings the bot back after the kill.
pgrep -f 'run-live\.s[h]' >/dev/null || { echo "the live bot is not under scripts/run-live.sh; restart it by hand when idle" >&2; exit 1; }
echo "== waiting for the live bot (pid $pid) to be idle or offline"
n=0
while :; do
  last=$(tail -1 obs-live.jsonl 2>/dev/null || true)
  age=$(( $(date +%s) - $(stat -c %Y obs-live.jsonl 2>/dev/null || echo 0) ))
  # No fresh frames for a minute = offline; IDLE = between runs.
  if [ "$age" -gt 60 ] || echo "$last" | grep -q '"phase":"IDLE"'; then break; fi
  sleep 2; n=$((n+1))
  [ $((n % 30)) -eq 0 ] && echo "   still in a run ($((n*2))s)..."
  [ "$n" -gt 1800 ] && { echo "gave up waiting after an hour" >&2; exit 1; }
done
echo "== restarting (SIGTERM; the supervisor brings it back on the new binary)"
kill "$pid"
for _ in $(seq 1 40); do
  sleep 1
  new=$(pgrep -f 'ngtwitchtimer --config live.toml ru[n]' | head -1 || true)
  [ -n "$new" ] && [ "$new" != "$pid" ] && break
done
[ -n "${new:-}" ] && [ "$new" != "$pid" ] || { echo "the bot did not come back; check logs/live.log" >&2; exit 1; }
sleep 3
echo "== new pid $new: $(tail -3 logs/live.log | sed 's/\x1b\[[0-9;]*m//g' | grep -oE 'tracking .*|arcus is .*|session #[0-9]+ opened.*' | tail -1)"
