#!/usr/bin/env bash
# The nightly self-audit: replay yesterday's broadcast on the deployed build
# and say what the live bot missed, misnamed or invented.
#
#   ./scripts/self-audit.sh [<vod_id>] [--import]
#
# Every failure of the first week under follow_title = "any" lived in the
# first minutes of a scene (the wrong layout locked before the pane could be
# read, a header read in pieces until the roster or the vote caught up), and
# every fix was proven by replaying the day. This runs that replay by itself:
# with no id it takes the newest archived VOD that is not still live and has
# not been audited; `replay-big20.sh` replays it on target/release with
# live.toml into $SELF_AUDIT_OUT (default self-audit/), and the practice rows
# of that pass are compared with the live database's for the VOD's span.
#
# The report (one block per VOD, appended to logs/self-audit.log and printed):
#   LIVE-ONLY   a live row the replay has no counterpart for (same game and
#               category, starting within thirty seconds or the same finish):
#               a run the
#               replay missed, or a live row under a name the replay did not
#               give — the latter flagged UNNAMED when no roster, no earlier
#               day of the database and not the tracked game knows the name,
#               which is what a minted spelling looks like ("MONSIer rary").
#   REPLAY-ONLY a replay row live has no counterpart for: a run live missed.
#   MINTED      a replay row under a name nothing knows: the replay's own
#               mistake, and the reason an import is refused.
#
# `--import` lands the pass with import-big20.sh --replace-live --deploy —
# only when the replay minted nothing. Live rows the replay lacks are kept by
# that import (a replay that misses a game must not take live's rows with
# it), and a live row under a minted name is never deleted here: those are
# listed with their ids for a hand DELETE. SELF_AUDIT_PASS=<dir> compares an
# existing pass instead of replaying (for a second look at a day).
#
# Environment: SELF_AUDIT_OUT (default self-audit/), SELF_AUDIT_PASS, LIVE
# (default ninja-gaiden.db), the rest as replay-big20.sh and import-big20.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
out=${SELF_AUDIT_OUT:-self-audit}
live=${LIVE:-ninja-gaiden.db}
log=logs/self-audit.log
mkdir -p "$out" logs
import=false; id=""
for a in "$@"; do
  case "$a" in
    --import) import=true;;
    -*) echo "unknown option $a (usage: $0 [<vod_id>] [--import])" >&2; exit 1;;
    *) id=$a;;
  esac
done
channel=$(grep -E '^channel = ' live.toml | head -1 | sed 's/.*= *"\(.*\)".*/\1/')
tracked=$(grep -E '^name = ' live.toml | head -1 | sed 's/.*= *"\(.*\)".*/\1/')
q() { sqlite3 -separator '|' "$1" "$2"; }

if [ -z "$id" ]; then
  # The newest VOD that is over (not the broadcast still open in the live
  # database) and not yet audited.
  touch "$out/done.txt"
  open=$(q "$live" "SELECT COALESCE(MAX(vod_id),'') FROM sessions WHERE ended_at_ms IS NULL AND source='hls'")
  while read -r vid date hours title; do
    [ -n "$vid" ] || continue
    [ "$vid" = "$open" ] && continue
    grep -qx "$vid" "$out/done.txt" && continue
    id=$vid; break
  done < <(scripts/list-vods.sh "$channel" 2>/dev/null)
  [ -n "$id" ] || { echo "self-audit: nothing to audit" | tee -a "$log"; exit 0; }
fi

pass=${SELF_AUDIT_PASS:-$out}
if [ -z "${SELF_AUDIT_PASS:-}" ]; then
  BIG20_OUT="$out" scripts/replay-big20.sh "$id" > "$out/replay-$id.txt" 2>&1 || {
    echo "self-audit $id: replay failed; see $out/replay-$id.txt" | tee -a "$log"; exit 1; }
fi
db="$pass/vod-$id.db"
[ -f "$db" ] || { echo "self-audit $id: no pass at $db" | tee -a "$log"; exit 1; }

