#!/usr/bin/env bash
# Replay a window of a Twitch VOD through the bot and score the capture
# against the runner's own attempt counter — the regression test for any
# change to OCR, locking or detection. Baseline and candidate binaries run on
# identical frames, so their lines are directly comparable.
#
#   scripts/replay-window.sh <config.toml> <vod_id> <start_secs> <dur_secs> [binary] [label]
#   scripts/replay-window.sh <config.toml> <window.mp4> <start_secs> <dur_secs> [binary] [label]
#
# The second form replays a local file instead of streaming the VOD: when the
# second argument is an existing .mp4/.mkv path the replay config gets
# `source = "file"`, `input = <path>` and a `start_secs` relative to the
# file. Pinned windows under vods/windows/ are named `<label>-<vod>-<start>.mp4`
# and begin at second <start> of their VOD, so <start_secs> is still given on
# the VOD's timeline and a request for that <start> maps to 0 in the file
# (asking for a moment before the file begins is an error); a file whose
# name does not end in -<digits>.<ext> is taken to begin at second 0. The
# label defaults to <binary>-<file stem>-<start_secs>. Runs of a file replay
# are dated from a placeholder epoch, 2000-01-01 00:00 UTC standing for
# second 0 of the VOD (the bot cannot ask Twitch when a file was broadcast,
# and without a base it would date every run today), so the window count
# below still works and the replay database reads as offsets into the
# broadcast; the config's own `recorded_start` is replaced. Two replays of
# the same file are frame-aligned and diff frame by frame
# (scripts/obs-diff.sh); whether a file replay aligns with a STREAMED replay
# of the same window depends on how the file was cut. The pinned windows
# were cut by an accurate ffmpeg seek (`-ss <start>`, re-encoded) from the
# same HLS rendition the bot streams, and a26-2857064807-2000.mp4 replayed
# frame-identically to a streamed replay of 2857064807 from 2000 s (5040
# frames, no differing field); a file cut with `-c copy` starts at the
# segment boundary before <start>, up to ten seconds early, and a file from
# another rendition or tool aligns with nothing but itself. Check before
# reading a file-vs-stream diff frame by frame: a shift shows in obs-diff's
# summary as nearly every frame differing. The summary lines compare either
# way.
#
# Prints one line:
#   frames, % parsed, % legible while the timer is under 10s (the range every
#   attempt starts in), % of readings the tracker accepted (agreement with its
#   own clock, i.e. OCR accuracy), runs started in the window, how many carry
#   his run number and the span of those numbers (= attempts he really made),
#   lock events / poor-lock re-probes (the trailing "grown" column counted
#   crop auto-grow events, a feature since removed; it is always 0 now).
#
# Work files land in replays/<label>/ (config, db, obs log, bot log), ignored
# by git; a rerun with the same label replaces the last capture. The config's
# own db/obs paths are overridden and chat is forced off whatever it says, so
# hand it the live config (or a variant of it) rather than a backfill one:
# everything else — the layouts, thresholds, fps — is what gets tested. The
# bot runs 12 more minutes of video past the window (so a run that starts in
# it and ends after it is still written) and is then killed. Bounded: no
# frame within two minutes, or the window not done within its length plus 15
# minutes, exits 1 with the reason on stderr.
set -euo pipefail
cd "$(dirname "$0")/.."
cfg=$1; vod=$2; start=$3; dur=$4; bin=${5:-./target/release/ngtwitchtimer}
# A local recording in place of the VOD id (paths are taken from the repo
# root, like the config's). vods/windows/<label>-<vod>-<start>.mp4 begins at
# second <start> of its VOD, so the requested start is mapped into the file.
file=
case $vod in
  *.mp4|*.mkv|*.MP4|*.MKV)
    [ -f "$vod" ] || { echo "no such file: $vod" >&2; exit 2; }
    case $vod in /*) file=$vod;; *) file=$PWD/$vod;; esac
    stem=$(basename "$file"); stem=${stem%.*}
    fstart=$(printf '%s' "$stem" | sed -nE 's/^.*-([0-9]+)$/\1/p'); fstart=${fstart:-0}
    if [ "$start" -lt "$fstart" ]; then
      echo "the window starts at ${start}s but $stem begins at second $fstart of the VOD" >&2; exit 2
    fi
    ;;
esac
if [ -n "$file" ]; then label=${6:-$(basename "$bin")-$stem-$start}; else label=${6:-$(basename "$bin")-$vod-$start}; fi
work=replays/$label; rm -rf "$work"; mkdir -p "$work"
if [ -n "$file" ]; then
  fsecs=$(( start - fstart ))
  # Placeholder timeline: 2000-01-01T00:00:00Z (946684800) stands for second
  # 0 of the VOD, so the replay's runs are dated by their offset into the
  # broadcast; the bot adds start_secs itself.
  epoch=$(date -u -d "@$(( 946684800 + fstart ))" +%Y-%m-%dT%H:%M:%SZ)
  sed -e "s|^source = .*|source = \"file\"|" -e "s|^input = .*|input = \"$file\"|" \
      -e "s|^start_secs = .*|start_secs = $fsecs|" -e "s|^recorded_start = .*|recorded_start = \"$epoch\"|" \
      -e "s|^path = .*|path = \"$work/db.sqlite\"|" -e "s|^obs_log = .*|obs_log = \"$work/obs.jsonl\"|" "$cfg" > "$work/cfg.toml"
  grep -q '^source = "file"' "$work/cfg.toml" || sed -i 's/^\[stream\]/[stream]\nsource = "file"/' "$work/cfg.toml"
  grep -q '^input = ' "$work/cfg.toml" || sed -i "s|^\[stream\]|[stream]\ninput = \"$file\"|" "$work/cfg.toml"
  grep -q '^start_secs = ' "$work/cfg.toml" || sed -i "s/^\[stream\]/[stream]\nstart_secs = $fsecs/" "$work/cfg.toml"
  grep -q '^recorded_start = ' "$work/cfg.toml" || sed -i "s/^\[stream\]/[stream]\nrecorded_start = \"$epoch\"/" "$work/cfg.toml"
else
  sed -e "s|^source = .*|source = \"vod\"|" -e "s|^vod_id = .*|vod_id = \"$vod\"|" \
      -e "s|^start_secs = .*|start_secs = $start|" \
      -e "s|^path = .*|path = \"$work/db.sqlite\"|" -e "s|^obs_log = .*|obs_log = \"$work/obs.jsonl\"|" "$cfg" > "$work/cfg.toml"
  grep -q '^source = "vod"' "$work/cfg.toml" || sed -i 's/^\[stream\]/[stream]\nsource = "vod"/' "$work/cfg.toml"
  grep -q '^vod_id = ' "$work/cfg.toml" || sed -i "s/^\[stream\]/[stream]\nvod_id = \"$vod\"/" "$work/cfg.toml"
  grep -q '^start_secs = ' "$work/cfg.toml" || sed -i "s/^\[stream\]/[stream]\nstart_secs = $start/" "$work/cfg.toml"
fi
grep -q '^obs_log = ' "$work/cfg.toml" || printf '\n[debug]\nobs_log = "%s/obs.jsonl"\n' "$work" >> "$work/cfg.toml"
# A replay must never talk in the real channel: force chat off whatever the
# config says (the live config is exactly what this script is handed).
awk 'BEGIN{s=0} /^\[/{ if (s && !done) {print "enabled = false"; done=1}; s=($0=="[chat]") } { if (s && $0 ~ /^enabled *=/) {print "enabled = false"; done=1; next} print } END{ if (s && !done) print "enabled = false"; if (!seen_chat) {} }' "$work/cfg.toml" > "$work/cfg.tmp" && mv "$work/cfg.tmp" "$work/cfg.toml"
grep -q '^\[chat\]' "$work/cfg.toml" || printf '\n[chat]\nenabled = false\n' >> "$work/cfg.toml"
fps=$(grep -oE '^fps = [0-9]+' "$work/cfg.toml" | grep -oE '[0-9]+' || true); fps=${fps:-1}
want=$(( dur * fps ))
# Frame metrics cover exactly the window; the bot runs 12 more minutes of
# video so a run that STARTS inside the window and finishes after it is still
# written (rows are inserted when a run ends). Then it is stopped — it would
# otherwise run to the end of the VOD.
export OMP_THREAD_LIMIT=1
"$bin" --config "$work/cfg.toml" run > "$work/log" 2>&1 &
pid=$!
# Bounded: a capture that never produces a frame (VOD gone, Twitch API
# trouble) must fail here, not hang whoever called us. First frame within two
# minutes; the whole window within a generous multiple of its length.
waited=0
while kill -0 $pid 2>/dev/null; do
  have=0; [ -f "$work/obs.jsonl" ] && have=$(wc -l < "$work/obs.jsonl")
  [ "$have" -ge $(( want + 720 * fps )) ] && break
  sleep 2; waited=$((waited + 2))
  if { [ "$have" -eq 0 ] && [ "$waited" -ge 120 ]; } || [ "$waited" -ge $(( dur + 900 )) ]; then
    kill $pid 2>/dev/null || true
    echo "replay produced $have frames in ${waited}s; giving up (see $work/log)" >&2
    exit 1
  fi
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
