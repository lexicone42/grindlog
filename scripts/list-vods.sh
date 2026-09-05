#!/usr/bin/env bash
# List a channel's archived VODs (its past broadcasts), newest first, one per
# line, in the form the other backfill scripts take their ids:
#
#   <id>  <date>  <hours>  <title>
#
#   ./scripts/list-vods.sh <channel> [--game <substring>]
#
# The date is the broadcast's start in this machine's local time — the day
# import-vod.sh will replace — and the hours its length; `--game` keeps only
# the titles containing the substring, case-insensitively ("ninja gaiden").
# Asks Twitch's public GraphQL endpoint with the web player's client-id (the
# one src/twitch_hls.rs sends; when Twitch rotates it, the comments there say
# where the current one is, and TWITCH_CLIENT_ID=<id> overrides it here
# without an edit) through curl and jq: 100 VODs a page, following pages while
# Twitch serves them (it keeps archives for 14 to 60 days, so one page normally
# holds them all; a declined next page is reported on stderr and the list
# stays partial). When GQL fails and yt-dlp is installed it falls back to
# `yt-dlp --flat-playlist -J`, which lists the same ids and lengths but knows
# no dates: the date column then reads `?`. Exits 1 with the reason when the
# channel does not exist or neither source answers. Reads nothing local and
# writes nothing.
set -euo pipefail
GQL=https://gql.twitch.tv/gql
CLIENT_ID="${TWITCH_CLIENT_ID:-kimne78kx3ncx6brgo4mv6wki5h1ko}"

usage() { echo "usage: $0 <channel> [--game <substring>]"; }
channel="" game=""
while [ $# -gt 0 ]; do
  case "$1" in
    --game) game="${2:?--game needs a substring}"; shift 2;;
    --game=*) game="${1#--game=}"; shift;;
    -h|--help) usage; exit 0;;
    -*) echo "unknown option $1" >&2; usage >&2; exit 1;;
    *) [ -z "$channel" ] || { echo "one channel only" >&2; usage >&2; exit 1; }; channel="$1"; shift;;
  esac
done
[ -n "$channel" ] || { usage >&2; exit 1; }
# Accept the channel's URL too, and match Twitch's lowercase logins.
channel=${channel#https://}; channel=${channel#http://}; channel=${channel#www.}
channel=${channel#twitch.tv/}; channel=${channel%%/*}; channel=${channel%%\?*}
channel=$(printf '%s' "$channel" | tr 'A-Z' 'a-z')
[[ "$channel" =~ ^[a-z0-9_]+$ ]] || { echo "not a Twitch login: $channel" >&2; exit 1; }

# Formats one JSON array of {id, createdAt (RFC 3339, or null), lengthSeconds,
# title} into the output lines: newest first, the title filter applied.
format() {
  jq -r --arg game "$game" '
    def hours: (. / 360 | round) as $t | "\($t / 10 | floor).\($t % 10)";
    def lpad($n): tostring | if length >= $n then . else (" " * ($n - length)) + . end;
    map(select($game == "" or ((.title // "") | ascii_downcase | contains($game | ascii_downcase))))
    | sort_by(.createdAt // "", (.id | tonumber? // 0)) | reverse | .[]
    | [ .id,
        (if .createdAt then (.createdAt | fromdateiso8601 | strflocaltime("%Y-%m-%d")) else "?         " end),
        ((.lengthSeconds // 0) | hours | lpad(5)),
        ((.title // "") | gsub("[\\t\\n\\r]"; " ")) ]
    | join("  ")'
}

# One GQL page: the channel's archives after <cursor> ("" for the first
# page). Prints the JSON response; fails when the request itself does.
gql_page() {
  local body
  body=$(jq -cn --arg login "$channel" --arg after "$1" '{query:
    ("{ user(login: " + ($login | tojson) + ") { videos(first: 100, type: ARCHIVE, sort: TIME"
     + (if $after == "" then "" else ", after: " + ($after | tojson) end)
     + ") { edges { cursor node { id title createdAt lengthSeconds } } pageInfo { hasNextPage } } } }")}')
  curl -sS -m 30 -H "Client-ID: $CLIENT_ID" -H 'Content-Type: application/json' --data "$body" "$GQL"
}

# GQL first: all pages into one array on stdout. Returns 1 when Twitch's
# answer is unusable (rotated client-id, GQL errors, non-JSON) so the caller
# can fall back, 2 for a channel that does not exist (final: no fallback).
gql_list() {
  local after="" page resp err nodes="[]"
  for ((page = 1; ; page++)); do
    resp=$(gql_page "$after") || { echo "gql: curl failed" >&2; return 1; }
    err=$(jq -r 'if type != "object" then "non-JSON answer"
                 elif .error then "\(.status // "") \(.message // .error)"
                 elif (.errors | length) > 0 then ([.errors[].message] | join("; "))
                 else "" end' <<<"$resp" 2>/dev/null) || err="non-JSON answer"
    if [ -n "$err" ]; then
      if [ "$page" -gt 1 ]; then
        echo "warning: Twitch declined page $page ($err); listing the newest $(jq 'length' <<<"$nodes") only" >&2
        break
      fi
      echo "gql: $err (see src/twitch_hls.rs on the client-id)" >&2; return 1
    fi
    if [ "$(jq -r '.data.user == null' <<<"$resp")" = true ]; then
      echo "no such channel: $channel" >&2; return 2
    fi
    nodes=$(jq -c --argjson acc "$nodes" '$acc + [.data.user.videos.edges[].node]' <<<"$resp")
    [ "$(jq -r '.data.user.videos.pageInfo.hasNextPage' <<<"$resp")" = true ] || break
    after=$(jq -r '[.data.user.videos.edges[].cursor | select(. != null)] | last // ""' <<<"$resp")
    [ -n "$after" ] || { echo "warning: more VODs exist but Twitch gave no cursor; listing the first $(jq 'length' <<<"$nodes")" >&2; break; }
  done
  printf '%s\n' "$nodes"
}

if nodes=$(gql_list); then
  format <<<"$nodes"
  exit 0
else
  [ $? -ne 2 ] || exit 1
fi
command -v yt-dlp >/dev/null || { echo "gql failed and yt-dlp is not installed" >&2; exit 1; }
echo "falling back to yt-dlp (no dates)" >&2
out=$(yt-dlp --flat-playlist -J "https://www.twitch.tv/$channel/videos?filter=archives" 2>/dev/null) \
  || { echo "yt-dlp failed for $channel" >&2; exit 1; }
jq -c '[.entries[]? | {id: (.id | ltrimstr("v")), createdAt: null, lengthSeconds: (.duration // 0), title}]' <<<"$out" \
  | format
