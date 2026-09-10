#!/usr/bin/env bash
# What the identity gate (src/identity.rs) decided, per broadcast.
#
#   ./scripts/identity-report.sh [db] [n]
#
# Reads the `identity`, `title`, `suspended` and `resumed` events every
# session records and prints, newest broadcast first: how many distinct
# readings it produced, the boards it saw, and every reading shape with
# what the gate made of it. `n` is how many sessions back to go (default
# 12); `db` defaults to the live database.
#
# The four shapes and what each one means:
#
#   clear       something spoke for the tracked game, nothing against it.
#   convicting  enough signals disagreed to suspend recording.
#   silent      nothing legible said anything; the verdict stands.
#   undecided   something disagreed but not enough to act on. THE ONE TO
#               READ. It is what a board with a generic category and no
#               legible rows or counter looks like — and it is also what
#               his own board looks like when the header misreads. Those
#               two are the same shape and want opposite outcomes, so
#               this is where a rule change has to be argued from.
#
# A session that recorded runs while showing undecided readings is the
# case worth chasing: something was disagreeing the whole time and the
# gate let it through.
set -euo pipefail
cd "$(dirname "$0")/.."

DB=${1:-${LIVE:-ninja-gaiden.db}}
N=${2:-12}
[ -f "$DB" ] || { echo "no database at $DB" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

q() { sqlite3 -cmd '.timeout 10000' "$DB" "$1"; }

# Sessions newest first BY BROADCAST TIME, not by row id. Importing a VOD
# writes a session with a fresh id and a months-old timestamp, so id order
# stopped being time order the day marathons were imported and this listed
# July while the bot was capturing today. Third place that bug appeared.
q "SELECT s.id, COALESCE(date(s.started_at_ms/1000,'unixepoch','localtime'),'?'),
          COALESCE(s.source,'?'), COUNT(r.id),
          COALESCE(s.events,'[]')
     FROM sessions s LEFT JOIN runs r ON r.session_id = s.id
    GROUP BY s.id ORDER BY s.started_at_ms DESC, s.id DESC LIMIT $N;" \
| while IFS='|' read -r id day source runs events; do
    # events is the last field and holds no newlines, but it does hold
    # "|" inside its JSON, so take it as everything after the 4th field.
    events=$(q "SELECT COALESCE(events,'[]') FROM sessions WHERE id = $id;")
    ident=$(printf '%s' "$events" | jq -r '[.[]|select(.k=="identity")]|length' 2>/dev/null || echo 0)
    susp=$(printf '%s' "$events" | jq -r '[.[]|select(.k=="suspended")]|length' 2>/dev/null || echo 0)

    printf '\n=== session #%s  %s  (%s)  %s run(s), %s suspension(s)\n' \
      "$id" "$day" "$source" "$runs" "$susp"
    if [ "${ident:-0}" = 0 ]; then
      echo "    no identity readings (a session from before the gate, or one that never locked a pane)"
      continue
    fi

    # How the session's passes came out, written at close.
    tally=$(printf '%s' "$events" | jq -r 'last(.[]|select(.k=="identity-tally")|.d) // empty' 2>/dev/null)
    [ -n "$tally" ] && echo "    passes: $tally"

    # The boards it saw. Capped, because a twenty-game day is not twenty
    # names: one session produced FIFTY-ONE spellings, forty of them
    # Double Dragon II, and printing them all buried everything else on
    # the line. The count is the useful part — it says how hard the pane
    # was to read — and the `identity` shapes below carry the canonical
    # name of each board anyway.
    nboards=$(printf '%s' "$events" | jq -r '[.[]|select(.k=="title")|.d]|unique|length' 2>/dev/null)
    if [ "${nboards:-0}" -gt 0 ]; then
      boards=$(printf '%s' "$events" | jq -r '[.[]|select(.k=="title")|.d]|unique|.[:6]|join(", ")' 2>/dev/null)
      if [ "$nboards" -gt 6 ]; then
        echo "    boards: $nboards distinct readings, e.g. $boards …"
      else
        echo "    boards: $boards"
      fi
    fi

    # Each distinct reading once. Older sessions logged one per change
    # rather than one per shape, so collapse duplicates either way.
    printf '%s' "$events" | jq -r '.[]|select(.k=="identity")|.d' 2>/dev/null \
      | sort | uniq -c | sed 's/^ */    /'

    # The line worth acting on. NOT "recorded while something disagreed" —
    # a real Ninja Gaiden broadcast does that on every window measured,
    # because the header misreads and the pass goes undecided while the
    # run carries on correctly. What is actually suspect is recording
    # without the board EVER having spoken for the tracked game.
    if [ "$runs" -gt 0 ] && printf '%s' "$events" | jq -e \
         '[.[]|select(.k=="identity")|select(.d|startswith("clear"))]|length == 0' >/dev/null 2>&1; then
      echo "    !!! recorded $runs run(s) and no pass ever spoke FOR the tracked game"
    fi
  done

echo
echo "Session-close lines carry the counts; grep them with:"
echo "  grep 'identity ' logs/live.log | tail"
