#!/usr/bin/env bash
# Replay a Big 20 practice broadcast into a scratch database, one VOD per run.
#
#   ./scripts/replay-big20.sh <vod_id> [vod_id...]
#
# A practice day is his ORDINARY LiveSplit pane — one game, a timer that
# resets, the run state machine turning each attempt into a run — pointed at
# a different game. So unlike a marathon day it needs no special layout and
# no board mode: it needs the LIVE config exactly as it is, with the stream
# redirected at a VOD. That is why this script overrides the config rather
# than writing one, the way scripts/replay-window.sh does: the layouts, the
# timer crop, the glyph templates and the thresholds are the deployment's,
# and a copy of them here would drift from it silently.
#
# What makes the runs land under the right game is `follow_title = "track"`
# (docs/detection.md, "Recording the other game"), which live.toml already
# sets — a board the identity gate convicts AND whose header names a game on
# a shipped roster has its runs recorded under that game and the roster's
# category. The gate is active whatever `follow_title` says, so a replay
# WITHOUT it records nothing on these days rather than recording them wrong;
# scripts/backfill-vods.sh is the one to reach for on a Ninja Gaiden day.
#
# Per VOD it writes, under $BIG20_OUT (default big20-db/):
#   vod-<id>.db        the runs and the session
#   obs-<id>.jsonl     the per-frame observation log
#   log-<id>.txt       the bot's own log
# A rerun replaces an earlier pass over the same VOD. Nothing here touches
# the live database or the running bot; land it with import-big20.sh.
#
# Starts at second 0 and watches the whole broadcast: unlike a marathon, an
# attempt is only ever recorded from the frame the timer starts, so there is
# nothing to be gained by joining late and a run in the first minute to lose.
#
# Environment: BIG20_OUT, BIG20_BIN (default target/release/ngtwitchtimer),
# BIG20_CFG (default live.toml), BIG20_START (seconds in, default 0),
# BIG20_NICE (default 15, so the live bot keeps the box).
set -uo pipefail
cd "$(dirname "$0")/.."
out=${BIG20_OUT:-big20-db}
bin=${BIG20_BIN:-./target/release/ngtwitchtimer}
src=${BIG20_CFG:-live.toml}
start=${BIG20_START:-0}
prio=${BIG20_NICE:-15}
[ -f "$src" ] || { echo "no config at $src" >&2; exit 1; }
[ -x "$bin" ] || { echo "no binary at $bin (scripts/build-release.sh)" >&2; exit 1; }
mkdir -p "$out"
# One OpenMP thread per worker: tesseract's threads only spin-wait on crops
# this small, and several workers on one box otherwise starve each other.
export OMP_THREAD_LIMIT=1 OMP_NUM_THREADS=1

# The replay must not be able to talk in his channel, and must not be able to
# reach the live database. Both are forced here rather than assumed of the
# config, because the config this reads IS the live one.
for id in "$@"; do
  cfg="$out/cfg-$id.toml"
  sed -e "s|^source = .*|source = \"vod\"|" \
      -e "s|^vod_id = .*|vod_id = \"$id\"|" \
      -e "s|^start_secs = .*|start_secs = $start|" \
      -e "s|^path = .*|path = \"$out/vod-$id.db\"|" \
      -e "s|^obs_log = .*|obs_log = \"$out/obs-$id.jsonl\"|" "$src" > "$cfg"
  for line in 'source = "vod"' "vod_id = \"$id\"" "start_secs = $start"; do
    key=${line%% =*}
    grep -q "^$key = " "$cfg" || sed -i "s|^\[stream\]|[stream]\n$line|" "$cfg"
  done
  grep -q '^path = ' "$cfg" || printf '\n[database]\npath = "%s/vod-%s.db"\n' "$out" "$id" >> "$cfg"
  grep -q '^obs_log = ' "$cfg" || printf '\n[debug]\nobs_log = "%s/obs-%s.jsonl"\n' "$out" "$id" >> "$cfg"
  # Chat off whatever the config says. Same awk as replay-window.sh.
  awk 'BEGIN{s=0} /^\[/{ if (s && !done) {print "enabled = false"; done=1}; s=($0=="[chat]") } { if (s && $0 ~ /^enabled *=/) {print "enabled = false"; done=1; next} print } END{ if (s && !done) print "enabled = false" }' \
      "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
  grep -q '^\[chat\]' "$cfg" || printf '\n[chat]\nenabled = false\n' >> "$cfg"
  # A replay that could write the live database would be the worst bug in
  # this repository. Refuse rather than trust the substitution above.
  if grep -qE '^path = .*(ninja-gaiden|live)\.db' "$cfg"; then
    echo "!!! $cfg still points at the live database; refusing" >&2; exit 1
  fi
  if ! grep -q '^follow_title = "track"' "$cfg"; then
    echo "!!! $src does not set follow_title = \"track\"; this replay would" >&2
    echo "    record nothing on a practice day (the gate suspends instead)" >&2
    exit 1
  fi

  echo "=== Big 20 practice VOD $id — $(date -Is) ==="
  rm -f "$out/vod-$id.db" "$out/vod-$id.db-wal" "$out/vod-$id.db-shm" \
        "$out/obs-$id.jsonl" "$out/log-$id.txt"
  if nice -n "$prio" "$bin" --config "$cfg" run > "$out/log-$id.txt" 2>&1; then
    sed -i 's/\x1b\[[0-9;]*m//g' "$out/log-$id.txt"
    sqlite3 "$out/vod-$id.db" \
      "SELECT 'VOD $id: '||COUNT(*)||' run(s) of '||COUNT(DISTINCT game)||' game(s): '
       ||GROUP_CONCAT(g,', ') FROM (SELECT game g, COUNT(*) n FROM runs GROUP BY game);" \
      2>/dev/null || echo "VOD $id: no runs"
  else
    echo "!!! VOD $id failed; see $out/log-$id.txt"
  fi
done
echo "=== big20 replay complete $(date -Is) ==="
