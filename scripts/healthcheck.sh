#!/usr/bin/env bash
# Dead-man check for the live deployment, for a ten-minute cron (see
# crontab.example). Each pass reads fourteen signals:
#
#   database      ninja-gaiden.db opens and answers a query
#   tracker-churn at most two board-tracker rebuilds in the last hour
#   tracker-stall a marathon in force has recorded a completion within the hour
#   tracker-unvouched no row the board shows finished has been unvouched for
#               over ten minutes without being filed (a lost anchor, a wrong total)
#   tracker-total the marathon total has not been refused as beyond the board
#               on five passes (the timer being misread)
#   site-stale    while a session is open, the public page is under 30 min old
#
#   supervisor  the tmux session "ngtimer" exists on the bot's own tmux server
#               (socket "grindlog") and scripts/run-live.sh is alive in it (the
#               server outlives a closed window)
#   clock       the box is within a minute of the public site's HTTP Date
#               (a reboot without a time daemon; deploys fail to sign)
#   bot         exactly one `ngtwitchtimer --config live.toml run` is alive
#   restarts    run-live.sh started the bot at most 3 times in the last 30
#               minutes ("[wrapper] starting bot" lines in logs/live.log;
#               more is a crash loop)
#   capture     while ninja-gaiden.db has an open hls session, obs-live.jsonl
#               has grown within the last 2 minutes
#   polling     inside [stream] active_hours (read off live.toml) with no
#               session open, logs/live.log shows an offline poll within the
#               last 35 minutes: the bot polls every offline_poll_secs (60)
#               there, and a Twitch API breakage logs capture errors instead
#   disk        more than 5 GB free on the repository's filesystem
#   deploy      the last deploy-site.sh entry in logs/deploy.log (cron's
#               redirect for deploy-if-live.sh and the nightly deploy; each
#               entry starts with a "=== <time> deploy start" line), when it
#               started within the last 20 minutes, reached its "live:" line
#               (else the build or an upload failed) and printed no "!!!"
#               line (build-site.sh's "per-day feed not built"). Older
#               entries are the daily summary's business
#
# A failing signal is reported at most once an hour (logs/health-state/<signal>
# remembers the last alert) and once more, as an all-clear, when it recovers.
# Reports are appended to logs/health.log (one line each, ALERT or CLEAR) and,
# when NG_ALERT_URL or NG_ALERT_MAIL is set, sent through scripts/notify.sh
# in one message per pass. Always exits 0: cron must not mail on its own.
#
#   scripts/healthcheck.sh        # quiet unless something changed
#   scripts/healthcheck.sh -v     # also print every signal's state
#
# Reads the database read-only (NG_DB overrides ninja-gaiden.db) and writes
# nothing outside logs/. The log's timestamps are UTC and the wrapper's local;
# both go through `date -d`.
set -uo pipefail
# Whatever goes wrong inside (an unbound variable, a tool missing from cron's
# PATH), the exit status stays 0; the cron line keeps stderr in health.log.
trap 'exit 0' EXIT
cd "$(dirname "$0")/.." || exit 0
verbose=0
[ "${1:-}" = "-v" ] && verbose=1
DB=${NG_DB:-ninja-gaiden.db}
LOG=logs/live.log
OBS=obs-live.jsonl
STATE=logs/health-state
mkdir -p "$STATE"
now=$(date +%s)
alerts=()
clears=()

record() { printf '%s %s\n' "$(date -Is)" "$1" >> logs/health.log; }

# check <signal> <0=ok|1=failing> <detail>
# The state file holds "<failing> <announced> <last_alert_epoch>": an alert
# goes out when the signal fails and the last alert for it is an hour or more
# old (a flapping signal is not a fresh alert every ten minutes); the
# all-clear goes out once, and only for a failure that was announced.
check() {
  local sig=$1 bad=$2 detail=$3 f="$STATE/$1" failing=0 announced=0 last=0
  if [ -f "$f" ]; then
    read -r failing announced last < "$f" || true
    case "$failing$announced$last" in *[!0-9]*|'') failing=0 announced=0 last=0 ;; esac
  fi
  if [ "$bad" -eq 0 ]; then
    [ "$verbose" -eq 1 ] && echo "ok    $sig: $detail"
    if [ "$failing" -eq 1 ] && [ "$announced" -eq 1 ]; then
      clears+=("$sig: $detail")
      record "CLEAR $sig: $detail"
    fi
    echo "0 0 $last" > "$f"
  else
    [ "$verbose" -eq 1 ] && echo "FAIL  $sig: $detail"
    if [ $((now - last)) -ge 3600 ]; then
      announced=1 last=$now
      alerts+=("$sig: $detail")
      record "ALERT $sig: $detail"
    fi
    echo "1 $announced $last" > "$f"
  fi
}

