#!/usr/bin/env bash
# Replay the race-board tracker over every Big 20 day captured so far and
# hold the result to what it recorded last time.
#
#   scripts/audit-race.sh            # audit; differences from race-audit/expected.txt fail
#   scripts/audit-race.sh --update   # rewrite race-audit/expected.txt from this run
#
# The corpus is race-audit/: one boards-<label>.jsonl and obs-<label>.jsonl
# per day, live captures and VOD replays both (links to where the bot or a
# replay wrote them). The audit is `ngtwitchtimer audit --dir`, the same
# harness docs/marathons.md describes, and its answer per day is the rows
# it files: "<row>:<cumulative>/<segment>". A change to marathon.rs,
# roster.rs or the readers is run through this BEFORE it is believed: on
# 2026-09-29 two rules that mended that day's log each cost rows on other
# days, and only this comparison said so. A day whose signature moves is
# not wrong by that fact — read the diff, decide, and `--update` when the
# new answer is the better one.
#
# The answer is the AUDIT's, which replays the tracker over the board log
# alone: not the run loop, which also seeds a rebuilt tracker from the
# database and closes the marathon with the broadcast. A day whose audit
# says fewer rows than its VOD replay recorded (2026-09-25: 16 against 20)
# is a day the tracker was rebuilt mid-run, and the comparison is still
# fair as long as both sides of it come from this script.
#
# Environment: RACE_AUDIT (the corpus, default race-audit/), NG_BIN (the
# binary, default target/release/ngtwitchtimer), NG_CFG (default live.toml).
set -u
cd "$(dirname "$0")/.."
corpus=${RACE_AUDIT:-race-audit}
bin=${NG_BIN:-target/release/ngtwitchtimer}
cfg=${NG_CFG:-live.toml}
expected="$corpus/expected.txt"
[ -d "$corpus" ] || { echo "no corpus at $corpus" >&2; exit 2; }
export OMP_THREAD_LIMIT=1
actual=$(mktemp)
"$bin" --config "$cfg" audit --dir "$corpus" 2>/dev/null \
  | awk '/^REC / { rows[$2] = rows[$2] " " $3 ":" $4/1000 "/" $5/1000; n[$2]++ }
         END { for (v in rows) printf "%s %d%s\n", v, n[v], rows[v] }' \
  | sort > "$actual"
if [ "${1:-}" = "--update" ]; then
  cp "$actual" "$expected"
  echo "wrote $expected: $(wc -l < "$expected") day(s)"
  awk '{print "  " $1 ": " $2 " row(s)"}' "$expected"
  rm -f "$actual"; exit 0
fi
if [ ! -f "$expected" ]; then
  echo "no $expected yet; run with --update to record this answer" >&2
  awk '{print "  " $1 ": " $2 " row(s)"}' "$actual"
  rm -f "$actual"; exit 2
fi
if diff <(cut -d' ' -f1,2 "$expected") <(cut -d' ' -f1,2 "$actual") > /dev/null \
   && diff "$expected" "$actual" > /dev/null; then
  echo "race audit: $(wc -l < "$expected") day(s) as expected"
  awk '{print "  " $1 ": " $2 " row(s)"}' "$expected"
  rm -f "$actual"; exit 0
fi
echo "race audit: differences from $expected"
join -j 1 <(cut -d' ' -f1 "$expected" | sort) <(cut -d' ' -f1 "$actual" | sort) > /dev/null
for day in $(cut -d' ' -f1 "$expected" "$actual" | sort -u); do
  e=$(grep "^$day " "$expected" | cut -d' ' -f2-)
  a=$(grep "^$day " "$actual" | cut -d' ' -f2-)
  [ "$e" = "$a" ] && continue
  echo "  $day: expected ${e%% *} row(s), got ${a%% *}"
  comm -3 <(echo "$e" | tr ' ' '\n' | tail -n +2 | sort) <(echo "$a" | tr ' ' '\n' | tail -n +2 | sort) \
    | sed 's/^\t/    now:  /; s/^[^ ]/    was:  &/' | sed 's/^    was:  now:/    now: /'
done
rm -f "$actual"
exit 1
