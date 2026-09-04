#!/usr/bin/env bash
# Install (or refresh) grindlog's crontab lines from scripts/crontab.example,
# leaving every other line of the user's crontab alone. Idempotent: lines
# that mention ngtwitchtimer/scripts, and the "# Grind Log:" comments above
# them, are replaced by the example's; nothing else changes.
#
#   scripts/install-cron.sh            # install
#   scripts/install-cron.sh --show     # print what would be installed
set -euo pipefail
cd "$(dirname "$0")/.."
example=scripts/crontab.example
current=$(crontab -l 2>/dev/null || true)
kept=$(printf '%s\n' "$current" | grep -vE 'ngtwitchtimer/scripts|^# Grind Log:' | sed '/^$/N;/^\n$/D' || true)
# Everything from the first "# Grind Log:" comment on: the comments stay
# with the lines they describe.
ours=$(awk '/^# Grind Log:/{p=1} p' "$example" | grep -vE '^$' || true)
new=$(printf '%s\n\n%s\n' "$kept" "$ours" | sed '/^$/N;/^\n$/D')
if [ "${1:-}" = "--show" ]; then
  printf '%s\n' "$new"
  exit 0
fi
printf '%s\n' "$new" | crontab -
echo "installed $(printf '%s\n' "$ours" | grep -c .) grindlog cron line(s); crontab -l to review"
