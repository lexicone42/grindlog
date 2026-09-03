#!/usr/bin/env bash
# Replay a window of a Twitch VOD through the bot and score the capture
# against the runner's own attempt counter — the regression test for any
# change to OCR, locking or detection. Baseline and candidate binaries run on
# identical frames, so their lines are directly comparable.
#
#   scripts/replay-window.sh <config.toml> <vod_id> <start_secs> <dur_secs> [binary] [label]
#
# Prints one line:
#   frames, % parsed, % legible while the timer is under 10s (the range every
#   attempt starts in), % of readings the tracker accepted (agreement with its
#   own clock, i.e. OCR accuracy), runs started in the window, how many carry
#   his run number and the span of those numbers (= attempts he really made),
#   lock events / poor-lock re-probes / crop growths.
#
# Work files land in replays/<label>/ (config, db, obs log, bot log), ignored
# by git. The config's own db/obs paths are overridden; everything else — the
# layouts, thresholds, fps — is what gets tested, so hand it the live config
# (or a variant of it) rather than a backfill one.
set -euo pipefail
cd "$(dirname "$0")/.."
cfg=$1; vod=$2; start=$3; dur=$4; bin=${5:-./target/release/ngtwitchtimer}; label=${6:-$(basename "$bin")-$vod-$start}
work=replays/$label; rm -rf "$work"; mkdir -p "$work"
sed -e "s|^source = .*|source = \"vod\"|" -e "s|^vod_id = .*|vod_id = \"$vod\"|" \
    -e "s|^start_secs = .*|start_secs = $start|" \
    -e "s|^path = .*|path = \"$work/db.sqlite\"|" -e "s|^obs_log = .*|obs_log = \"$work/obs.jsonl\"|" "$cfg" > "$work/cfg.toml"
grep -q '^source = "vod"' "$work/cfg.toml" || sed -i 's/^\[stream\]/[stream]\nsource = "vod"/' "$work/cfg.toml"
grep -q '^vod_id = ' "$work/cfg.toml" || sed -i "s/^\[stream\]/[stream]\nvod_id = \"$vod\"/" "$work/cfg.toml"
grep -q '^start_secs = ' "$work/cfg.toml" || sed -i "s/^\[stream\]/[stream]\nstart_secs = $start/" "$work/cfg.toml"
grep -q '^obs_log = ' "$work/cfg.toml" || printf '\n[debug]\nobs_log = "%s/obs.jsonl"\n' "$work" >> "$work/cfg.toml"
fps=$(grep -oE '^fps = [0-9]+' "$work/cfg.toml" | grep -oE '[0-9]+' || true); fps=${fps:-1}
want=$(( dur * fps ))
# Frame metrics cover exactly the window; the bot runs 12 more minutes of
# video so a run that STARTS inside the window and finishes after it is still
# written (rows are inserted when a run ends). Then it is stopped — it would
# otherwise run to the end of the VOD.
export OMP_THREAD_LIMIT=1
"$bin" --config "$work/cfg.toml" run > "$work/log" 2>&1 &
pid=$!
while kill -0 $pid 2>/dev/null; do
  have=0; [ -f "$work/obs.jsonl" ] && have=$(wc -l < "$work/obs.jsonl")
  [ "$have" -ge $(( want + 720 * fps )) ] && break
  sleep 2
done
kill $pid 2>/dev/null || true; wait $pid 2>/dev/null || true
sed -i 's/\x1b\[[0-9;]*m//g' "$work/log"
win=$(mktemp); head -n "$want" "$work/obs.jsonl" > "$win"
n=$(wc -l < "$win")
read -r parsed sub10 accepted < <(awk '
  /"parsed_ms":[0-9]/ { p++ }
  { fr++ }
  /"phase":"RUNNING"/ {
    if (match($0, /"smoothed_ms":[0-9]+/)) { sm = substr($0, RSTART+14, RLENGTH-14) + 0
      if (sm < 10000) { s10n++; if ($0 ~ /"parsed_ms":[0-9]/) s10p++ }
      if (match($0, /"parsed_ms":[0-9]+/)) { pm = substr($0, RSTART+12, RLENGTH-12) + 0; an++; d = pm - sm; if (d < 0) d = -d; if (d <= 500) ap++ } }
  }
  END { printf "%d %d%%of%d %d\n", (fr ? 100*p/fr : 0), (s10n ? 100*s10p/s10n : 0), s10n, (an ? 100*ap/an : 0) }' "$win")
base=$(sqlite3 "$work/db.sqlite" "select min(started_at_ms) from sessions" 2>/dev/null || echo 0)
ws=$(( base + 0 )); we=$(( base + dur*1000 ))
read -r runs numbered span < <(sqlite3 -separator ' ' "$work/db.sqlite" \
  "select count(*), count(ls_attempt), coalesce(max(ls_attempt)-min(ls_attempt)+1, 0) from runs where started_at_ms between $ws and $we" 2>/dev/null || echo "0 0 0")
locks=$(grep -cE 'layout (locked|switched|.* re-anchored)' "$work/log" || true)
poor=$(grep -c 'reads poorly' "$work/log" || true); grown=$(grep -c 'crop grown' "$work/log" || true)
printf "%-28s %5d fr  parsed %3d%%  <10s %-10s accepted %3d%%  window: %3d runs / %3d numbered / span %3d  locks %3d poor %2d grown %d\n" \
  "$label" "$n" "$parsed" "$sub10" "$accepted" "$runs" "$numbered" "$span" "${locks:-0}" "${poor:-0}" "${grown:-0}"
rm -f "$win"