# What the names can be checked against: every game of every shipped roster,
# every game the live database held before this day with three rows or more,
# and the tracked game.
lo=$(q "$db" "SELECT MIN(started_at_ms) FROM sessions")
hi=$(q "$db" "SELECT MAX(ended_at_ms) FROM sessions")
known=$(mktemp); trap 'rm -f "$known"' EXIT
{ grep -hoE '^\s*"[^"]+",?\s*$' assets/*roster*.toml | sed 's/^[[:space:]]*"//; s/",\{0,1\}[[:space:]]*$//'
  q "$live" "SELECT game FROM runs WHERE started_at_ms < $lo GROUP BY game HAVING COUNT(*) >= 3"
  echo "$tracked"; } | sort -u > "$known"
unknown() { grep -qxF -- "$1" "$known" && echo "" || echo " UNNAMED"; }

rep=$(mktemp); trap 'rm -f "$known" "$rep"' EXIT
{
  echo "=== self-audit $id ($(date -Is)): $(q "$db" "SELECT COUNT(*) FROM runs WHERE NOT (game='$tracked')") replay row(s), $(q "$live" "SELECT COUNT(*) FROM runs WHERE started_at_ms < $hi AND ended_at_ms > $lo AND NOT (game='$tracked')") live row(s) in its span"
  minted=0
  # Replay rows: counterpart in live, or REPLAY-ONLY; a minted name is MINTED.
  while IFS='|' read -r rid game cat outcome fin s e; do
    u=$(unknown "$game")
    [ -n "$u" ] && minted=$((minted+1)) && echo "MINTED      $game [$cat] $outcome $(date -u -d @$((s/1000)) +%H:%M:%S)"
    n=$(q "$live" "SELECT COUNT(*) FROM runs r JOIN sessions ss ON r.session_id=ss.id WHERE ss.source='hls' AND r.game='$(printf %s "$game" | sed "s/'/''/g")' AND r.category='$cat' AND (ABS(r.started_at_ms - $s) < 30000 OR (r.outcome='finished' AND r.final_time_ms=$fin AND ABS(r.started_at_ms-$s) < 3600000))")
    [ "$n" = 0 ] && [ -z "$u" ] && echo "REPLAY-ONLY $game [$cat] $outcome $((fin/1000))s $(date -u -d @$((s/1000)) +%H:%M:%S)  (live missed it)"
  done < <(q "$db" "SELECT id, game, category, outcome, COALESCE(final_time_ms,0), started_at_ms, ended_at_ms FROM runs WHERE NOT (game='$tracked') ORDER BY started_at_ms")
  # Live rows: counterpart in the replay, or LIVE-ONLY (UNNAMED when minted).
  while IFS='|' read -r lid game cat outcome fin s e; do
    n=$(q "$db" "SELECT COUNT(*) FROM runs WHERE game='$(printf %s "$game" | sed "s/'/''/g")' AND category='$cat' AND (ABS(started_at_ms - $s) < 30000 OR (outcome='finished' AND final_time_ms=$fin AND ABS(started_at_ms-$s) < 3600000))")
    [ "$n" = 0 ] && echo "LIVE-ONLY   id $lid $game [$cat] $outcome $((fin/1000))s $(date -u -d @$((s/1000)) +%H:%M:%S)$(unknown "$game")"
  done < <(q "$live" "SELECT r.id, r.game, r.category, r.outcome, COALESCE(r.final_time_ms,0), r.started_at_ms, r.ended_at_ms FROM runs r JOIN sessions ss ON r.session_id=ss.id WHERE ss.source='hls' AND r.started_at_ms < $hi AND r.ended_at_ms > $lo AND NOT (r.game='$tracked') ORDER BY r.started_at_ms")
  echo "minted=$minted"
} > "$rep"
grep -v '^minted=' "$rep" | tee -a "$log"
minted=$(grep '^minted=' "$rep" | cut -d= -f2)
if $import; then
  if [ "$minted" != 0 ]; then
    echo "self-audit $id: the replay minted $minted name(s); not importing" | tee -a "$log"; exit 3
  fi
  BIG20_OUT="$pass" scripts/import-big20.sh "$id" --replace-live --deploy 2>&1 | tail -3 | tee -a "$log"
fi
[ -z "${SELF_AUDIT_PASS:-}" ] && echo "$id" >> "$out/done.txt"
exit 0
