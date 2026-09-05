#!/usr/bin/env bash
# Label-free misread rate of the timer readings in an observation log — the
# accuracy check that needs no ground truth. Between two consecutive frames
# of a running timer the reading must advance by exactly one frame interval;
# a pair that does not (±TOL_MS, 60 by default) is a misread. Score the log
# of a replay (replays/<label>/obs.jsonl), a backfill day
# (backfill-logs/obs-<vod>.jsonl) or the live log with it; compare two
# binaries or two configs on the same window by their rates.
#
#   scripts/obs-accuracy.sh <obs.jsonl>
#
# Prints the frame interval it derived (the mode of the t_ms deltas: 500 at
# the live config's 2 fps), the pairs and frames compared, the misreads and
# their rate, what was excluded and why, the rate per reader (glyph / tess,
# when the log carries the field) and the WORST (10) worst frames: frame
# number, the readings before, at and after it, how far the reading moved
# and by how much that misses the interval. Exit status 0; 1 when the log
# holds nothing to compare; 2 on a usage error. Reads the file once with
# awk; writes nothing. Two seconds for a 57,000-frame day.
#
# A pair enters the comparison only when both frames have a parsed value and
# phase RUNNING. The naive "advance by one interval" rule then still fires on
# frames that are not misreads at all, so these are excluded, in this order
# (each excluded pair is counted once, under the first reason that applies):
#
#   static      the frame was pixel-identical to the previous one and the
#               reading was replayed, not read ("static": true): comparing it
#               would score the video, not the reader;
#   dropped     t_ms did not advance by exactly one interval, or the frame
#               numbers are not consecutive: a dropped or duplicated frame
#               (or a new session) moves the timer by some other amount;
#   under-10s   either value is under 10 s: LiveSplit's pre-start countdown
#               (a bare "5.00" counting down; "-5.00" without its sign) and
#               the first seconds after a reset live there, and the lenient
#               sub-ten-second parse ("476" for 4.76) is a different reading
#               problem from the one this measures;
#   held        either value is also held by its outer neighbour: the timer
#               stopped or resumed inside this interval (the pair INTO a
#               finish advances by less than an interval, and so does the
#               pair out of a pause) — the frozen value itself is caught by
#   unchanged   the value did not change at all: a frozen timer legitimately
#               reads the same value frame after frame. A real misread that
#               repeats the previous value exactly is missed by these two,
#               and accepted as the price;
#   transition  the second frame carries a state-machine event (a reset seen
#               as the timer zeroed, a desync restart, a finish): the jump is
#               the run ending, not the reader failing;
#   after-lock  either frame is within the first 3 frames after a lock or
#               re-anchor (the log's layout or offset changed): the crop has
#               just moved and the first reads at the new position are not
#               representative of steady-state reading.
#
# An isolated misread breaks two pairs, into it and out of it, with errors
# that cancel; those are collapsed into one misread frame, the one in the
# middle. A violation whose neighbouring pair could not be compared is
# attributed to its later frame, and a stretch of consecutive misreads (a
# minute digit read wrong for several frames) is blamed once at each end.
# The rate per pair is what the rule measures; the frame count is what it
# means.
#
# Tolerance: a 30 fps stream sampled at 2 fps puts the true advance anywhere
# within ±33 ms of the interval, and LiveSplit's own redraw adds jitter of
# the same size, so ±60 ms is where the jitter ends (on the reference
# channel's logs the error distribution is dense to 40 ms and all but empty
# from 41 to 100). Consequences: tesseract's systematic hundredths confusion ("11"
# read as "14", 30 ms) hides under the jitter and is NOT counted at ±60 —
# what is counted are readings with a digit or separator wrong, "9:34:57"
# for 9:34.57 parsed as hours among them. TOL_MS=40 counts the tail of the
# small confusions at the cost of some jitter. Calibration on VOD 2862548640
# (2026-09-01), backfill-logs/obs-2862548640.jsonl against
# backfill-logs/tess-20260903/obs-2862548640.jsonl: glyph reader 4 frames
# misread of 42,141 (0.01%; 12 at ±40), tesseract 64 of 19,681 (0.33%; 147
# at ±40) plus 5.8% of running frames it could not parse at all — reported
# on the excluded line, since a frame without a reading is a failure, not a
# misread, and a reader that declined everything would score a perfect rate.
# At the glyph reader's level what remains is mostly not the reader: a
# reading that misses by 70-100 ms and is caught up by the next frame is
# LiveSplit redrawing late on the streamer's machine, and both readings are
# plausible strings; the frames tesseract read as fallback (the ones the
# glyph reader declined) misread at a far higher rate than the rest.
set -euo pipefail
f=${1:-}
if [ -z "$f" ] || [ ! -f "$f" ]; then
  echo "usage: scripts/obs-accuracy.sh <obs.jsonl>" >&2
  exit 2
