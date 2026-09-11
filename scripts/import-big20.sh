#!/usr/bin/env bash
# Import one broadcast's race-practice runs into the live database, from the
# per-VOD database scripts/replay-big20.sh wrote.
#
#   ./scripts/import-big20.sh <vod_id>... [--deploy]
#
# This is NOT import-vod.sh. That one replaces a broadcast DAY, which is right
# for a Ninja Gaiden VOD and wrong here: he practises Big 20 games in the
# afternoon of days he also runs Ninja Gaiden, and replacing the day would
# delete those runs. This import is additive and scoped to its own VOD, in the
# shape import-arcathlon.sh established — and for the same reason, which cost
# 1888 Ninja Gaiden runs to learn the first time.
#
# What lands: one session (source "vod", carrying the VOD id so every run can
# be linked to its moment) and its PRACTICE runs — the ones follow_title =
# "track" recorded under another game. Runs of the tracked game that the same
# replay found are NOT imported and are reported instead: those belong to
# import-vod.sh, which knows how to replace a day of them.
#
# Runs inside one transaction per VOD, so the live bot can keep the database
# open. Re-running is safe and idempotent. `LIVE=copy.db` targets another
# database, which is how to rehearse this, and rehearsing is not optional —
# it is what caught the marathon import that would have deleted a day.
# `--deploy` rebuilds and uploads the site afterwards.
#
# Exit 1 when a per-VOD database is missing or holds no practice runs, 2 when
# its session is still open or the live bot is still capturing that VOD.
set -euo pipefail
cd "$(dirname "$0")/.."

LIVE=${LIVE:-ninja-gaiden.db}
BIG20_OUT=${BIG20_OUT:-big20-db}
# The (game, category) this deployment IS. Every OTHER pair in the per-VOD
# database is practice and gets imported; this pair is the tracked game's own
# runs and is left for import-vod.sh. Read from the config rather than
# hardcoded, so a deployment following a different game needs no edit here.
CFG=${CFG:-live.toml}
tracked_game=$(sed -n '/^\[game\]/,/^\[/p' "$CFG" | sed -n 's/^name *= *"\(.*\)"$/\1/p' | head -1)
tracked_cat=$(sed -n '/^\[game\]/,/^\[/p' "$CFG" | sed -n 's/^category *= *"\(.*\)"$/\1/p' | head -1)
[ -n "$tracked_game" ] && [ -n "$tracked_cat" ] || {
  echo "could not read [game] name/category from $CFG" >&2; exit 1; }

deploy=false ids=()
for a in "$@"; do
  case "$a" in
    --deploy) deploy=true;;
    -*) echo "unknown option $a (usage: $0 <vod_id>... [--deploy])" >&2; exit 1;;
    *) ids+=("$a");;
  esac
