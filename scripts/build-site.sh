#!/usr/bin/env bash
# Regenerate the records site from the database:
#   ./scripts/build-site.sh [config.toml]
# Produces site/index.html — a self-contained page (deployable anywhere
# static, e.g. lexicone.com; just copy the one file).
set -euo pipefail
cd "$(dirname "$0")/.."
cfg="${1:-live.toml}"
./target/release/ngtwitchtimer --config "$cfg" report --json > site/data.json

# Refresh reference times from speedrun.com (Ninja Gaiden NES, Any%) so the
# WR / lifetime-PB lines self-update. Falls back to the config values.
SRC_GAME=lde39l63
SRC_CAT=ndx8vvkq
SRC_USER=zx7oyvj7
wr=$(curl -sf --max-time 10 "https://www.speedrun.com/api/v1/leaderboards/${SRC_GAME}/category/${SRC_CAT}?top=1" \
      | jq -r '.data.runs[0].run.times.primary_t // empty' 2>/dev/null || true)
pb=$(curl -sf --max-time 10 "https://www.speedrun.com/api/v1/users/${SRC_USER}/personal-bests" \
      | jq -r ".data[] | select(.run.game==\"${SRC_GAME}\" and .run.category==\"${SRC_CAT}\") | .run.times.primary_t" 2>/dev/null || true)
if [ -n "${wr:-}" ] && [ -n "${pb:-}" ]; then
  jq --argjson wr "$wr" --argjson pb "$pb" \
    '.references = [{label:"WR", ms:($wr*1000|round)}, {label:"Lifetime PB", ms:($pb*1000|round)}]' \
    site/data.json > site/data.json.tmp && mv site/data.json.tmp site/data.json
  echo "references from speedrun.com: WR ${wr}s, lifetime PB ${pb}s"
else
  echo "speedrun.com unavailable; keeping config reference times"
fi
awk '
  /__DATA__/ { while ((getline l < "site/data.json") > 0) print l; next }
  { print }
' site/template.html > site/index.html
echo "wrote site/index.html ($(wc -c < site/index.html) bytes)"
