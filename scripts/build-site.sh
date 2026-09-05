#!/usr/bin/env bash
# Regenerate the records site from the database:
#   ./scripts/build-site.sh [config.toml]
# Produces site/index.html, a self-contained page (copy the one file to any
# static host; ng.lexicone.com is the reference deployment, see
# deploy-site.sh), the machine-readable feed under site/api/v1/ (the full
# report and its projections, then the per-day files behind manifest.json
# that `report --api-dir` writes), and site/data.json, the copy of the report
# the page embeds (trimmed of what the template never reads, see below; kept
# for inspection, not uploaded). First fetches the WR and lifetime-PB
# reference times from speedrun.com (see below for what they can override),
# then checks that the JSON parses with jq. Holds .build-site.lock so the
# live cron and a finishing import cannot build at once. Needs the release
# binary, the config's database, curl, jq and flock.
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

# Fetch reference times from speedrun.com (Ninja Gaiden NES, Any%). They only
# fill a label the report does not already carry: the report's own entries
# (live.toml [game] references, replaced by values read off the streamer's
# layout once it has shown them) come first and win the merge below. With WR
# and Lifetime PB both set in live.toml, speedrun.com's values are never used;
# drop a label from the config to let speedrun.com supply it.
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
jq -e . "$tmp" >/dev/null || { echo "report JSON is not valid; not building" >&2; exit 1; }

# ---- machine-readable data: site/api/v1/ (field reference: site/static/api/v1/README.md)
# Projections of the full report, written compact (CloudFront gzips on the
# wire) and BEFORE the page's diet and the "</" escaping below, which are only
# for the copy spliced into the page. TZ_NAME is the streamer's IANA zone: the
# report carries only today's UTC offset, and the box's /etc/localtime is a
# plain file, so it cannot be derived here — hardcoded like the speedrun.com
# ids.
TZ_NAME=America/Los_Angeles
api=site/api/v1
mkdir -p "$api"
env="{schema_version:1, timezone:\"$TZ_NAME\", docs:\"https://ng.lexicone.com/api/v1/README.md\", generated_at:(.generated_at_ms/1000|floor|todate)}"
jq -c ". + $env" "$tmp" > "$api/report.json.tmp"
jq -c ". + $env | del(.runs, .splits_by_run, .recent_runs) | .sessions |= map(del(.events))" "$tmp" > "$api/summary.json.tmp"
jq -c --arg tz "$TZ_NAME" -f scripts/api-latest.jq "$tmp" > "$api/latest.json.tmp"
for f in report summary latest; do
  jq -e . "$api/$f.json.tmp" >/dev/null || { echo "$api/$f.json is not valid; not building" >&2; exit 1; }
done

# Phase 2, written by the binary itself (src/api.rs): one file per broadcast
# day under days/, history.json, schema.json and, last, manifest.json with
# each file's size and sha256. Built into a scratch directory beside the
# feed and moved into place file by file (same filesystem, so each move is a
# rename; manifest last) only when the binary succeeded and every file it
# lists parses. Otherwise the previous manifest and day files stay as they
# were and the phase-1 site still ships: the per-day feed must never take
# the page down. The "!!!" line is what healthcheck.sh's deploy signal looks
# for in logs/deploy.log.
feed=$(mktemp -d "$api/feed.XXXXXX")
trap 'rm -f "$tmp" "$tmp.tmp" "$api/index.json.tmp2"; rm -rf "$feed"' EXIT
build_feed() {
  ./target/release/ngtwitchtimer --config "$cfg" report --api-dir "$feed" || return 1
  local f
  for f in $(jq -r '.days[].path, .files[].path' "$feed/manifest.json") manifest.json; do
    jq -e . "$feed/$f" >/dev/null 2>&1 || { echo "$f is not valid JSON" >&2; return 1; }
  done
}
if build_feed; then
  mkdir -p "$api/days"
  for f in "$feed"/days/*.json; do [ -e "$f" ] && mv -f "$f" "$api/days/"; done
  mv -f "$feed/history.json" "$feed/schema.json" "$api/"
  mv -f "$feed/manifest.json" "$api/manifest.json"
else
  echo "!!! per-day feed not built: report --api-dir failed; keeping the previous manifest and day files" >&2
fi

jq -n -c --arg tz "$TZ_NAME" --argjson g "$(jq .generated_at_ms "$tmp")" \
  --argjson latest "$(wc -c < "$api/latest.json.tmp")" \
  --argjson summary "$(wc -c < "$api/summary.json.tmp")" \
  --argjson report "$(wc -c < "$api/report.json.tmp")" '{
    schema_version: 1, generated_at_ms: $g, generated_at: ($g/1000|floor|todate), timezone: $tz,
    docs: "https://ng.lexicone.com/api/v1/README.md", llms_txt: "https://ng.lexicone.com/llms.txt",
    files: [
      {path: "/api/v1/latest.json",  purpose: "state of the grind in one small document: today, records, last run, streaks", bytes: $latest,  cache_max_age: 60},
      {path: "/api/v1/summary.json", purpose: "every aggregate the site shows, without per-run rows", bytes: $summary, cache_max_age: 60},
      {path: "/api/v1/report.json",  purpose: "the whole dataset: every run, split and session", bytes: $report, cache_max_age: 60},
      {path: "/api/v1/README.md",    purpose: "field reference", cache_max_age: 3600}
    ]}' > "$api/index.json.tmp"
# The per-day feed's entries, from whatever manifest is in place (this
# build's, or the previous one's when the feed was not rebuilt); none before
# the first successful feed build.
if [ -f "$api/manifest.json" ]; then
  jq -c --argjson manifest "$(wc -c < "$api/manifest.json")" \
    --argjson history "$(jq .files.history.bytes "$api/manifest.json")" \
    --argjson schema "$(jq .files.schema.bytes "$api/manifest.json")" \
    --argjson days "$(jq '.days | length' "$api/manifest.json")" '
    .files = .files[:-1] + [
      {path: "/api/v1/manifest.json", purpose: "per-day feed: lists days/<day>.json with size, sha256 and whether the day is closed; start here to fetch only what changed", bytes: $manifest, cache_max_age: 60, days: $days},
      {path: "/api/v1/history.json",  purpose: "per-day stats and every finish, without the runs", bytes: $history, cache_max_age: 60},
      {path: "/api/v1/schema.json",   purpose: "JSON Schema of manifest.json, days/<day>.json and history.json", bytes: $schema, cache_max_age: 3600}
    ] + .files[-1:]' "$api/index.json.tmp" > "$api/index.json.tmp2" && mv "$api/index.json.tmp2" "$api/index.json.tmp"
fi
for f in report summary latest index; do mv "$api/$f.json.tmp" "$api/$f.json"; done

# ---- the page's copy: only what site/template.html reads
# The full report stays in site/api/v1/report.json; the copy embedded in the
# page loses what the template never looks at. The once-a-minute title reads
# (`events` of kind "title", thousands per season and a sixth of the page)
# go: the template uses events for the day's pitch (kind "geometry") and the
# capture line's tooltip of the last few, which those reads only buried. Every
# run's `game`/`category` go too: `runs` is already filtered to the tracked
# game, and the page names it from `current_game`/`current_category`. Any
# field the template starts reading must be put back here.
jq '.sessions |= map(if .events then .events |= map(select(.k != "title")) else . end)
    | .runs |= map(del(.game, .category))' "$tmp" > "$tmp.tmp" && mv "$tmp.tmp" "$tmp"
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
