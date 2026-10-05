#!/usr/bin/env bash
# Re-analyse archived broadcasts with the bot as it is now: the live config
# (live.toml) and the current binary, over the local recording, into one
# database per VOD that import-vod.sh can land.
#
#   scripts/reanalyse-archive.sh [-j N] vods/archive/<date>-<vod_id>.mp4...
#
# Why: a day captured live is captured by whatever binary ran that day. The
# readers and rules since then (the learned timer reader; a split that ties
# its comparison, recorded at the run's end; the race tracker kept across a
# restart) are not in it. Replayed, 2026-09-04 gains the two splits run 6029
# lost live, and every other run of the window matches.
#
# Each pass reads the recording as a file, dated from the VOD's creation
# time as the live database has it (sessions.vod_created_at_ms; a VOD it
# does not know is skipped, since its runs would land on the wrong day),
# and its session is then relabelled as the VOD ("vod <id>"), the shape
# import-vod.sh expects of a backfill pass. Nothing touches the live
# database: compare first, then land a day with
#   cp $OUT/vod-<id>.db backfill-db/ && scripts/import-vod.sh <id>
# (import-vod.sh replaces the whole broadcast DAY and refuses a thinner pass;
# rehearse it with LIVE=<a copy> as CLAUDE.md says).
#
# Environment: REANALYSE_OUT (default reanalyse-db), NG_BIN (default
# target/release/ngtwitchtimer), NG_CFG (default live.toml), REANALYSE_NICE
# (default 15). -j runs N recordings at once (default 3).
set -uo pipefail
cd "$(dirname "$0")/.."
out=${REANALYSE_OUT:-reanalyse-db}
bin=${NG_BIN:-./target/release/ngtwitchtimer}
src=${NG_CFG:-live.toml}
prio=${REANALYSE_NICE:-15}
jobs=3
if [ "${1:-}" = "-j" ]; then jobs=$2; shift 2; fi
[ -f "$src" ] || { echo "no config at $src" >&2; exit 1; }
[ -x "$bin" ] || { echo "no binary at $bin" >&2; exit 1; }
mkdir -p "$out"
export OMP_THREAD_LIMIT=1 OMP_NUM_THREADS=1

one() {
  local file=$1 id created epoch cfg
  id=$(basename "$file" .mp4 | sed -n 's/.*-\([0-9]\{6,\}\)$/\1/p')
  [ -n "$id" ] || { echo "!!! $file: no VOD id in the name (<date>-<id>.mp4)"; return; }
  created=$(sqlite3 -readonly ninja-gaiden.db "SELECT MIN(vod_created_at_ms) FROM sessions WHERE vod_id = '$id'" 2>/dev/null)
  [ -n "$created" ] || { echo "!!! $id: the live database has no creation time for this VOD; skipped"; return; }
  epoch=$(date -u -d "@$((created / 1000))" +%Y-%m-%dT%H:%M:%SZ)
  cfg="$out/cfg-$id.toml"
  sed -e "s|^source = .*|source = \"file\"|" \
      -e "s|^input = .*|input = \"$PWD/$file\"|" \
      -e "s|^start_secs = .*|start_secs = 0|" \
      -e "s|^recorded_start = .*|recorded_start = \"$epoch\"|" \
      -e "s|^path = .*|path = \"$out/vod-$id.db\"|" \
      -e "s|^obs_log = .*|obs_log = \"$out/obs-$id.jsonl\"|" \
      -e "s|^board_log = .*|board_log = \"$out/boards-$id.jsonl\"|" "$src" > "$cfg"
  for line in 'source = "file"' "input = \"$PWD/$file\"" 'start_secs = 0' "recorded_start = \"$epoch\""; do
    key=${line%% =*}
    grep -q "^$key = " "$cfg" || sed -i "s|^\[stream\]|[stream]\n$line|" "$cfg"
  done
  grep -q '^path = ' "$cfg" || printf '\n[database]\npath = "%s/vod-%s.db"\n' "$out" "$id" >> "$cfg"
  grep -q '^obs_log = ' "$cfg" || printf '\n[debug]\nobs_log = "%s/obs-%s.jsonl"\n' "$out" "$id" >> "$cfg"
  awk 'BEGIN{s=0} /^\[/{ if (s && !done) {print "enabled = false"; done=1}; s=($0=="[chat]") } { if (s && $0 ~ /^enabled *=/) {print "enabled = false"; done=1; next} print } END{ if (s && !done) print "enabled = false" }' \
      "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
  grep -q '^\[chat\]' "$cfg" || printf '\n[chat]\nenabled = false\n' >> "$cfg"
  if grep -qE '^path = .*(ninja-gaiden|live)\.db' "$cfg"; then
    echo "!!! $cfg still points at the live database; refusing" >&2; return
  fi
  rm -f "$out/vod-$id.db" "$out/vod-$id.db-wal" "$out/vod-$id.db-shm" "$out/obs-$id.jsonl" "$out/boards-$id.jsonl"
  echo "=== $id ($file, from $epoch) — $(date -Is)"
  if nice -n "$prio" "$bin" --config "$cfg" run > "$out/log-$id.txt" 2>&1; then
    sed -i 's/\x1b\[[0-9;]*m//g' "$out/log-$id.txt"
    sqlite3 "$out/vod-$id.db" "UPDATE sessions SET source = 'vod', label = 'vod $id' WHERE source = 'file'"
    sqlite3 "$out/vod-$id.db" \
      "SELECT '$id: '||COUNT(*)||' run(s), '||SUM(outcome = 'finished')||' finished, '||(SELECT COUNT(*) FROM splits)||' split(s)' FROM runs" 2>/dev/null
  else
    echo "!!! $id failed; see $out/log-$id.txt"
  fi
}
export -f one
export out bin src prio
printf '%s\n' "$@" | xargs -P "$jobs" -I{} bash -c 'one "$@"' _ {}
echo "=== reanalysis complete $(date -Is)"