# --- supervisor: the tmux session run-live.sh lives in, on the bot's own
# tmux server (socket "grindlog", scripts/start-supervisor.sh), AND the
# wrapper alive inside it. The server outlives its windows: a session whose
# bot window was closed by hand still answers has-session while nothing
# restarts the bot. A session on the DEFAULT server is an older crontab's:
# still supervised, so not a failure, but it is where `tmux attach` lands.
if tmux -L grindlog has-session -t =ngtimer 2>/dev/null; then
  if pgrep -f 'scripts/run-live\.s[h]' >/dev/null; then
    check supervisor 0 "tmux -L grindlog session ngtimer present, run-live.sh alive"
  else
    check supervisor 1 "tmux -L grindlog session ngtimer has no run-live.sh in it (tmux -L grindlog kill-session -t ngtimer; scripts/start-supervisor.sh)"
  fi
elif tmux has-session -t =ngtimer 2>/dev/null && pgrep -f 'scripts/run-live\.s[h]' >/dev/null; then
  check supervisor 0 "session ngtimer on the DEFAULT tmux server; move it between runs: scripts/start-supervisor.sh"
else
  check supervisor 1 "no supervisor session (scripts/start-supervisor.sh)"
fi

# --- bot: the process itself, not `... live.toml report --json` from the site
# cron. Two of them would fight over the database.
pids=$(pgrep -f 'ngtwitchtimer --config live.toml ru[n]' | tr '\n' ' ' || true)
n=$(printf '%s' "$pids" | wc -w)
if [ "$n" -eq 1 ]; then
  check bot 0 "pid ${pids% }"
elif [ "$n" -eq 0 ]; then
  check bot 1 "no 'ngtwitchtimer --config live.toml run' process"
else
  check bot 1 "$n bot processes (pids ${pids% })"
fi

# --- restarts: wrapper start lines in the last 30 minutes.
starts=0
for t in $(grep -a '^\[wrapper\] starting bot ' "$LOG" 2>/dev/null | tail -30 | awk '{print $NF}'); do
  e=$(date -d "$t" +%s 2>/dev/null) || continue
  [ $((now - e)) -le 1800 ] && starts=$((starts + 1))
done
if [ "$starts" -gt 3 ]; then
  check restarts 1 "bot started $starts times in 30 minutes (crash loop; tail logs/live.log)"
else
  check restarts 0 "$starts start(s) in 30 minutes"
fi

# --- database, capture: the open-session count decides whether obs-live.jsonl
# must be moving. A database that cannot be read is a signal of its own.
open=$(sqlite3 -readonly -cmd '.timeout 5000' "$DB" \
       "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL AND source='hls'" 2>&1)
if [ "$open" -eq "$open" ] 2>/dev/null; then
  check database 0 "$DB readable, $open open hls session(s)"
else
  check database 1 "cannot read $DB: ${open:-no output}"
  open=0
fi
if [ "$open" -gt 0 ]; then
  age=$((now - $(stat -c %Y "$OBS" 2>/dev/null || echo 0)))
  if [ "$age" -le 120 ]; then
    check capture 0 "live session open, $OBS written ${age}s ago"
  else
    check capture 1 "live session open but $OBS last grew $((age / 60)) min ago"
  fi
else
  check capture 0 "no live session open"
fi

# --- polling: inside active_hours with nothing open, the offline poll must
# be recent. HH:MM strings compare as text once zero-padded; a window that
# crosses midnight is start..24:00 plus 00:00..end.
window=$(grep -E '^[[:space:]]*active_hours[[:space:]]*=' live.toml 2>/dev/null | grep -oE '[0-9]{1,2}:[0-9]{2}' | head -2 \
         | awk -F: '{printf "%02d:%02d\n", $1, $2}' | tr '\n' ' ')