fi
tol=${TOL_MS:-60}
worst=${WORST:-10}
awk -v tol="$tol" -v worst="$worst" -v name="$f" '
function num(s, key,   re) {            # "key":123 -> 123; "key":null or absent -> ""
  re = "\"" key "\":-?[0-9]+"
  if (match(s, re)) return substr(s, RSTART + length(key) + 3, RLENGTH - length(key) - 3) + 0
  return ""
}
function str(s, key,   re) {            # "key":"text" -> text (the OCR strings hold digits, ":" and ".")
  re = "\"" key "\":\"[^\"]*\""
  if (match(s, re)) return substr(s, RSTART + length(key) + 4, RLENGTH - length(key) - 5)
  return ""
}
function abs(x) { return x < 0 ? -x : x }
function rdr(i) { return rd[i] == "" ? "-" : rd[i] }
{
  n++
  fr[n] = num($0, "frame"); t[n] = num($0, "t_ms"); p[n] = num($0, "parsed_ms")
  ph[n] = str($0, "phase"); oc[n] = str($0, "ocr"); rd[n] = str($0, "reader"); lay[n] = str($0, "layout")
  off[n] = match($0, /"offset":\[[^]]*\]/) ? substr($0, RSTART + 9, RLENGTH - 9) : ""
  ev[n] = match($0, /"events":\[[^]]*\]/) ? substr($0, RSTART + 9, RLENGTH - 9) : "[]"
  st[n] = ($0 ~ /"static":true/)
  if (rd[n] != "") has_reader = 1
  if (ph[n] == "RUNNING") { running++; if (p[n] == "") unparsed++ }
  if (n > 1 && t[n] != "" && t[n-1] != "") dcount[t[n] - t[n-1]]++
}
END {
  best = 0; interval = 0
  for (d in dcount) if (dcount[d] > best) { best = dcount[d]; interval = d + 0 }
  if (n < 2 || interval <= 0) { printf "%s: %d frames, no frame interval to derive\n", name, n; exit 1 }
  # A lock or re-anchor shows in the log as a change of layout or offset;
  # taint that frame and the two after it.
  for (i = 2; i <= n; i++) if (lay[i] != lay[i-1] || off[i] != off[i-1]) { locks++; taint[i] = 1; taint[i+1] = 1; taint[i+2] = 1 }
  for (i = 2; i <= n; i++) {
    a = i - 1; b = i
    if (ph[a] != "RUNNING" || ph[b] != "RUNNING") { x_run++; continue }
    if (p[a] == "" || p[b] == "") { x_unparsed++; continue }
    if (st[a] || st[b]) { x_static++; continue }
    if (t[b] - t[a] != interval || fr[b] != fr[a] + 1) { x_drop++; continue }
    if (p[a] < 10000 || p[b] < 10000) { x_low++; continue }
    if (p[a] == p[b]) { x_same++; continue }
    if (p[b] == p[b+1] || p[a] == p[a-1]) { x_held++; continue }
    if (ev[b] != "[]") { x_event++; continue }
    if (taint[a] || taint[b]) { x_lock++; continue }
    pairs++; inpair[a] = 1; inpair[b] = 1
    d = p[b] - p[a]; e = d - interval
    if (abs(e) > tol) { V++; vi[V] = b; vd[V] = d; ve[V] = e }
  }
  for (i = 1; i <= n; i++) if (inpair[i]) { frames++; fr_rd[rdr(i)]++ }
  # Collapse the pair into a misread and the pair out of it (errors cancel)
  # into the frame between them; anything else is blamed on its later frame.
  for (j = 1; j <= V; j++) {
    if (skip[j]) continue
    B++; bf[B] = vi[j]; bd[B] = vd[j]; be[B] = ve[j]
    if (j < V && vi[j+1] == vi[j] + 1 && abs(ve[j] + ve[j+1]) <= tol) skip[j+1] = 1
    bad_rd[rdr(vi[j])]++
  }
  printf "%s: %d frames at %d ms, %d running, %d lock/re-anchor changes\n", name, n, interval, running + 0, locks + 0
  if (pairs == 0) { print "no consecutive running pairs to compare"; exit 1 }
  printf "compared %d pairs / %d frames; misread %d pairs (%.2f%%) = %d frames (%.2f%%); tolerance ±%d ms\n", \
    pairs, frames, V + 0, 100 * V / pairs, B + 0, 100 * B / frames, tol
  printf "excluded pairs: not-running %d, unparsed %d (%.1f%% of running frames unparsed), static %d, dropped %d, under-10s %d, held %d, unchanged %d, transition %d, after-lock %d\n", \
    x_run + 0, x_unparsed + 0, 100 * unparsed / running, x_static + 0, x_drop + 0, x_low + 0, x_held + 0, x_same + 0, x_event + 0, x_lock + 0
  if (has_reader) {
    line = "by reader:"
    for (k in fr_rd) line = line sprintf("  %s %d frames, %d misread (%.2f%%);", k, fr_rd[k], bad_rd[k] + 0, 100 * (bad_rd[k] + 0) / fr_rd[k])
    print substr(line, 1, length(line) - 1)
  }
  if (B == 0) exit 0
  printf "worst %d of %d frames:\n", (B < worst ? B : worst), B
  printf "  %7s  %-12s %-12s %-12s  %9s  %8s  %s\n", "frame", "previous", "reading", "next", "moved", "off by", "reader"
  for (k = 1; k <= worst && k <= B; k++) {
    m = 0
    for (j = 1; j <= B; j++) if (!used[j] && (m == 0 || abs(be[j]) > abs(be[m]))) m = j
    used[m] = 1; b = bf[m]
    printf "  %7d  %-12s %-12s %-12s  %+9d  %+8d  %s\n", fr[b], oc[b-1], oc[b], (b < n ? oc[b+1] : ""), bd[m], be[m], rdr(b)
  }
}' "$f"