done
[ ${#ids[@]} -gt 0 ] || { echo "usage: $0 <vod_id>... [--deploy]" >&2; exit 1; }
[ -f "$LIVE" ] || { echo "no live database at $LIVE" >&2; exit 1; }

q() { sqlite3 -cmd '.timeout 10000' "$1" "$2"; }
esc() { printf '%s' "$1" | sed "s/'/''/g"; }
G=$(esc "$tracked_game"); C=$(esc "$tracked_cat")
# The predicate, written once: a run that is not the tracked game's.
# Unqualified on purpose, so it drops into any query whose only table with a
# `game` column is `runs` — which is every query below, `sessions` having
# neither column. Qualifying it by rewriting the string with sed was the
# first version and would have corrupted itself on a tracked game whose NAME
# contained the word "game".
MINE="NOT (game = '$G' AND category = '$C')"
# What this import stamps on the sessions it writes, and the only thing it
# will ever delete. See the note on the DELETE below for why "a session that
# holds no run of the tracked game" is not good enough.
TAG=${BIG20_TAG:-big20-import}
# How close two runs of the same game must start to be the same attempt.
# Both the live capture and a replay back-date a start from the timer value
# at the run's first frame, so the same attempt agrees to within a frame or
# two; a different attempt of the same game is minutes away.
DUP_MS=${BIG20_DUP_MS:-10000}

echo "tracking $tracked_game [$tracked_cat]; everything else in a pass is practice"

for id in "${ids[@]}"; do
  srcdb="$BIG20_OUT/vod-$id.db"
  [ -f "$srcdb" ] || { echo "!!! $srcdb is missing — replay it first (scripts/replay-big20.sh $id)" >&2; exit 1; }
  open=$(q "$srcdb" "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL")
  [ "$open" = 0 ] || { echo "!!! $srcdb still has an open session; the replay did not finish" >&2; exit 2; }
  runs=$(q "$srcdb" "SELECT COUNT(*) FROM runs WHERE $MINE")
  [ "${runs:-0}" -gt 0 ] || { echo "!!! $srcdb holds no practice runs" >&2; exit 1; }
  theirs=$(q "$srcdb" "SELECT COUNT(*) FROM runs WHERE NOT ($MINE)")

  # Never touch a session that is still capturing. He may be streaming the
  # very broadcast being imported, and the delete below would take the open
  # row with it — stranding the rest of the day on a dangling session_id and
  # silencing healthcheck.sh and deploy-if-live.sh, which both look for an
  # open hls row.
  live=$(q "$LIVE" "SELECT COUNT(*) FROM sessions WHERE vod_id = '$id' AND ended_at_ms IS NULL")
  if [ "${live:-0}" -gt 0 ]; then
    echo "!!! vod $id is still being captured (an open session carries it)" >&2
    echo "    import it once the broadcast has ended" >&2
    exit 2
  fi

  day=$(q "$srcdb" "SELECT date(MIN(started_at_ms)/1000,'unixepoch','localtime') FROM runs WHERE $MINE")
  games=$(q "$srcdb" "SELECT COUNT(DISTINCT game) FROM runs WHERE $MINE")
  had=$(q "$LIVE" "SELECT COUNT(*) FROM runs r JOIN sessions s ON r.session_id = s.id WHERE s.vod_id = '$id' AND s.tag = '$TAG'")
  other=$(q "$LIVE" "SELECT COUNT(*) FROM runs WHERE date(started_at_ms/1000,'unixepoch','localtime') = '$day' AND NOT ($MINE)")
  echo "vod $id ($day): $runs practice run(s) of $games game(s) to import; $had already from this VOD"
  echo "  $theirs $tracked_game run(s) in the pass are NOT imported (that is import-vod.sh's job)"
  echo "  $other $tracked_game run(s) that day are left alone"

  # One transaction: drop what an earlier import of THIS VOD left, then
  # insert its session and its practice runs.
  #
  # "What an earlier import left" is not everything carrying this vod_id: a
  # day he spent on Ninja Gaiden in the morning and practice in the afternoon
  # is ONE broadcast under ONE vod_id, so the Ninja Gaiden backfill's session
  # for that day shares the id this import is scoped to. Ownership is
  # POSITIVE — a session carrying this import's own tag — and not "a session
  # with no evidence against", which takes silence for consent.
  q "$LIVE" "
    ATTACH DATABASE '$(pwd)/$srcdb' AS src;
    BEGIN IMMEDIATE;
    CREATE TEMP TABLE mine AS
      SELECT s.id FROM sessions s
       WHERE s.vod_id = '$id'
         AND s.source = 'vod'
         -- The TAG is the ownership, and it has to be: 'holds no run of the
         -- tracked game' was the first version and it selects an ARCATHLON
         -- session for the same VOD, whose ten completions are not the
         -- tracked game's either. That is the shape of the bug that once
         -- deleted 1888 Ninja Gaiden runs, one table over. A session this
         -- import wrote says so.
         AND s.tag = '$TAG'
         -- Kept as well, though the tag already settles it: a tag is a
         -- string anyone can write, and no run of the tracked game may be
         -- deleted by this script under any circumstances.
         AND NOT EXISTS (SELECT 1 FROM runs r
                          WHERE r.session_id = s.id AND NOT ($MINE));
    DELETE FROM splits WHERE run_id IN
      (SELECT id FROM runs WHERE session_id IN (SELECT id FROM mine));
    DELETE FROM runs WHERE session_id IN (SELECT id FROM mine);
    DELETE FROM sessions WHERE id IN (SELECT id FROM mine);
    DROP TABLE mine;
    INSERT INTO sessions (started_at_ms, ended_at_ms, source, label, tag, frames, parsed,
                          probing, relocks, counter_reads, events, vod_id, vod_created_at_ms)
      SELECT started_at_ms, ended_at_ms, source, label, '$TAG', frames, parsed,
             probing, relocks, counter_reads, events, '$id', vod_created_at_ms
      FROM src.sessions ORDER BY started_at_ms;
    -- The session just written, held aside: the VOD's other sessions carry
    -- the same vod_id, and last_insert_rowid() moves with every run below.
    CREATE TEMP TABLE target AS SELECT last_insert_rowid() AS id;
    -- Attempt numbers are the LIVE database's, counted per (game, category)
    -- over everything already there, so a second practice day of the same
    -- game continues its numbering instead of restarting at 1.
    INSERT INTO runs (game, category, attempt_number, started_at_ms, ended_at_ms, outcome,
                      reset_reason, final_time_ms, last_timer_ms, session_id, ls_attempt)
      SELECT r.game, r.category,
             (SELECT COUNT(*) FROM runs x WHERE x.game = r.game AND x.category = r.category
                AND x.started_at_ms <= r.started_at_ms) + 1,
             r.started_at_ms, r.ended_at_ms, r.outcome, r.reset_reason, r.final_time_ms,
             r.last_timer_ms, (SELECT id FROM target), r.ls_attempt
      FROM src.runs r WHERE $MINE
        -- Skip a run the live capture already has.
        --
        -- A day can be BOTH captured live and replayed afterwards, and today
        -- was: \`track\` was enabled halfway through 2026-09-10, so the
        -- afternoon is in the database from the hls sessions and only the
        -- morning is missing. A replay finds the whole day, and without this
        -- the afternoon would land a second time.
        --
        -- Matched on the game and the start, within \$DUP_MS. Both passes
        -- back-date a run's start from the timer value at its first frame,
        -- so the same attempt gets the same start to within a frame or two
        -- whichever pass saw it; a different attempt of the same game is
        -- minutes away, never seconds.
        --
        -- Additive, like everything else here: what is already recorded
        -- wins and nothing is deleted to make room. That does mean a day
        -- captured across several restarts keeps the fragmented version
        -- rather than the replay's single clean pass. Re-import it properly
        -- by removing the live rows first, deliberately, and not as a side
        -- effect of a backfill.
        AND NOT EXISTS (
          SELECT 1 FROM runs e
           WHERE e.game = r.game
             AND ABS(e.started_at_ms - r.started_at_ms) <= $DUP_MS)
      ORDER BY r.started_at_ms;
    DROP TABLE target;
    COMMIT;
    DETACH DATABASE src;"

  now=$(q "$LIVE" "SELECT COUNT(*) FROM runs r JOIN sessions s ON r.session_id = s.id WHERE s.vod_id = '$id' AND s.tag = '$TAG'")
  still=$(q "$LIVE" "SELECT COUNT(*) FROM runs WHERE date(started_at_ms/1000,'unixepoch','localtime') = '$day' AND NOT ($MINE)")
  echo "  imported: $now practice run(s) from this VOD; $still $tracked_game run(s) that day (was $other)"
  [ "$still" = "$other" ] || { echo "!!! that day lost $tracked_game runs — investigate before importing more" >&2; exit 1; }
done

echo "practice now in the database: $(q "$LIVE" "SELECT COUNT(*) FROM runs WHERE $MINE AND category <> 'Arcathlon'") run(s) across $(q "$LIVE" "SELECT COUNT(DISTINCT game) FROM runs WHERE $MINE AND category <> 'Arcathlon'") game(s)"
if $deploy; then
  echo "--- deploying the site"
  ./scripts/deploy-site.sh
fi