read -r ah_start ah_end <<< "${window:-}" || true
hm=$(date +%H:%M)
inside=0
if [ -n "${ah_start:-}" ] && [ -n "${ah_end:-}" ]; then
  if [[ "$ah_start" < "$ah_end" ]]; then
    [[ "$hm" > "$ah_start" || "$hm" == "$ah_start" ]] && [[ "$hm" < "$ah_end" ]] && inside=1
  else
    { [[ "$hm" > "$ah_start" || "$hm" == "$ah_start" ]] || [[ "$hm" < "$ah_end" ]]; } && inside=1
  fi
fi
if [ "$inside" -eq 0 ]; then
  check polling 0 "outside active hours (${ah_start:-?}-${ah_end:-?}, now $hm)"
elif [ "$open" -gt 0 ]; then
  check polling 0 "live session open"
else
  stamp=$(grep -a 'is offline; checking again in' "$LOG" 2>/dev/null | tail -1 \
          | sed 's/\x1b\[[0-9;]*m//g' | grep -oE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z' || true)
  e=$(date -d "${stamp:-1970-01-01T00:00:00Z}" +%s 2>/dev/null || echo 0)
  age=$((now - e))
  if [ "$age" -le 2100 ]; then
    check polling 0 "offline poll $((age / 60)) min ago"
  elif [ "$e" -eq 0 ]; then
    check polling 1 "inside active hours, no offline poll line in $LOG at all"
  else
    check polling 1 "inside active hours, last offline poll $((age / 60)) min ago ($(date -d "@$e" +%H:%M))"
  fi
fi

# --- disk: the repository's filesystem (database, obs log, backups, VODs).
avail_kb=$(df -Pk . 2>/dev/null | awk 'NR==2 {print $4}')
case $avail_kb in
  ''|*[!0-9]*) check disk 1 "cannot read free space (df said '${avail_kb:-nothing}')" ;;
  *)
    gb=$((avail_kb / 1048576))
    if [ "$avail_kb" -gt $((5 * 1048576)) ]; then
      check disk 0 "${gb} GB free"
    else
      check disk 1 "only ${gb} GB free on $(df -P . 2>/dev/null | awk 'NR==2 {print $6}')"
    fi ;;
esac

# --- deploy: the last deploy's entry in logs/deploy.log, from its start
# marker to the end of the file. Within 20 minutes of its start it must have
# reached "live:" (every failure under set -e stops before that line) and
# printed no "!!!" line (the per-day feed not built; the page still shipped).
# A deploy takes well under a minute, so one started over 5 minutes ago
# without a "live:" line died. No marker at all (a log from before the
# markers) is nothing to alert on.
DEPLOY_LOG=logs/deploy.log
entry=$(awk '/^=== .* deploy start$/ {buf=""} {buf=buf $0 "\n"} END {printf "%s", buf}' "$DEPLOY_LOG" 2>/dev/null)
stamp=$(printf '%s' "$entry" | head -1 | sed -n 's/^=== \(.*\) deploy start$/\1/p')
e=$(date -d "${stamp:-1970-01-01T00:00:00Z}" +%s 2>/dev/null || echo 0)
age=$((now - e))
if [ -z "$stamp" ] || [ "$e" -eq 0 ]; then
  check deploy 0 "no marked deploy entry in $DEPLOY_LOG yet"
elif [ "$age" -gt 1200 ]; then
  check deploy 0 "last deploy $((age / 60)) min ago"
elif printf '%s' "$entry" | grep -q '^!!!'; then
  check deploy 1 "deploy $((age / 60)) min ago: $(printf '%s' "$entry" | grep -m1 '^!!!' | cut -c1-200)"
elif printf '%s' "$entry" | grep -qi 'not building\|^error\|command not found\|No such file'; then
  # A deploy that dies in its first seconds leaves neither "!!!" nor "live:",
  # and "started N min ago did not reach live:" below waits five minutes —
  # which two ten-minute crons can straddle every time. 2026-09-17: the
  # report's JSON had a log line in front of it and every deploy for two and
  # a half hours failed at "report JSON is not valid; not building" with no
  # alert. The failure text is the signal.
  check deploy 1 "deploy $((age / 60)) min ago failed: $(printf '%s' "$entry" | grep -mi1 'not building\|^error\|command not found\|No such file' | cut -c1-160)"
