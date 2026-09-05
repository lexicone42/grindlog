#!/usr/bin/env bash
# One plain-text block for a day of the grind, from the live database and the
# logs; cron runs it at 23:58 (crontab.example), or run it by hand for a day:
#
#   scripts/daily-summary.sh [YYYY-MM-DD]     # default: today, local time
#
# Attempts, finishes and the best time with its LiveSplit run number, resets
# by act (bucketed like the site's death chart, from [game] acts in
# live.toml), numbered coverage ("N of his M attempts captured", M being the
# span of his own counter that day), the sessions and their capture health,
# the glyph reader's read/declined totals from the session-close lines in
# logs/live.log (a session the supervisor SIGTERMs, as a rollout does, closes
# without one), VOD imports that landed that day (logs/import-when-done*.log,
# else the mtime of backfill-db/imported.txt) and the day's healthcheck
# alerts (logs/health.log). Printed to stdout and, when NG_ALERT_URL or
# NG_ALERT_MAIL is set, delivered through scripts/notify.sh. Reads the
# database read-only (NG_DB overrides ninja-gaiden.db). The bot's log stamps
# are UTC; the day is the box's local day, like the site's. Exits 0 unless
# the argument is not a date.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 0
DB=${NG_DB:-ninja-gaiden.db}
LOG=logs/live.log
day=$(date -d "${1:-today}" +%F 2>/dev/null) || { echo "usage: $0 [YYYY-MM-DD]" >&2; exit 2; }
# From here on the exit status stays 0 whatever goes wrong: cron redirects
# stderr into logs/summary.log, where a failure is read the next morning.
trap 'exit 0' EXIT
start=$(date -d "$day 00:00" +%s)
end=$(date -d "$(date -d "$day + 1 day" +%F) 00:00" +%s)
s_ms=$((start * 1000)) e_ms=$((end * 1000))
in_day="started_at_ms >= $s_ms AND started_at_ms < $e_ms"

q() { sqlite3 -readonly -cmd '.timeout 5000' "$DB" "$1" 2>/dev/null; }
fmt() { # ms -> M:SS.t like the log and LiveSplit (H:MM:SS.t past an hour)
  local ms=$1 h m s t
  h=$((ms / 3600000)); m=$((ms % 3600000 / 60000)); s=$((ms % 60000 / 1000)); t=$((ms % 1000 / 100))
  if [ "$h" -gt 0 ]; then printf '%d:%02d:%02d.%d' "$h" "$m" "$s" "$t"; else printf '%d:%02d.%d' "$m" "$s" "$t"; fi
}
hhmm() { date -d "@$(($1 / 1000))" +%H:%M; }
channel=$(grep -E '^[[:space:]]*channel[[:space:]]*=' live.toml 2>/dev/null | head -1 | sed -E 's/.*"([^"]*)".*/\1/')
out=()
say() { out+=("$1"); }

say "grindlog daily summary: $(date -d "$day" '+%a %Y-%m-%d')${channel:+ ($channel)}"

# --- attempts, finishes, best.
IFS='|' read -r attempts finished numbered ls_min ls_max <<< \
  "$(q "SELECT COUNT(*), COALESCE(SUM(outcome='finished'),0), COALESCE(SUM(ls_attempt IS NOT NULL),0),
              COALESCE(MIN(ls_attempt),''), COALESCE(MAX(ls_attempt),'') FROM runs WHERE $in_day")"
if [ -z "${attempts:-}" ]; then
  say "cannot read $DB"
  attempts=0 finished=0 numbered=0 ls_min="" ls_max=""
