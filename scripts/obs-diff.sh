#!/usr/bin/env bash
# Diff two observation logs of the same window frame by frame — the second
# validation step in CLAUDE.md: two replays of one window (an old and a new
# binary, two configs, two thresholds) explain a regression by the frames
# where they disagree. Joins on `frame` and reports every frame whose OCR
# text, parsed value, phase, locked layout/offset or state-machine events
# differ.
#
#   scripts/obs-diff.sh [-q] <a.jsonl> <b.jsonl>
#
# Prints one summary line first — frames compared, how many differ and in
# which fields, frames present in only one log, the first differing frame —
# then one line per differing frame: the frame, its t_ms, which fields
# differ, and both sides as `"ocr" parsed PHASE layout@[dx,dy] events`
# (events shown as - when there are none). -q prints the summary alone. Exit
# status like diff's: 0 when the logs agree on every joined frame, 1 when
# they differ, 2 on a usage error. Holds the first log in memory (awk; one
# pass over each file) and writes nothing.
#
# The join assumes both logs number the same frames of video the same way:
# two replays of the same VOD window with the same fps and start, or of the
# same pinned file (see scripts/replay-window.sh — a file replay and a
# streamed one are not aligned, and neither is a log spanning several
# sessions, whose frame numbers restart; split those first). When t_ms
# disagrees on joined frames the summary says so, and the per-frame lines
# are then comparing different moments. Only the fields above are compared:
# smoothed_ms, reader, ink and splits are not, so a reader change that reads
# the same text is (correctly) not a difference.
set -euo pipefail
quiet=0
if [ "${1:-}" = "-q" ]; then quiet=1; shift; fi
a=${1:-}; b=${2:-}
if [ -z "$a" ] || [ -z "$b" ] || [ ! -f "$a" ] || [ ! -f "$b" ]; then
  echo "usage: scripts/obs-diff.sh [-q] <a.jsonl> <b.jsonl>" >&2
  exit 2
fi
if [ ! -s "$a" ] || [ ! -s "$b" ]; then
  echo "obs-diff: an empty log (no frames) — a replay that never produced a frame" >&2
  exit 2
fi
# The first record of each file bumps the pass: 1 while reading a, 2 for b
# (so the same path can be diffed against itself).
awk -v quiet="$quiet" '
FNR == 1 { pass++ }
function num(s, key,   re) {            # "key":123 -> 123; "key":null or absent -> ""
  re = "\"" key "\":-?[0-9]+"
  if (match(s, re)) return substr(s, RSTART + length(key) + 3, RLENGTH - length(key) - 3) + 0
  return ""
}
function str(s, key,   re) {
  re = "\"" key "\":\"[^\"]*\""
  if (match(s, re)) return substr(s, RSTART + length(key) + 4, RLENGTH - length(key) - 5)
  return ""
}
function arr(s, key,   re) {            # "key":[...] -> [...]
  re = "\"" key "\":\\[[^]]*\\]"
  if (match(s, re)) return substr(s, RSTART + length(key) + 3, RLENGTH - length(key) - 3)
  return "[]"
}
function describe(o, p, ph, lo, ev) { return sprintf("\"%s\" %s %s %s %s", o, (p == "" ? "null" : p), ph, lo, (ev == "[]" ? "-" : ev)) }
pass == 1 {
  f = num($0, "frame"); if (f == "") next
  if (f in A_t) { dupA++; next }
  A_t[f] = num($0, "t_ms"); A_ocr[f] = str($0, "ocr"); A_p[f] = num($0, "parsed_ms"); A_ph[f] = str($0, "phase")
  A_lo[f] = str($0, "layout") "@" arr($0, "offset"); A_ev[f] = arr($0, "events")
  nA++; next
}
{
  f = num($0, "frame"); if (f == "") next
  if (f in B_seen) { dupB++; next }
  B_seen[f] = 1; nB++
  if (!(f in A_t)) { onlyB++; if (firstOnly == "" || f < firstOnly) firstOnly = f; next }
  joined++
  t = num($0, "t_ms"); ocr = str($0, "ocr"); p = num($0, "parsed_ms"); ph = str($0, "phase")
  lo = str($0, "layout") "@" arr($0, "offset"); ev = arr($0, "events")
  if (t != A_t[f]) tdiff++
  what = ""
  if (ocr != A_ocr[f]) { what = what "ocr,"; d_ocr++ }
  if (p != A_p[f]) { what = what "parsed,"; d_p++ }
  if (ph != A_ph[f]) { what = what "phase,"; d_ph++ }
  if (lo != A_lo[f]) { what = what "offset,"; d_lo++ }
  if (ev != A_ev[f]) { what = what "events,"; d_ev++ }
  if (what == "") next
  differing++
  if (first == "" || f < first) { first = f; first_t = A_t[f] }
  if (!quiet) lines[differing] = sprintf("frame %7d  t %9s  %-26s a: %-44s b: %s", f, A_t[f], substr(what, 1, length(what) - 1), \
    describe(A_ocr[f], A_p[f], A_ph[f], A_lo[f], A_ev[f]), describe(ocr, p, ph, lo, ev))
}
END {
  for (f in A_t) if (!(f in B_seen)) { onlyA++; if (firstOnly == "" || f < firstOnly) firstOnly = f }
  printf "obs-diff: %d frames compared, %d differ (ocr %d, parsed %d, phase %d, offset %d, events %d); only in a %d, only in b %d", \
    joined + 0, differing + 0, d_ocr + 0, d_p + 0, d_ph + 0, d_lo + 0, d_ev + 0, onlyA + 0, onlyB + 0
  if (differing) printf "; first difference at frame %d (t_ms %s)", first, first_t
  else if (onlyA + onlyB) printf "; first frame missing on one side: %d", firstOnly
  printf "\n"
  if (tdiff) printf "warning: t_ms differs on %d of %d joined frames — the logs are not frame-aligned (different start or fps), per-frame lines compare different moments\n", tdiff, joined
  if (dupA + dupB) printf "warning: repeated frame numbers (a %d, b %d) — a log spanning several sessions; only the first occurrence was compared\n", dupA + 0, dupB + 0
  if (!quiet) for (i = 1; i <= differing; i++) print lines[i]
  exit (differing ? 1 : 0)
}' "$a" "$b"
