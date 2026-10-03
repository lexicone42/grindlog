#!/usr/bin/env bash
# What a stream drop or a bot restart in the middle of a race costs, over
# every Big 20 day captured so far, held to what it cost last time.
#
#   scripts/audit-disrupt.sh            # sweep; a day that got worse fails
#   scripts/audit-disrupt.sh --update   # rewrite race-audit/disrupt-expected.txt
#
# For each kind of disruption below, `ngtwitchtimer audit --disrupt` replays
# every day of the corpus once per pass with the disruption at that pass,
# and compares what is filed with the same day replayed undisturbed (see
# audit::disrupt). Its answer per day is one line:
#
#   DAY <day> kind=offline gap=120 points=263 bad=44 lost=41 wrong=3 dup=0 extra=2
#
# bad is the number of passes at which the disruption costs anything at
# all. A day whose bad count rises fails; one whose count falls passes and
# says so, and `--update` records the better answer. The race audit
# (scripts/audit-race.sh) is still the check on the undisturbed days: this
# one only says what an interruption adds to them.
#
# Kinds and gaps (broadcast time unseen): a reconnect of 30 s (ffmpeg
# restarting on a stalled segment), the channel offline for 2 and 8 minutes
# (2026-09-22 dropped for 8), a crash with the supervisor back in 60 s, and
# a SIGTERM (a rollout, a plain kill) back in 30 s.
#
# Environment: RACE_AUDIT (default race-audit/), NG_BIN (default
# target/release/ngtwitchtimer), NG_CFG (default live.toml), DISRUPT_STEP
# (every Nth pass, default 1; the expected file is for 1).
set -u
cd "$(dirname "$0")/.."
corpus=${RACE_AUDIT:-race-audit}
bin=${NG_BIN:-target/release/ngtwitchtimer}
cfg=${NG_CFG:-live.toml}
step=${DISRUPT_STEP:-1}
expected="$corpus/disrupt-expected.txt"
[ -d "$corpus" ] || { echo "no corpus at $corpus" >&2; exit 2; }
export OMP_THREAD_LIMIT=1
actual=$(mktemp)
detail=${DISRUPT_DETAIL:-$(mktemp)}
for kg in reconnect:30 offline:120 offline:480 crash:60 sigterm:30; do
  "$bin" --config "$cfg" audit --dir "$corpus" --disrupt "${kg%:*}" --gap "${kg#*:}" --step "$step" 2>/dev/null
done > "$detail"
grep '^DAY ' "$detail" | cut -d' ' -f2- | sort > "$actual"
if [ "${1:-}" = "--update" ]; then
  cp "$actual" "$expected"
  echo "wrote $expected: $(wc -l < "$expected") line(s)"
  rm -f "$actual"; exit 0
fi
if [ ! -f "$expected" ]; then
  echo "no $expected yet; run with --update to record this answer" >&2
  cat "$actual"; rm -f "$actual"; exit 2
fi
key() { awk '{print $1 "/" $2 "/" $3}'; }
worse=0; better=0
while read -r day kind gap points bad rest; do
  k="$day $kind $gap"
  old=$(awk -v d="$day" -v k="$kind" -v g="$gap" '$1==d && $2==k && $3==g {print $5}' "$expected")
  [ -n "$old" ] || { echo "  new: $day $kind $gap $bad"; continue; }
  if [ "${bad#bad=}" -gt "${old#bad=}" ]; then
    echo "  WORSE: $k ${old} -> ${bad} ($rest)"; worse=$((worse + 1))
  elif [ "${bad#bad=}" -lt "${old#bad=}" ]; then
    echo "  better: $k ${old} -> ${bad} ($rest)"; better=$((better + 1))
  fi
done < "$actual"
total_old=$(awk '{sub("bad=","",$5); s+=$5} END{print s+0}' "$expected")
total_new=$(awk '{sub("bad=","",$5); s+=$5} END{print s+0}' "$actual")
echo "disruption audit: $(wc -l < "$actual") day/kind line(s); passes costing a row: $total_old before, $total_new now"
rm -f "$actual"
[ -z "${DISRUPT_DETAIL:-}" ] && rm -f "$detail"
if [ "$worse" -gt 0 ]; then echo "disruption audit: $worse line(s) worse"; exit 1; fi
[ "$better" -gt 0 ] && echo "disruption audit: $better line(s) better; --update to record them"
exit 0
