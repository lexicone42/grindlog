#!/usr/bin/env bash
# Regenerate the records site from the database:
#   ./scripts/build-site.sh [config.toml]
# Produces site/index.html — a self-contained page (deployable anywhere
# static, e.g. lexicone.com; just copy the one file).
set -euo pipefail
cd "$(dirname "$0")/.."
cfg="${1:-live.toml}"
# Two builds overlap in normal operation (the every-10-minutes live cron and
# an import finishing), and they share these fixed paths. Without a lock one
# truncates site/data.json while the other is splicing it, and the page ships
# with no data at all.
exec 9> .build-site.lock
flock 9
tmp=$(mktemp site/data.json.XXXXXX)
trap 'rm -f "$tmp" "$tmp.tmp"' EXIT
./target/release/ngtwitchtimer --config "$cfg" report --json > "$tmp"

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
  # Merge, don't replace: times read off the streamer's own layout are more
  # current than speedrun.com and must survive this step.
  jq --argjson wr "$wr" --argjson pb "$pb" \
    '.references = ((.references // []) + [{label:"WR", ms:($wr*1000|round)}, {label:"Lifetime PB", ms:($pb*1000|round)}]
       | group_by(.label) | map(.[0]))' \
    "$tmp" > "$tmp.tmp" && mv "$tmp.tmp" "$tmp"
  echo "references from speedrun.com: WR ${wr}s, lifetime PB ${pb}s"
else
  echo "speedrun.com unavailable; keeping the layout/config reference times"
fi
[ -s "$tmp" ] || { echo "report produced no JSON; not building" >&2; exit 1; }
python3 - "$tmp" >/dev/null <<'PY' || { echo "report JSON is not valid; not building" >&2; exit 1; }
import json,sys; json.load(open(sys.argv[1]))
PY
# The JSON is spliced into a <script> element, where the parser ends the
# script at the first "</" regardless of JSON quoting.
sed -i 's|</|<\\/|g' "$tmp"
out=$(mktemp site/index.html.XXXXXX)
awk -v data="$tmp" '
  /__DATA__/ { found = 1
               while ((getline l < data) > 0) { print l; n++ }
               next }
  { print }
  END { if (!found) { print "template has no __DATA__ placeholder" > "/dev/stderr"; exit 1 }
        if (!n)     { print "no data spliced into the page"        > "/dev/stderr"; exit 1 } }
' site/template.html > "$out"
mv "$tmp" site/data.json
mv "$out" site/index.html
echo "wrote site/index.html ($(wc -c < site/index.html) bytes)"