elif printf '%s' "$entry" | grep -q '^live: '; then
  check deploy 0 "deploy $((age / 60)) min ago finished"
elif [ "$age" -gt 300 ]; then
  check deploy 1 "deploy started $((age / 60)) min ago did not reach 'live:' (tail $DEPLOY_LOG)"
else
  check deploy 0 "deploy in progress, started ${age}s ago"
fi

# --- board tracker: churn and stall on a marathon board (the race board, an
# Arcathlon). "board replaced" is the tracker rebuilt under the same event;
# once or twice a day is a board really replaced, three in an hour is one
# bad pane read in five rebuilding it and nothing ever recorded (2026-09-17,
# the first live run). A marathon in force whose last completion is over an
# hour old while a session is open is stuck — his longest game is under an
# hour — or he walked away from the run, which is worth a look either way.
# The log carries colour codes; they are stripped before the timestamps are
# compared.
plain() { sed 's/\x1b\[[0-9;]*m//g' logs/live.log 2>/dev/null; }
replaced=$(plain | awk -v since="$(date -u -d "@$((now - 3600))" +%Y-%m-%dT%H:%M:%S)" '$1 >= since && /board replaced/' | wc -l)
if [ "$replaced" -ge 3 ]; then
  check tracker-churn 1 "the board tracker was rebuilt $replaced times in the last hour (logs/live.log 'board replaced')"
else
  check tracker-churn 0 "$replaced rebuild(s) in the last hour"
fi
if [ "$open" -gt 0 ] && command -v jq >/dev/null; then
  state=$(sqlite3 -readonly -cmd '.timeout 5000' "$DB" "select coalesce(events,'[]') from sessions where source='hls' and ended_at_ms is null order by started_at_ms desc limit 1" 2>/dev/null \
    | jq -r '[.[] | select(.k == "marathon") | select(.d | test(" (started|board replaced|ended)$"))] | last | .d // ""' 2>/dev/null)
  case "$state" in
    *" started"|*" board replaced")
      last_row=$(plain | awk '/marathon row/ {t=$1} END {print t}')
      lr=$(date -d "${last_row:-1970-01-01T00:00:00Z}" +%s 2>/dev/null || echo 0)
      since_row=$((now - lr))
      if [ "$since_row" -gt 3600 ]; then
        check tracker-stall 1 "a marathon is in force (${state% started}) and the last completion was $((since_row / 60)) min ago"
      else
        check tracker-stall 0 "marathon in force, last completion $((since_row / 60)) min ago"
      fi;;
    *) check tracker-stall 0 "no marathon in force";;
  esac
fi

