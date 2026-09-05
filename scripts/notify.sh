#!/usr/bin/env bash
# Deliver one plain-text message from the monitoring scripts (healthcheck.sh,
# daily-summary.sh) wherever the environment says:
#
#   printf '%s\n' "$body" | scripts/notify.sh "<subject>"
#
#   NG_ALERT_URL   POST the text there with curl, ntfy-style (the subject
#                  travels in the Title header); a URL containing "discord"
#                  gets a JSON {"content": ...} body instead, so a channel
#                  webhook works as is (content is cut at Discord's 2000).
#   NG_ALERT_MAIL  when no URL is set: `mail -s <subject>` to that address.
#   neither        append subject and body to logs/health.log.
#
# Always exits 0 (the callers run from cron, which must not mail on its own);
# a failed POST or mail is appended to logs/health.log with curl's complaint
# instead, so the message is not lost. Set the variables at the top of the
# crontab (crontab.example explains), not here: an ntfy topic or a webhook is
# a secret and this file is tracked.
set -u
cd "$(dirname "$0")/.." || exit 0
subject=${1:-grindlog}
body=$(cat)
[ -n "$body" ] || exit 0
mkdir -p logs
log() { # append the message (with a reason when delivery failed) to health.log
  { printf '%s %s\n' "$(date -Is)" "$1"; printf '%s\n' "$body" | sed 's/^/  /'; } >> logs/health.log
}
if [ -n "${NG_ALERT_URL:-}" ]; then
  case $NG_ALERT_URL in
    *discord*)
      out=$(printf '%s\n%s\n' "$subject" "$body" | jq -Rs '{content: .[:1990]}' \
            | curl -fsS --max-time 20 -H 'Content-Type: application/json' --data-binary @- "$NG_ALERT_URL" 2>&1) ;;
    *)
      out=$(printf '%s\n' "$body" \
            | curl -fsS --max-time 20 -H "Title: $subject" -H 'Content-Type: text/plain; charset=utf-8' \
                   --data-binary @- "$NG_ALERT_URL" 2>&1) ;;
  esac
  rc=$?
  [ "$rc" -eq 0 ] || log "$subject (POST failed, exit $rc: ${out:-no output})"
elif [ -n "${NG_ALERT_MAIL:-}" ]; then
  printf '%s\n' "$body" | mail -s "$subject" "$NG_ALERT_MAIL" || log "$subject (mail failed)"
else
  log "$subject"
fi
exit 0