fi
sessions=$(q "SELECT started_at_ms, COALESCE(ended_at_ms,''), source, COALESCE(frames,''), COALESCE(parsed,''), COALESCE(relocks,'')
              FROM sessions WHERE $in_day ORDER BY started_at_ms")
if [ "$attempts" -eq 0 ] && [ -z "$sessions" ]; then
  say "no session and no attempts: offline, or another game (title_filter)"
else
  say "attempts: $attempts ($finished finished, $((attempts - finished)) resets)"
fi
if [ "$finished" -gt 0 ]; then
  season=$(q "SELECT value FROM settings WHERE key='ls_season_best_ms'")
  IFS='|' read -r best_ms best_ls <<< "$(q "SELECT final_time_ms, COALESCE(ls_attempt,'') FROM runs
     WHERE $in_day AND outcome='finished' AND final_time_ms IS NOT NULL ORDER BY final_time_ms LIMIT 1")"
  if [ -n "${best_ms:-}" ]; then
    line="best: $(fmt "$best_ms")${best_ls:+ (run $best_ls)}"
    if [ -n "${season:-}" ]; then
      if [ "$best_ms" -lt "$season" ]; then line+=" -- NEW SEASON BEST (was $(fmt "$season"))"; else line+=", season best $(fmt "$season")"; fi
    fi
    say "$line"
  fi
  fins=$(q "SELECT final_time_ms, COALESCE(ls_attempt,'?') FROM runs WHERE $in_day AND outcome='finished'
            AND final_time_ms IS NOT NULL ORDER BY started_at_ms LIMIT 12" \
         | while IFS='|' read -r ms ls; do printf '%s (%s), ' "$(fmt "$ms")" "$ls"; done)
  [ "$finished" -gt 12 ] && fins+="..."
  say "finishes: ${fins%, }"
fi

# --- resets by act: the same buckets as stats::death_chart, a reset falls in
# the first act whose end_ms its last timer value is under; the last act (no
# end_ms) takes the rest. Runs with no last timer value are not counted.
acts=$(sed -n '/^acts = \[/,/^\]/p' live.toml 2>/dev/null | grep -oE '\{[^}]*name = "[^"]+"[^}]*\}')
if [ -n "$acts" ] && [ "$((attempts - finished))" -gt 0 ]; then
  case_sql="CASE" names=()
  while IFS= read -r a; do
    name=$(sed -E 's/.*name = "([^"]+)".*/\1/' <<< "$a")
    end_ms=$(grep -oE 'end_ms = [0-9]+' <<< "$a" | grep -oE '[0-9]+' || true)
    names+=("$name")
    if [ -n "$end_ms" ]; then case_sql+=" WHEN last_timer_ms < $end_ms THEN '${name//\'/\'\'}'"; else break; fi
  done <<< "$acts"
  case_sql+=" ELSE '${names[${#names[@]}-1]//\'/\'\'}' END"
  counts=$(q "SELECT act, COUNT(*) FROM (SELECT $case_sql AS act FROM runs WHERE $in_day AND outcome != 'finished'
              AND last_timer_ms IS NOT NULL) GROUP BY act")
  line=""
  for name in "${names[@]}"; do
    c=$(awk -F'|' -v n="$name" '$1 == n {print $2}' <<< "$counts")
    line+="$name: ${c:-0}, "
  done
  say "resets by act: ${line%, }"
fi

# --- coverage against his own counter.
if [ -n "$ls_min" ] && [ -n "$ls_max" ]; then
  span=$((ls_max - ls_min + 1))
  line="coverage: $attempts of his $span attempts captured (counter $ls_min-$ls_max), run numbers on $numbered/$attempts"
  # More runs than the counter spans: an unnumbered run at either edge, or a
  # misread number fill-run-numbers.sh has not cleared yet.
  [ "$attempts" -gt "$span" ] && line+=" -- more than his counter spans (an unnumbered edge run or a misread number)"
  say "$line"
elif [ "$attempts" -gt 0 ]; then
  say "coverage: no run numbers read today ($attempts attempts)"
fi

# --- sessions and their capture health, from the sessions table.
if [ -n "$sessions" ]; then
  line="" total_ms=0 frames_sum=0 parsed_sum=0 relocks_sum=0
  while IFS='|' read -r st en src fr pa rl; do
    if [ -n "$en" ]; then line+="$(hhmm "$st")-$(hhmm "$en")"; total_ms=$((total_ms + en - st)); else line+="$(hhmm "$st")-open"; total_ms=$((total_ms + $(date +%s) * 1000 - st)); fi
    [ "$src" = "hls" ] || line+=" ($src)"
    line+=", "
    frames_sum=$((frames_sum + ${fr:-0})); parsed_sum=$((parsed_sum + ${pa:-0})); relocks_sum=$((relocks_sum + ${rl:-0}))
  done <<< "$sessions"
  health=""
  if [ "$frames_sum" -gt 0 ]; then
    health="; $parsed_sum of $frames_sum frames read ($((parsed_sum * 100 / frames_sum))%), $relocks_sum layout event(s)"
  fi
  say "sessions: ${line%, } ($((total_ms / 3600000))h$(printf '%02d' $((total_ms % 3600000 / 60000)))m on air)$health"
fi

# --- the glyph reader, from the session-close lines of the local day.
lo=$(date -u -d "@$start" +%Y-%m-%dT%H:%M:%S) hi=$(date -u -d "@$end" +%Y-%m-%dT%H:%M:%S)
glyph=$(grep -a 'session #[0-9]* closed (' "$LOG" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' \
        | awk -v lo="$lo" -v hi="$hi" '$1 >= lo && $1 < hi' \
        | grep -oE 'glyph reader [0-9]+ read / [0-9]+ declined' \
        | awk '{r += $3; d += $6; n++} END {if (n) printf "%d read / %d declined (%d session close line%s)", r, d, n, n == 1 ? "" : "s"}')
if [ -n "$glyph" ]; then
  say "glyph reader: $glyph"
elif [ -n "$sessions" ]; then
  say "glyph reader: no session-close line in $LOG for the day (a SIGTERM'd session closes without one)"
fi

# --- VOD imports that landed, from the importer's logs (local stamps), else
# the marker file's mtime.
imports=$(grep -ah "^=== ${day}T[0-9:]*[-+][0-9:]* importing vod " logs/import-when-done*.log 2>/dev/null \
          | awk '{printf "%s (%s), ", $NF, substr($2, 12, 5)}')
if [ -n "$imports" ]; then
  say "imports: ${imports%, }"
elif [ -f backfill-db/imported.txt ] && [ "$(date -d "@$(stat -c %Y backfill-db/imported.txt)" +%F)" = "$day" ]; then
  say "imports: backfill-db/imported.txt updated $(date -d "@$(stat -c %Y backfill-db/imported.txt)" +%H:%M) ($(grep -c . backfill-db/imported.txt) ids), no importer log line for the day"
else
  say "imports: none"
fi

# --- healthcheck alerts of the day.
if [ -f logs/health.log ]; then
  alerts=$(grep -aE "^${day}T[0-9:]*[-+][0-9:]* (ALERT|CLEAR) " logs/health.log | awk '{printf "  %s %s %s", substr($1, 12, 5), $2, $3; for (i = 4; i <= NF; i++) printf " %s", $i; print ""}')
  if [ -n "$alerts" ]; then
    say "healthcheck: $(grep -c ' ALERT ' <<< "$alerts") alert(s)"
    while IFS= read -r l; do say "$l"; done <<< "$alerts"
  else
    say "healthcheck: no alerts"
  fi
else
  say "healthcheck: no alerts (no logs/health.log yet)"
fi

text=$(printf '%s\n' "${out[@]}")
printf '%s\n' "$text"
if [ -n "${NG_ALERT_URL:-}${NG_ALERT_MAIL:-}" ]; then
  printf '%s\n' "$text" | ./scripts/notify.sh "grindlog $day: $attempts attempts, $finished finished${best_ms:+, best $(fmt "$best_ms")}"
fi
exit 0