# --- board tracker: a row the board shows finished that nothing confirms.
# The tracker says "nothing vouches for it" once a settled, coherent
# cumulative has gone five passes with neither the marathon total nor the
# row above to vouch for it, and "filed after N unvouched passes" when such
# a row is filed after all. A row said more than ten minutes ago and not
# filed since is a lost anchor or a wrong total: a game the site will be
# missing until the day is replayed (docs/big20.md).
lost=$(plain | awk -v since="$(date -u -d "@$((now - 3600))" +%Y-%m-%dT%H:%M:%S)" -v aged="$(date -u -d "@$((now - 600))" +%Y-%m-%dT%H:%M:%S)" '
  $1 >= since && /nothing vouches for it/ && match($0, /marathon row [0-9]+: /) {
    k = substr($0, RSTART + 13, RLENGTH - 15); if ($1 <= aged) said[k] = $0; else delete said[k] }
  $1 >= since && /filed after [0-9]+ unvouched passes/ && match($0, /marathon row [0-9]+: /) {
    delete said[substr($0, RSTART + 13, RLENGTH - 15)] }
  END { for (k in said) { s = said[k]; sub(/.*marathon row [0-9]+: /, "", s); sub(/ on [0-9]+ passes.*/, "", s); printf "%s%s", sep, s; sep = ", " } }')
if [ -n "$lost" ]; then
  check tracker-unvouched 1 "rows the board shows finished, unvouched for over ten minutes: $lost (logs/live.log 'nothing vouches')"
else
  check tracker-unvouched 0 "no row unvouched for over ten minutes"
fi

# --- board tracker: the marathon total refused as beyond anything the board
# shows. The tracker says so at the first refused pass, the fifth and every
# thirtieth; five is the timer being misread (a stray digit, a shifted crop),
# which leaves the board alone to vouch for every row.
refused=$(plain | awk -v since="$(date -u -d "@$((now - 3600))" +%Y-%m-%dT%H:%M:%S)" \
  '$1 >= since && /marathon total .* ignored, [0-9]+ pass/ { n = $0; sub(/.*ignored, /, "", n); sub(/ pass.*/, "", n); if (n + 0 > max) max = n + 0 } END { print max + 0 }')
if [ "$refused" -ge 5 ]; then
  check tracker-total 1 "the marathon total has been refused on $refused passes: the timer is being misread (logs/live.log 'beyond anything the board shows')"
else
  check tracker-total 0 "$refused refused total pass(es)"
fi

# --- site: while a session is open the ten-minute deploy keeps the public
# page fresh; a page over 30 minutes old with the stream up is a deploy that
# has been failing (see deploy above) or a cron that is not running. The
# page's address is the last "live:" line a deploy wrote, so nothing here
# names the site.
site=$(grep '^live: ' "$DEPLOY_LOG" 2>/dev/null | tail -1 | awk '{print $2}')

# --- clock: the box against the public site's HTTP Date. A reboot without a
# time daemon came up five minutes behind; the deploy's uploads went through
# and its CloudFront request was refused as expired ("Signature expired"),
# with nothing naming the cause. AWS allows five minutes; a minute is already
# wrong.
if [ -n "$site" ] && command -v curl >/dev/null; then
  remote=$(curl -sI --max-time 10 "${site%/}/" 2>/dev/null | awk 'tolower($1)=="date:" {sub(/^[^ ]+ /, ""); print}' | tr -d '\r')
  remote_s=$(date -u -d "$remote" +%s 2>/dev/null || echo 0)
  if [ "$remote_s" -gt 0 ]; then
    skew=$(( $(date +%s) - remote_s ))
    if [ "${skew#-}" -gt 60 ]; then
      check clock 1 "the clock is ${skew#-} s $([ "$skew" -lt 0 ] && echo behind || echo ahead) of the site's server (deploys will fail to sign; start chronyd)"
    else
      check clock 0 "within ${skew#-} s of the site's server"
    fi
  fi
fi
if [ "$open" -gt 0 ] && [ -n "$site" ] && command -v jq >/dev/null; then
  gen=$(curl -s --max-time 10 "${site%/}/index.html?hc=$now" -H 'Cache-Control: no-cache' \
    | sed -n '/<script id="data"/,/<\/script>/p' | sed '1d;$d' | jq -r '.generated_at_ms // 0' 2>/dev/null)
  gen=${gen:-0}; gen=${gen%.*}
  if [ "$gen" -gt 0 ] 2>/dev/null; then
    page_age=$(( now - gen / 1000 ))
    if [ "$page_age" -gt 1800 ]; then
      check site-stale 1 "the public page was built $((page_age / 60)) min ago with the stream up"
    else
      check site-stale 0 "public page $((page_age / 60)) min old"
    fi
  fi
fi

if [ ${#alerts[@]} -gt 0 ] || [ ${#clears[@]} -gt 0 ]; then
  body=""
  for a in ${alerts[@]+"${alerts[@]}"}; do body+="ALERT $a"$'\n'; done
  for c in ${clears[@]+"${clears[@]}"}; do body+="all clear: $c"$'\n'; done
  if [ ${#alerts[@]} -gt 0 ]; then
    subject="grindlog: ${#alerts[@]} alert(s) on $(hostname)"
  else
    subject="grindlog: all clear on $(hostname)"
  fi
  [ "$verbose" -eq 1 ] && printf '%s\n%s' "$subject" "$body"
  if [ -n "${NG_ALERT_URL:-}${NG_ALERT_MAIL:-}" ]; then
    printf '%s' "$body" | ./scripts/notify.sh "$subject"
  fi
fi
exit 0
